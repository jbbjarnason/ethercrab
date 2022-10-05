mod frame_header;
mod pdu;
mod pdu_frame;

use crate::{
    command::{Command, CommandCode},
    error::{Error, PduError, PduValidationError},
    pdu_loop::{
        frame_header::FrameHeader,
        pdu::{Pdu, PduFlags},
        pdu_frame::SendableFrame,
    },
    timeout,
    timer_factory::TimerFactory,
    ETHERCAT_ETHERTYPE, MASTER_ADDR,
};
use core::{
    cell::{RefCell, UnsafeCell},
    marker::PhantomData,
    mem::MaybeUninit,
    sync::atomic::{AtomicU8, Ordering},
    task::Waker,
};
use nom::{
    bytes::complete::take,
    combinator::map_res,
    error::context,
    number::complete::{le_u16, u8},
};
use packed_struct::PackedStructSlice;
use smoltcp::wire::EthernetFrame;

pub type PduResponse<T> = (T, u16);

pub trait CheckWorkingCounter<T> {
    fn wkc(self, expected: u16, context: &'static str) -> Result<T, Error>;
}

impl<T> CheckWorkingCounter<T> for PduResponse<T> {
    fn wkc(self, expected: u16, context: &'static str) -> Result<T, Error> {
        if self.1 == expected {
            Ok(self.0)
        } else {
            Err(Error::WorkingCounter {
                expected,
                received: self.1,
                context: Some(context),
            })
        }
    }
}

pub struct PduLoop<const MAX_FRAMES: usize, const MAX_PDU_DATA: usize, TIMEOUT> {
    // TODO: Can we have a single buffer that gives out variable length slices instead of wasting
    // space with lots of potentially huge PDUs?
    // No, at least not with BBQueue; the received data needs to be written back into the grant, but
    // that means the grant lives too long and blocks the sending of any other data from the
    // BBBuffer.
    frames: [UnsafeCell<pdu_frame::Frame<MAX_PDU_DATA>>; MAX_FRAMES],
    /// A waker used to wake up the TX task when a new frame is ready to be sent.
    tx_waker: RefCell<Option<Waker>>,
    /// EtherCAT frame index.
    idx: AtomicU8,
    _timeout: PhantomData<TIMEOUT>,
}

// If we don't impl Send, does this guarantee we can have a PduLoopRef and not invalidate the
// pointer? BBQueue does this.
unsafe impl<const MAX_FRAMES: usize, const MAX_PDU_DATA: usize, TIMEOUT> Sync
    for PduLoop<MAX_FRAMES, MAX_PDU_DATA, TIMEOUT>
{
}

impl<const MAX_FRAMES: usize, const MAX_PDU_DATA: usize, TIMEOUT>
    PduLoop<MAX_FRAMES, MAX_PDU_DATA, TIMEOUT>
where
    TIMEOUT: TimerFactory,
{
    // // TODO: Make this a const fn so we can store the PDU loop in a static. This will let us give
    // // `Client` and other stuff to other threads, without using scoped threads. I'll need to use
    // // MaybeUninit for `frames`. I also need to move all the methods to `PduLoopRef`, similar to how
    // // BBQueue does it, then initialise the maybeuninit on that call. Maybe we can only get one ref,
    // // but allow `Clone` on it?
    // pub fn new() -> Self {
    //     Self {
    //         frames: [(); MAX_FRAMES].map(|_| UnsafeCell::new(pdu_frame::Frame::default())),
    //         tx_waker: RefCell::new(None),
    //         idx: AtomicU8::new(0),
    //         _timeout: PhantomData,
    //     }
    // }

    pub const fn new() -> Self {
        let frames = unsafe { MaybeUninit::uninit().assume_init() };

        Self {
            frames,
            tx_waker: RefCell::new(None),
            idx: AtomicU8::new(0),
            _timeout: PhantomData,
        }
    }

    fn set_send_waker(&self, waker: &Waker) {
        if self.tx_waker.borrow().is_none() {
            self.tx_waker.borrow_mut().replace(waker.clone());
        }
    }

    pub fn send_frames_blocking<F>(&self, waker: &Waker, mut send: F) -> Result<(), ()>
    where
        F: FnMut(&SendableFrame<MAX_PDU_DATA>) -> Result<(), ()>,
    {
        self.frames.iter().try_for_each(|frame| {
            let frame = unsafe { &mut *frame.get() };

            if let Some(ref mut frame) = frame.sendable() {
                frame.mark_sending();

                send(frame)
            } else {
                Ok(())
            }
        })?;

        self.set_send_waker(waker);

        Ok(())
    }

    fn frame(&self, idx: u8) -> Result<&mut pdu_frame::Frame<MAX_PDU_DATA>, Error> {
        let req = self
            .frames
            .get(usize::from(idx))
            .ok_or(PduError::InvalidIndex(idx))?;

        Ok(unsafe { &mut *req.get() })
    }

    pub async fn pdu_tx(
        &self,
        command: Command,
        data: &[u8],
        data_length: u16,
    ) -> Result<Pdu<MAX_PDU_DATA>, Error> {
        let idx = self.idx.fetch_add(1, Ordering::AcqRel) % MAX_FRAMES as u8;

        let frame = self.frame(idx)?;

        let pdu = Pdu::<MAX_PDU_DATA>::new(command, data_length, idx, data)?;

        frame.replace(pdu)?;

        // Tell the packet sender there is data ready to send
        match self.tx_waker.try_borrow() {
            Ok(waker) => {
                if let Some(waker) = &*waker {
                    waker.wake_by_ref()
                }
            }
            Err(_) => warn!("Send waker is already borrowed"),
        }

        // TODO: Configurable timeout
        let timer = core::time::Duration::from_micros(30_000);

        timeout::<TIMEOUT, _, _>(timer, frame).await
    }

    pub fn pdu_rx(&self, ethernet_frame: &[u8]) -> Result<(), Error> {
        let raw_packet = EthernetFrame::new_checked(ethernet_frame)?;

        // Look for EtherCAT packets whilst ignoring broadcast packets sent from self
        if raw_packet.ethertype() != ETHERCAT_ETHERTYPE || raw_packet.src_addr() == MASTER_ADDR {
            return Ok(());
        }

        let i = raw_packet.payload();

        let (i, header) = context("header", FrameHeader::parse)(i)?;

        // Only take as much as the header says we should
        let (_rest, i) = take(header.payload_len())(i)?;

        let (i, command_code) = map_res(u8, CommandCode::try_from)(i)?;
        let (i, index) = u8(i)?;

        let frame = self.frame(index)?;

        let (i, command) = command_code.parse_address(i)?;

        // Check for weird bugs where a slave might return a different command than the one sent for
        // this PDU index.
        if command.code() != frame.pdu().command().code() {
            return Err(Error::Pdu(PduError::Validation(
                PduValidationError::CommandMismatch {
                    sent: command,
                    received: frame.pdu().command(),
                },
            )));
        }

        let (i, flags) = map_res(take(2usize), PduFlags::unpack_from_slice)(i)?;
        let (i, irq) = le_u16(i)?;
        let (i, data) = take(flags.length)(i)?;
        let (i, working_counter) = le_u16(i)?;

        // `_i` should be empty as we `take()`d an exact amount above.
        debug_assert_eq!(i.len(), 0);

        frame.wake_done(flags, irq, data, working_counter)?;

        Ok(())
    }
}
