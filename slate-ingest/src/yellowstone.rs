

use std::collections::HashMap;
use futures::StreamExt; 

use yellowstone_grpc_client::{ClientTlsConfig, GeyserGrpcClient, GeyserStream};
use yellowstone_grpc_proto::geyser::{
    CommitmentLevel, SlotStatus as ProtoSlotStatus, SubscribeRequest,
    SubscribeRequestFilterAccounts, SubscribeRequestFilterSlots, SubscribeUpdate,
    SubscribeUpdateAccount, SubscribeUpdateSlot, subscribe_update::UpdateOneof,
};

use crate::capture::{AccountWrite, Capturer, SlotStatus, StreamEvent};

pub struct IngestConfig {
    pub endpoint: String,
    pub x_token: Option<String>,
    pub owners: Vec<String>,
    // pub commitment: Commitmewnt
}

/// Map one Yellowstone update to a `StreamEvent`, or `None` to skip it. `None` covers keepalives
/// (Ping/Pong), streams we didn't subscribe to (Transaction/Block/Entry), unmapped slot statuses,
/// and malformed messages — the driver just ignores those.
pub fn adapt(update: SubscribeUpdate) -> Option<StreamEvent> {
    match update.update_oneof? {
        UpdateOneof::Account(a) => adapt_account(a),
        UpdateOneof::Slot(s) => adapt_slot(s),
        _ => None,
    }
}

fn adapt_account(a: SubscribeUpdateAccount) -> Option<StreamEvent> {
    let info = a.account?;
    let account = AccountWrite {
        pubkey: info.pubkey.try_into().ok()?,
        owner: info.owner.try_into().ok()?,
        lamports: info.lamports,
        executable: info.executable,
        rent_epoch: info.rent_epoch,
        write_version: info.write_version,
        data: info.data,
        slot: a.slot,
    };
    Some(StreamEvent::Account(account))
}

fn adapt_slot(s: SubscribeUpdateSlot) -> Option<StreamEvent> {
    let status = map_status(s.status)?;
    Some(StreamEvent::Slot {
        slot: s.slot,
        parent: s.parent,
        status,
    })
}

/// Yellowstone `SlotStatus` (an i32 on the wire) -> our `SlotStatus`. Only the four with pipeline
/// meaning map; `SlotFirstShredReceived` / `SlotCompleted` / `SlotCreatedBank` have no counterpart,
/// so they return `None` and the whole slot update is skipped.
fn map_status(raw: i32) -> Option<SlotStatus> {
    match ProtoSlotStatus::try_from(raw).ok()? {
        ProtoSlotStatus::SlotFinalized => Some(SlotStatus::Finalized),
        ProtoSlotStatus::SlotProcessed => Some(SlotStatus::Processed),
        ProtoSlotStatus::SlotDead => Some(SlotStatus::Dead),
        ProtoSlotStatus::SlotConfirmed => Some(SlotStatus::Confirmed),
        _ => None,
    }
}

pub async fn connect_and_subscribe(cfg: &IngestConfig) -> anyhow::Result<GeyserStream> {
    let sub_request = SubscribeRequest {
        accounts: HashMap::from([(
            "accounts".into(),
            SubscribeRequestFilterAccounts {
                owner: cfg.owners.clone(), 
                ..Default::default()
            },
        )]),
        slots: HashMap::from([(
            "slots".into(),
            SubscribeRequestFilterSlots {
                filter_by_commitment: Some(false),
                ..Default::default()
            },
        )]),
        commitment: Some(CommitmentLevel::Processed as i32),
        ..Default::default()
    };

    let mut builder =
        GeyserGrpcClient::build_from_shared(cfg.endpoint.clone())?.x_token(cfg.x_token.clone())?;
    if cfg.endpoint.starts_with("https") {
        builder = builder.tls_config(ClientTlsConfig::new().with_native_roots())?;
    }
    let mut client = builder.connect().await?;
    let stream = client.subscribe_once(sub_request).await?;
    Ok(stream)
}

pub async fn run(cfg: &IngestConfig, capturer: &mut Capturer) -> anyhow::Result<()> {
    let mut stream = connect_and_subscribe(cfg).await?;
    while let Some(update) = stream.next().await {
        if let Some(event) = adapt(update?) {
            capturer.handle_event(event).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use yellowstone_grpc_proto::geyser::{SubscribeUpdateAccountInfo, SubscribeUpdatePing};

    /// Wrap an UpdateOneof into a SubscribeUpdate (the rest of the fields default).
    fn update(oneof: UpdateOneof) -> SubscribeUpdate {
        SubscribeUpdate {
            update_oneof: Some(oneof),
            ..Default::default()
        }
    }

    #[test]
    fn account_update_maps_to_account_write() {
        let u = update(UpdateOneof::Account(SubscribeUpdateAccount {
            account: Some(SubscribeUpdateAccountInfo {
                pubkey: vec![0x11; 32],
                owner: vec![0x22; 32],
                lamports: 5,
                executable: false,
                rent_epoch: 7,
                data: b"hi".to_vec(),
                write_version: 9,
                ..Default::default()
            }),
            slot: 100, // slot lives on the OUTER wrapper, not the info
            ..Default::default()
        }));

        assert_eq!(
            adapt(u),
            Some(StreamEvent::Account(AccountWrite {
                pubkey: [0x11; 32],
                owner: [0x22; 32],
                lamports: 5,
                executable: false,
                rent_epoch: 7,
                data: b"hi".to_vec(),
                slot: 100,
                write_version: 9,
            }))
        );
    }

    #[test]
    fn finalized_slot_maps_to_finalized() {
        let u = update(UpdateOneof::Slot(SubscribeUpdateSlot {
            slot: 150,
            parent: Some(149),
            status: ProtoSlotStatus::SlotFinalized as i32,
            ..Default::default()
        }));

        assert_eq!(
            adapt(u),
            Some(StreamEvent::Slot {
                slot: 150,
                parent: Some(149),
                status: SlotStatus::Finalized,
            })
        );
    }

    #[test]
    fn dead_slot_maps_to_dead() {
        let u = update(UpdateOneof::Slot(SubscribeUpdateSlot {
            slot: 155,
            parent: Some(150),
            status: ProtoSlotStatus::SlotDead as i32,
            ..Default::default()
        }));
        assert_eq!(
            adapt(u),
            Some(StreamEvent::Slot {
                slot: 155,
                parent: Some(150),
                status: SlotStatus::Dead,
            })
        );
    }

    #[test]
    fn unmapped_slot_status_is_skipped() {
        // SlotCompleted has no pipeline counterpart -> the whole update is dropped.
        let u = update(UpdateOneof::Slot(SubscribeUpdateSlot {
            slot: 200,
            status: ProtoSlotStatus::SlotCompleted as i32,
            ..Default::default()
        }));
        assert_eq!(adapt(u), None);
    }

    #[test]
    fn ping_is_skipped() {
        assert_eq!(
            adapt(update(UpdateOneof::Ping(SubscribeUpdatePing::default()))),
            None
        );
    }

    #[test]
    fn malformed_pubkey_is_skipped() {
        // A non-32-byte pubkey fails the try_into and is skipped, not panicked on.
        let u = update(UpdateOneof::Account(SubscribeUpdateAccount {
            account: Some(SubscribeUpdateAccountInfo {
                pubkey: vec![0x11; 5],
                owner: vec![0x22; 32],
                ..Default::default()
            }),
            slot: 100,
            ..Default::default()
        }));
        assert_eq!(adapt(u), None);
    }
}
