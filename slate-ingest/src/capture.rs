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
    Gap,
}

pub struct Capturer {
    store: ClickHouseClient,
    /// Un-finalized writes, keyed by slot then pubkey. Ordered by slot so ancestors are easy to
    /// reason about. Within a (slot, pubkey) keep the write with the highest write_version
    /// (end-of-slot state).
    buffer: BTreeMap<u64, HashMap<[u8; 32], AccountWrite>>,
    /// Highest finalized slot committed.
    watermark: u64,
    current_segment_lo: Option<u64>,
}

impl Capturer {
    pub fn new(store: ClickHouseClient) -> Self {
        Self {
            store,
            buffer: BTreeMap::new(),
            watermark: 0,
            current_segment_lo: None,
        }
    }
    
    pub fn from_baseline(store: ClickHouseClient, s_snap: u64) -> Self {
        Self {
            store,
            buffer: BTreeMap::new(),
            watermark: s_snap,
            current_segment_lo: Some(s_snap),
        }
    }

    /// Drive one event through the pipeline.
    pub async fn handle_event(&mut self, event: StreamEvent) -> anyhow::Result<()> {
        match event {
            StreamEvent::Account(write) => {
                let slot_map = self.buffer.entry(write.slot).or_default();
                match slot_map.get(&write.pubkey) {
                    Some(v) => {
                        if write.write_version >= v.write_version {
                            slot_map.insert(write.pubkey, write);
                        }
                    }
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
                        slot_map.into_values().map(|w| to_insert(&w)).collect();
                    if !rows.is_empty() {
                        self.store.insert_accounts(&rows).await?;
                    }
                }
                self.watermark = self.watermark.max(slot);
                let lo = *self.current_segment_lo.get_or_insert(slot);
                self.store.record_coverage(lo, self.watermark).await?;
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
            StreamEvent::Gap => {
                self.current_segment_lo = None;
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
        pubkey: w.pubkey,
        slot: w.slot,
        write_version: w.write_version,
        owner: w.owner,
        lamports: w.lamports,
        executable: w.executable as u8,
        rent_epoch: w.rent_epoch,
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
        Slot {
            slot: 100,
            parent: Some(99),
            status: Confirmed,
        },
        Slot {
            slot: 100,
            parent: Some(99),
            status: Finalized,
        },
        // slot 150: A1 re-written + A2 written; confirmed but NOT finalized -> stay buffered
        Account(write(0xA1, 150, 2, 6)),
        Account(write(0xA2, 150, 3, 9)),
        Slot {
            slot: 150,
            parent: Some(100),
            status: Confirmed,
        },
        // slot 155 on a fork: A3 written, then the slot dies -> A3 must be DROPPED
        Account(write(0xA3, 155, 4, 7)),
        Slot {
            slot: 155,
            parent: Some(150),
            status: Dead,
        },
        // now slot 150 finalizes -> COMMIT A1@150 = 6 and A2@150 = 9
        Slot {
            slot: 150,
            parent: Some(100),
            status: Finalized,
        },
    ]
}

/// A scripted stream with a real capture GAP: the stream drops after slot 150 and doesn't
/// resume until slot 500, so slots 151..499 are never captured. This yields two coverage
/// segments, [100,150] and [500,550], with a hole between them. Uses pubkey 0xB1 to stay
/// clear of mock_stream()'s A1.
pub fn mock_stream_with_gap() -> Vec<StreamEvent> {
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
    use SlotStatus::Finalized;
    use StreamEvent::{Account, Gap, Slot};

    vec![
        // segment 1: slots 100 and 150 captured -> coverage [100, 150]
        Account(write(0xB1, 100, 1, 5)),
        Slot {
            slot: 100,
            parent: Some(99),
            status: Finalized,
        },
        Account(write(0xB1, 150, 2, 6)),
        Slot {
            slot: 150,
            parent: Some(100),
            status: Finalized,
        },
        // stream drops here: slots 151..499 are NEVER seen -> a coverage hole
        Gap,
        // segment 2: stream resumes at 500 -> coverage [500, 550]
        Account(write(0xB1, 500, 3, 8)),
        Slot {
            slot: 500,
            parent: Some(499),
            status: Finalized,
        },
        Account(write(0xB1, 550, 4, 9)),
        Slot {
            slot: 550,
            parent: Some(500),
            status: Finalized,
        },
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
        assert_eq!(
            a1_at_100.lamports, 5,
            "as-of 100 should be the slot-100 version"
        );

        let a1_at_200 = store
            .get_account_info(&pk(0xA1), 200)
            .await
            .unwrap()
            .expect("A1 exists as of slot 200");
        assert_eq!(
            a1_at_200.lamports, 6,
            "as-of 200 should be the slot-150 version"
        );

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

    /// A capture gap must never surface as a silently-stale answer. After a stream that drops
    /// between slots 150 and 500, get_account_info_as_of still returns the best value it has, but
    /// stamps the fidelity: Exact inside a covered segment, Uncertain when the span from the
    /// resolved write up to the query slot straddles the gap. Uses 0xB1 (its own account).
    #[tokio::test]
    async fn gap_makes_post_gap_reads_untrusted() {
        use slate_store::Fidelity;

        let mut cap = Capturer::new(ClickHouseClient::new("http://localhost:8123"));
        cap.run(mock_stream_with_gap()).await.unwrap();

        let store = ClickHouseClient::new("http://localhost:8123");

        // as-of 120: resolves to the slot-100 write, and (100,120] is inside segment [100,150].
        let ans = store.get_account_info_as_of(&pk(0xB1), 120).await.unwrap();
        assert_eq!(
            ans.account.expect("B1 exists as of slot 120").lamports,
            5
        );
        assert_eq!(ans.fidelity, Fidelity::Exact, "120 is inside segment 1");

        // as-of 300: still resolves (to the slot-150 write), but (150,300] straddles the gap.
        let ans = store.get_account_info_as_of(&pk(0xB1), 300).await.unwrap();
        assert_eq!(
            ans.account.expect("B1 still resolves as of slot 300").lamports,
            6,
            "we still return an answer..."
        );
        assert_eq!(
            ans.fidelity,
            Fidelity::Uncertain,
            "...but 300 is after the gap -> uncertain"
        );

        // as-of 520: resolves to the slot-500 write, and (500,520] is inside segment [500,550].
        let ans = store.get_account_info_as_of(&pk(0xB1), 520).await.unwrap();
        assert_eq!(
            ans.account.expect("B1 exists as of slot 520").lamports,
            8
        );
        assert_eq!(ans.fidelity, Fidelity::Exact, "520 is inside segment 2");
    }

    /// The bootstrap seam: a snapshot baseline makes accounts answerable even if they never move
    /// again, and turns a true absence into a provable one. Synthetic baseline at slot 10 (kept
    /// below the other tests' >=100 segments so it's the global coverage floor here), then a mock
    /// stream extends it forward to slot 50. Uses 0xD1/0xD2/0xD3, their own accounts.
    #[tokio::test]
    async fn baseline_makes_untouched_and_absent_accounts_answerable() {
        use slate_store::{AccountUpdateInsert, Fidelity};

        let store = ClickHouseClient::new("http://localhost:8123");

        // Synthetic baseline: the full account set stamped at S_snap = 10 (what the snapshot
        // loader will do next), plus coverage marking slot 10 captured. Stamping every baseline
        // account at S_snap is what lets is_covered treat the baseline as one coherent floor.
        let baseline = |first: u8, lamports: u64| AccountUpdateInsert {
            pubkey: pk(first),
            slot: 10,
            write_version: 0,
            owner: pk(0xC0),
            lamports,
            executable: 0,
            rent_epoch: 0,
            data: Vec::new(),
        };
        store
            .insert_accounts(&[baseline(0xD1, 100), baseline(0xD2, 200)])
            .await
            .unwrap();
        store.record_coverage(10, 10).await.unwrap();

        // Stream forward FROM the baseline: D1 rewritten to 150 at slot 50 extends coverage to one
        // contiguous segment [10, 50]. D2 is never touched again.
        let mut cap =
            Capturer::from_baseline(ClickHouseClient::new("http://localhost:8123"), 10);
        cap.run(vec![
            StreamEvent::Account(AccountWrite {
                pubkey: pk(0xD1),
                owner: pk(0xC0),
                lamports: 150,
                executable: false,
                rent_epoch: 0,
                data: Vec::new(),
                slot: 50,
                write_version: 1,
            }),
            StreamEvent::Slot {
                slot: 50,
                parent: Some(49),
                status: SlotStatus::Finalized,
            },
        ])
        .await
        .unwrap();

        // D2 never moved after the baseline, yet it's answerable, and (10,30] is inside the
        // covered segment -> Exact. This is what the baseline buys us.
        let ans = store.get_account_info_as_of(&pk(0xD2), 30).await.unwrap();
        assert_eq!(
            ans.account.expect("D2 answerable from the baseline").lamports,
            200
        );
        assert_eq!(ans.fidelity, Fidelity::Exact);

        // D1 as-of 40: before the slot-50 update, so the baseline value, Exact.
        let ans = store.get_account_info_as_of(&pk(0xD1), 40).await.unwrap();
        assert_eq!(ans.account.expect("D1 at 40").lamports, 100);
        assert_eq!(ans.fidelity, Fidelity::Exact);

        // D1 as-of 50: the streamed update lands on top of the baseline.
        let ans = store.get_account_info_as_of(&pk(0xD1), 50).await.unwrap();
        assert_eq!(ans.account.expect("D1 at 50").lamports, 150);
        assert_eq!(ans.fidelity, Fidelity::Exact);

        // D3 never existed. With gapless coverage from the baseline floor up to 30, that absence
        // is provable -> None, Exact. This is the seam's whole point for the None case.
        let ans = store.get_account_info_as_of(&pk(0xD3), 30).await.unwrap();
        assert!(ans.account.is_none());
        assert_eq!(
            ans.fidelity,
            Fidelity::Exact,
            "absence inside a gapless baseline is provable"
        );

        // D3 as-of 5: below the baseline floor (10), we have no data there, so we can't vouch.
        let ans = store.get_account_info_as_of(&pk(0xD3), 5).await.unwrap();
        assert!(ans.account.is_none());
        assert_eq!(
            ans.fidelity,
            Fidelity::Uncertain,
            "below the baseline floor -> uncertain"
        );
    }
}
