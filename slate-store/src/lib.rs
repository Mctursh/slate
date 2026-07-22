use clickhouse::Row;
use serde::{Deserialize, Serialize};

pub type StoreResult<T> = Result<T, clickhouse::error::Error>;

#[derive(Deserialize, Row)]
pub struct AccountUpdate {
    pub pubkey: [u8; 32],
    pub owner: [u8; 32],
    pub slot: u64,
    pub lamports: u64,
    pub write_version: u64,
    pub rent_epoch: u64,
    pub executable: u8,
    #[serde(with = "serde_bytes")]
    pub data: Vec<u8>,
}
#[derive(Serialize, Row)]
pub struct AccountUpdateInsert {
    pub pubkey: [u8; 32],
    pub slot: u64,
    pub write_version: u64,
    pub owner: [u8; 32],
    pub lamports: u64,
    pub executable: u8,
    pub rent_epoch: u64,
    #[serde(with = "serde_bytes")]
    pub data: Vec<u8>,
}

// #[derive(Serialize, Row)]
// pub struct CoverageRow {
//     segment_lo: u64,
//     segment_hi: u64
// }
#[derive(Debug, PartialEq, Serialize)]
pub enum Fidelity {
    Exact,
    Uncertain,
}

pub struct AccountAtSlot {
    pub account: Option<AccountUpdate>,
    pub fidelity: Fidelity,
}

pub struct ClickHouseClient {
    client: clickhouse::Client,
}

impl ClickHouseClient {
    pub fn new(url: &str) -> Self {
        let client = clickhouse::Client::default()
            .with_url(url)
            .with_database("slate")
            .with_user("slate")
            .with_password("slate");
        ClickHouseClient { client }
    }

    pub async fn get_account_info(
        &self,
        pubkey: &[u8; 32],
        as_of_slot: u64,
    ) -> StoreResult<Option<AccountUpdate>> {
        let query = "SELECT pubkey, owner, slot, lamports, write_version, rent_epoch, executable, data FROM slate.account_updates WHERE pubkey = unhex(?) AND slot <= ? ORDER BY slot DESC, write_version DESC LIMIT 1";
        let row = self
            .client
            .query(query)
            .bind(hex::encode(pubkey))
            .bind(as_of_slot)
            .fetch_optional::<AccountUpdate>()
            .await?;

        Ok(row)
    }

    pub async fn get_program_accounts(
        &self,
        owner: &[u8; 32],
        as_of_slot: u64,
    ) -> StoreResult<Vec<AccountUpdate>> {
        let query = "SELECT pubkey, owner, as_of_slot AS slot, lamports, as_of_write_version AS write_version, rent_epoch, executable, data
                    FROM (
                        SELECT
                            pubkey,
                            argMax(owner,         (slot, write_version)) AS owner,
                            argMax(slot,          (slot, write_version)) AS as_of_slot,
                            argMax(lamports,      (slot, write_version)) AS lamports,
                            argMax(write_version, (slot, write_version)) AS as_of_write_version,
                            argMax(rent_epoch,    (slot, write_version)) AS rent_epoch,
                            argMax(executable,    (slot, write_version)) AS executable,
                            argMax(data,          (slot, write_version)) AS data
                        FROM slate.account_updates
                        WHERE pubkey IN (
                            SELECT DISTINCT pubkey FROM slate.account_updates_by_owner
                            WHERE owner = unhex(?) AND slot <= ?
                        ) AND slot <= ?
                        GROUP BY pubkey
                    )
                    WHERE owner = unhex(?) AND lamports > 0
            ";

        let rows = self
            .client
            .query(query)
            .bind(hex::encode(owner))
            .bind(as_of_slot)
            .bind(as_of_slot)
            .bind(hex::encode(owner))
            .fetch_all::<AccountUpdate>()
            .await?;
        Ok(rows)
    }

    pub async fn insert_accounts(&self, rows: &[AccountUpdateInsert]) -> StoreResult<()> {
        let mut insert = self
            .client
            .insert::<AccountUpdateInsert>("account_updates")
            .await?;

        for row in rows {
            insert.write(row).await?;
        }
        insert.end().await?;
        Ok(())
    }

    pub async fn record_coverage(&self, lo: u64, hi: u64) -> StoreResult<()> {
        self.client
            .query("INSERT INTO coverage  VALUES (?, ?)")
            .bind(lo)
            .bind(hi)
            .execute()
            .await?;
        Ok(())
    }

    pub async fn is_covered(&self, from: u64, to: u64) -> StoreResult<bool> {
        let query = "
            SELECT count() > 0 FROM (
                SELECT segment_lo, max(segment_hi) AS hi
                FROM slate.coverage
                GROUP BY segment_lo
            )
            WHERE segment_lo <= ? AND hi >= ?
        ";
        let covered = self
            .client
            .query(query)
            .bind(from)
            .bind(to)
            .fetch_one::<u8>()
            .await?;

        Ok(covered != 0)
    }

    pub async fn get_account_info_as_of(
        &self,
        pubkey: &[u8; 32],
        as_of_slot: u64,
    ) -> StoreResult<AccountAtSlot> {
        let account = self.get_account_info(pubkey, as_of_slot).await?;
        let fidelity = match &account {
            Some(a) => {
                if self.is_covered(a.slot, as_of_slot).await? {
                    Fidelity::Exact
                } else {
                    Fidelity::Uncertain
                }
            }
            None =>
            /* Decision 1 goes here */
            {
                Fidelity::Uncertain
            }
        };
        Ok(AccountAtSlot { account, fidelity })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 32-byte pubkey whose first byte is `first` and the rest zeros,
    /// matching the seed data (e.g. unhex('11' || repeat('00',31))).
    fn pk(first: u8) -> [u8; 32] {
        let mut a = [0u8; 32];
        a[0] = first;
        a
    }

    fn store() -> ClickHouseClient {
        ClickHouseClient::new("http://localhost:8123")
    }

    /// Seed the canonical X/Y/Z dataset through the real insert path so the tests are
    /// self-contained rather than depending on manually-seeded rows. Idempotent:
    /// ReplacingMergeTree collapses identical rows, so calling it every test is safe.
    async fn seed_test_accounts() {
        fn row(
            pubkey: [u8; 32],
            slot: u64,
            wv: u64,
            owner: [u8; 32],
            lamports: u64,
            data: &[u8],
        ) -> AccountUpdateInsert {
            AccountUpdateInsert {
                pubkey,
                slot,
                write_version: wv,
                owner,
                lamports,
                executable: 0,
                rent_epoch: 0,
                data: data.to_vec(),
            }
        }
        let rows = vec![
            // X (0x11): three versions, always owned by P1 (0xAA)
            row(pk(0x11), 100, 1, pk(0xAA), 5, b"x-v1"),
            row(pk(0x11), 150, 2, pk(0xAA), 6, b"x-v2"),
            row(pk(0x11), 200, 3, pk(0xAA), 4, b"x-v3"),
            // Y (0x22): P1, then reassigned to P2 (0xBB) at slot 180
            row(pk(0x22), 120, 10, pk(0xAA), 9, b"y-early"),
            row(pk(0x22), 180, 11, pk(0xBB), 9, b"y-moved"),
            // Z (0x33): alive under P1, then closed at 170 (0 lamports, owner reset to system)
            row(pk(0x33), 130, 20, pk(0xAA), 7, b"z-alive"),
            row(pk(0x33), 170, 21, pk(0x00), 0, b""),
        ];
        store()
            .insert_accounts(&rows)
            .await
            .expect("seeding test accounts failed");
    }

    /// Sorted pubkeys returned by a program scan, for order-independent comparison.
    async fn scanned_pubkeys(owner: [u8; 32], slot: u64) -> StoreResult<Vec<[u8; 32]>> {
        let mut keys: Vec<[u8; 32]> = store()
            .get_program_accounts(&owner, slot)
            .await?
            .iter()
            .map(|a| a.pubkey)
            .collect();
        keys.sort();
        Ok(keys)
    }

    // ---- point lookup (get_account_info) ----

    #[tokio::test]
    async fn point_lookup_returns_latest_version_at_slot() {
        seed_test_accounts().await;
        let acct = store()
            .get_account_info(&pk(0x11), 175)
            .await
            .unwrap()
            .expect("X exists at 175");
        assert_eq!(acct.lamports, 6); // the slot-150 version
        assert_eq!(acct.data, b"x-v2");
    }

    #[tokio::test]
    async fn point_lookup_at_later_slot_returns_newer_version() {
        seed_test_accounts().await;
        let acct = store()
            .get_account_info(&pk(0x11), 250)
            .await
            .unwrap()
            .expect("X exists at 250");
        assert_eq!(acct.lamports, 4); // the slot-200 version
        assert_eq!(acct.data, b"x-v3");
    }

    #[tokio::test]
    async fn point_lookup_before_creation_returns_none() {
        seed_test_accounts().await;
        assert!(
            store()
                .get_account_info(&pk(0x11), 99)
                .await
                .unwrap()
                .is_none()
        );
    }

    // ---- program scan (get_program_accounts) ----

    #[tokio::test]
    async fn scan_includes_owned_alive_accounts() {
        seed_test_accounts().await;
        // At slot 150: X owned by P1, Y still under P1, Z still alive under P1.
        assert_eq!(
            scanned_pubkeys(pk(0xAA), 150).await.unwrap(),
            vec![pk(0x11), pk(0x22), pk(0x33)]
        );
    }

    #[tokio::test]
    async fn scan_excludes_moved_and_closed_accounts() {
        seed_test_accounts().await;
        // At slot 200: Y moved to P2 (180), Z closed (170). Only X remains under P1.
        assert_eq!(
            scanned_pubkeys(pk(0xAA), 200).await.unwrap(),
            vec![pk(0x11)]
        );
    }

    #[tokio::test]
    async fn scan_finds_account_under_new_owner() {
        seed_test_accounts().await;
        // Y moved to P2 at slot 180, so P2 owns it at slot 200.
        assert_eq!(
            scanned_pubkeys(pk(0xBB), 200).await.unwrap(),
            vec![pk(0x22)]
        );
    }

    #[tokio::test]
    async fn coverage_only_matches_spans_inside_a_segment() {
        store().record_coverage(100, 150).await.unwrap();
        assert!(store().is_covered(120, 140).await.unwrap()); // fully inside [100,150]
        assert!(!store().is_covered(120, 200).await.unwrap()); // 200 is past hi
        assert!(!store().is_covered(90, 140).await.unwrap()); // 90 is below lo
    }
}
