//! An experiment in how to safely represent and use the PDI (Process Data Image).
//!
//! At time of writing, requires EL2004, EL2004 and EL1004 in that order to function correctly due
//! to a pile of hard-coding.

use async_ctrlc::CtrlC;
use ethercrab::coe::abort_code::AbortCode;
use ethercrab::coe::SdoAccess;
use ethercrab::eeprom::types::SyncManagerType;
use ethercrab::error::Error;
use ethercrab::std::tx_rx_task;
use ethercrab::Client;
use ethercrab::PduLoop;
use ethercrab::SlaveGroup;
use ethercrab::SlaveState;
use futures_lite::FutureExt;
use num_enum::FromPrimitive;
use smol::LocalExecutor;
use std::sync::Arc;
use std::time::Duration;

#[cfg(target_os = "windows")]
// ASRock NIC
// const INTERFACE: &str = "TODO";
// // USB NIC
// const INTERFACE: &str = "\\Device\\NPF_{DCEDC919-0A20-47A2-9788-FC57D0169EDB}";
// Lenovo USB-C NIC
const INTERFACE: &str = "\\Device\\NPF_{CC0908D5-3CB8-46D6-B8A2-575D0578008D}";
// Silver USB NIC
// const INTERFACE: &str = "\\Device\\NPF_{CC0908D5-3CB8-46D6-B8A2-575D0578008D}";
#[cfg(not(target_os = "windows"))]
const INTERFACE: &str = "eth1";

const MAX_SLAVES: usize = 16;
const MAX_PDU_DATA: usize = 1100;
const MAX_FRAMES: usize = 16;
const PDI_LEN: usize = 16;

static PDU_LOOP: PduLoop<MAX_FRAMES, MAX_PDU_DATA, smol::Timer> = PduLoop::new();

async fn main_inner(ex: &LocalExecutor<'static>) -> Result<(), Error> {
    log::info!("Starting SDO demo...");

    let client = Arc::new(Client::<MAX_FRAMES, MAX_PDU_DATA, smol::Timer>::new(
        &PDU_LOOP,
    ));

    ex.spawn(tx_rx_task(INTERFACE, &client).unwrap()).detach();

    // let num_slaves = client.num_slaves();

    let groups = [SlaveGroup::<MAX_SLAVES, PDI_LEN, MAX_FRAMES, MAX_PDU_DATA, _>::new(Box::new(
        |slave| {
            Box::pin(async {
                // --- Reads ---

                // // Name
                // dbg!(slave
                //     .read_sdo::<heapless::String<64>>(0x1008, SdoAccess::Index(0))
                //     .await
                //     .unwrap());

                // // Software version. For AKD, this should equal "M_01-20-00-003"
                // dbg!(slave
                //     .read_sdo::<heapless::String<64>>(0x100a, SdoAccess::Index(0))
                //     .await
                //     .unwrap());

                // --- Writes ---

                slave.write_sdo(0x1c12, 0u8, SdoAccess::Index(0)).await?;
                slave
                    .write_sdo(0x1c12, 0x1720u16, SdoAccess::Index(1))
                    .await?;
                slave.write_sdo(0x1c12, 0x01u8, SdoAccess::Index(0)).await?;

                // slave.write_sdo(0x1c13, 0u8, SdoAccess::Index(0)).await?;
                // slave
                //     .write_sdo(0x1c13, 0x1B22u16, SdoAccess::Index(1))
                //     .await?;
                // slave.write_sdo(0x1c13, 0x01u8, SdoAccess::Index(0)).await?;

                // ---

                let mut start = 0x1c12;
                let num_sms = slave.read_sdo::<u8>(0x1c00, SdoAccess::Index(0)).await?;

                for i in 1..=num_sms {
                    // Skip over SM0/SM1, used for mailbox write/read
                    let sm_idx = i + 2;

                    let sm_type = slave
                        .read_sdo::<u8>(0x1c00, SdoAccess::Index(sm_idx))
                        .await
                        .map(|raw| SyncManagerType::from_primitive(raw))?;

                    let sub_indices = slave.read_sdo::<u8>(start, SdoAccess::Index(0)).await?;

                    log::info!("SDO {start:#06x} {sm_type:?}, sub indices: {sub_indices}");

                    for i in 1..=sub_indices {
                        let pdo = slave.read_sdo::<u16>(start, SdoAccess::Index(i)).await?;
                        let num_mappings = slave.read_sdo::<u8>(pdo, SdoAccess::Index(0)).await?;

                        log::info!("--> #{i} data: {pdo:#06x} ({num_mappings} mappings):");

                        for i in 1..=num_mappings {
                            let mapping = slave.read_sdo::<u32>(pdo, SdoAccess::Index(i)).await?;

                            // Yes, big-endian
                            let parts = mapping.to_be_bytes();

                            let index = u16::from_le_bytes(parts[0..=1].try_into().unwrap());
                            let sub_index = parts[2];
                            let bit_len = parts[3];

                            log::info!("----> index {index:#06x}, sub index {sub_index}, bit length {bit_len}");
                        }
                    }

                    start += 1;
                }

                panic!("Nope");

                Ok(())
            })
        },
    )); 1];

    let mut groups = client
        .init(groups, |groups, slave| {
            // All slaves MUST end up in a group or they'll remain uninitialised
            groups[0].push(slave).expect("Too many slaves");

            // TODO: Return a group key so the user has to put the slave somewhere
        })
        .await
        .expect("Init");

    // let _slaves = &_groups[0].slaves();
    let group = groups.get_mut(0).expect("No group!");

    // log::info!("Discovered {num_slaves} slaves");

    // NOTE: Valid outputs must be provided before moving into operational state
    log::debug!("Moving slaves to OP...");

    client
        .request_slave_state(SlaveState::Op)
        .await
        .expect("OP");

    log::info!("Slaves moved to OP state");

    async_io::Timer::after(Duration::from_millis(100)).await;

    log::info!("Group has {} slaves", group.slaves().len());

    Ok(())
}

fn main() -> Result<(), Error> {
    env_logger::init();
    let local_ex = LocalExecutor::new();

    let ctrlc = CtrlC::new().expect("cannot create Ctrl+C handler?");

    futures_lite::future::block_on(
        local_ex.run(ctrlc.race(async { main_inner(&local_ex).await.unwrap() })),
    );

    Ok(())
}
