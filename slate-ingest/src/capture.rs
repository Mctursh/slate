use std::collections::{BTreeMap, HashMap};

use slate_store::{AccountUpdateInsert, ClickHouseClient};
// (You'll add `AccountUpdateInsert` to this import when you write the commit mapper.)

/// One account write off the stream.
#[derive(Clone)]
pub struct AccountWrite {
    pub pubkey: [u8; 32],
    pub owner: [u8; 32],
    pub lamports: u64,
    pub executable: bool,
    pub rent_epoch: u64,
    pub data: Vec<u8>,
    pub slot: u64,
    pub write_version: u64,
}

/// Slot lifecycle. We only commit on `Finalized`; `Dead` = abandoned fork.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SlotStatus {
    Processed,
    Confirmed,
    Finalized,
    Dead,
}

/// A single event off the stream.
pub enum StreamEvent {
    Account(AccountWrite),
    Slot {
        slot: u64,
        parent: Option<u64>,
        status: SlotStatus,
    },
}

pub struct Capturer {
    store: ClickHouseClient,
    /// Un-finalized writes, keyed by slot then pubkey. Ordered by slot so ancestors are easy to
    /// reason about. Within a (slot, pubkey) keep the write with the highest write_version
    /// (end-of-slot state).
    buffer: BTreeMap<u64, HashMap<[u8; 32], AccountWrite>>,
    /// Highest finalized slot committed.
    watermark: u64,
}

impl Capturer {
    pub fn new(store: ClickHouseClient) -> Self {
        Self {
            store,
            buffer: BTreeMap::new(),
            watermark: 0,
        }
    }

    /// Drive one event through the pipeline.
    pub async fn handle_event(&mut self, event: StreamEvent) -> anyhow::Result<()> {
        match event {
            StreamEvent::Account(write) => {
                let slot_map = self.buffer.entry(write.slot).or_default();
                match slot_map.get(&write.pubkey) {
                    Some(v) => {
                        if write.write_version >= v.write_version{
                            slot_map.insert(write.pubkey, write);
                        }
                    },
                    _ => {
                        slot_map.insert(write.pubkey, write);
                    }
                }
            }
            StreamEvent::Slot {
                slot,
                status: SlotStatus::Finalized,
                ..
            } => {
                if let Some(slot_map) = self.buffer.remove(&slot) {
                let rows: Vec<AccountUpdateInsert> =
                        slot_map.into_values()
                        .map(|w| to_insert(&w))
                        .collect();
                if !rows.is_empty() {
                    self.store.insert_accounts(&rows).await?;
                }
                }
                self.watermark = self.watermark.max(slot);


            }
            StreamEvent::Slot {
                slot,
                status: SlotStatus::Dead,
                ..
            } => {
                // TODO(you): abandoned fork — drop this slot's buffered writes, commit nothing.
                let _ = slot;
                self.buffer.remove(&slot);
            }
            StreamEvent::Slot { .. } => {
                // Processed / Confirmed: nothing to do yet — we only commit on Finalized.
            }
        }
        Ok(())
    }

    /// Convenience for the mock/tests: drive a whole scripted sequence.
    pub async fn run(&mut self, events: Vec<StreamEvent>) -> anyhow::Result<()> {
        for event in events {
            self.handle_event(event).await?;
        }
        Ok(())
    }
}

fn to_insert(w: &AccountWrite) -> slate_store::AccountUpdateInsert {
    slate_store::AccountUpdateInsert {
        pubkey: w.pubkey, slot: w.slot, write_version: w.write_version, owner: w.owner,
        lamports: w.lamports, executable: w.executable as u8, rent_epoch: w.rent_epoch,
        data: w.data.clone(),
    }
}

/// A scripted stream exercising the core behaviour: buffer-until-finalize, commit on finalize,
/// drop on a dead fork.
pub fn mock_stream() -> Vec<StreamEvent> {
    fn key(first: u8) -> [u8; 32] {
        let mut a = [0u8; 32];
        a[0] = first;
        a
    }
    fn write(first: u8, slot: u64, wv: u64, lamports: u64) -> AccountWrite {
        AccountWrite {
            pubkey: key(first),
            owner: key(0xC0), // some program
            lamports,
            executable: false,
            rent_epoch: 0,
            data: Vec::new(),
            slot,
            write_version: wv,
        }
    }
    use SlotStatus::*;
    use StreamEvent::{Account, Slot};

    vec![
        // slot 100: A1 written, then finalized -> should COMMIT A1@100 = 5
        Account(write(0xA1, 100, 1, 5)),
        Slot { slot: 100, parent: Some(99), status: Confirmed },
        Slot { slot: 100, parent: Some(99), status: Finalized },
        // slot 150: A1 re-written + A2 written; confirmed but NOT finalized -> stay buffered
        Account(write(0xA1, 150, 2, 6)),
        Account(write(0xA2, 150, 3, 9)),
        Slot { slot: 150, parent: Some(100), status: Confirmed },
        // slot 155 on a fork: A3 written, then the slot dies -> A3 must be DROPPED
        Account(write(0xA3, 155, 4, 7)),
        Slot { slot: 155, parent: Some(150), status: Dead },
        // now slot 150 finalizes -> COMMIT A1@150 = 6 and A2@150 = 9
        Slot { slot: 150, parent: Some(100), status: Finalized },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Same key convention as the store tests: first byte set, rest zero.
    fn pk(first: u8) -> [u8; 32] {
        let mut a = [0u8; 32];
        a[0] = first;
        a
    }

    /// Drive the scripted stream, then check what actually landed in the store.
    /// Needs ClickHouse up. Idempotent: ReplacingMergeTree collapses re-runs, and the
    /// pubkeys (0xA1/0xA2/0xA3) don't collide with the store's X/Y/Z fixtures.
    #[tokio::test]
    async fn commits_on_finalize_and_drops_dead_fork() {
        let mut cap = Capturer::new(ClickHouseClient::new("http://localhost:8123"));
        cap.run(mock_stream()).await.unwrap();

        let store = ClickHouseClient::new("http://localhost:8123");

        // A1 finalized twice: at 100 (lamports 5) then at 150 (lamports 6).
        let a1_at_100 = store
            .get_account_info(&pk(0xA1), 100)
            .await
            .unwrap()
            .expect("A1 exists as of slot 100");
        assert_eq!(a1_at_100.lamports, 5, "as-of 100 should be the slot-100 version");

        let a1_at_200 = store
            .get_account_info(&pk(0xA1), 200)
            .await
            .unwrap()
            .expect("A1 exists as of slot 200");
        assert_eq!(a1_at_200.lamports, 6, "as-of 200 should be the slot-150 version");

        // A2 finalized at 150.
        let a2_at_200 = store
            .get_account_info(&pk(0xA2), 200)
            .await
            .unwrap()
            .expect("A2 exists as of slot 200");
        assert_eq!(a2_at_200.lamports, 9);

        // A3 only ever lived on slot 155, which died -> never committed.
        assert!(
            store
                .get_account_info(&pk(0xA3), 200)
                .await
                .unwrap()
                .is_none(),
            "A3's slot died, so it must never have been committed"
        );
    }
}
