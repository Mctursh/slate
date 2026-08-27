//! Boundary diff: prove the engine's reconstructed end-state matches mainnet byte-for-byte.
//!
//! After replaying `(S_snap, S_end]`, the engine's store holds every account the range
//! touched at its `S_end` value. The real snapshot at `S_end` is ground truth. Comparing
//! the two over the touched keys is the DATA-fidelity proof the per-tx oracle can't give:
//! the oracle checks lamports + status (what getBlock carries), never arbitrary account
//! data; this checks the bytes. Every account a tx wrote in the range is in the footprint,
//! so a footprint-wide diff catches every write the replay could have gotten wrong.

use std::collections::HashSet;

use solana_account::{AccountSharedData, ReadableAccount};
use solana_pubkey::Pubkey;

use crate::store::AccountStore;

/// One account whose reconstructed value doesn't match the boundary snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mismatch {
    pub pubkey: Pubkey,
    pub kind: MismatchKind,
}

/// How a reconstructed account differs from the boundary snapshot. First difference wins
/// (lamports, then owner, then executable, then data) — one reason per account is enough
/// to flag it for a closer look.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MismatchKind {
    /// Live on one side, dead/absent on the other (`None` = absent or zero-lamport).
    Presence {
        engine_lamports: Option<u64>,
        snapshot_lamports: Option<u64>,
    },
    Lamports {
        engine: u64,
        snapshot: u64,
    },
    Owner {
        engine: Pubkey,
        snapshot: Pubkey,
    },
    Executable {
        engine: bool,
        snapshot: bool,
    },
    /// Data bytes differ; carries both lengths and the first differing offset (for the
    /// equal-length case, the offset points at the first mismatched byte).
    Data {
        engine_len: usize,
        snapshot_len: usize,
        first_diff: Option<usize>,
    },
}

/// The outcome of a boundary diff: how many keys were checked and every mismatch found.
pub struct DiffReport {
    pub checked: usize,
    pub mismatches: Vec<Mismatch>,
}

impl DiffReport {
    /// Byte-exact: every checked account matched.
    pub fn is_exact(&self) -> bool {
        self.mismatches.is_empty()
    }

    pub fn matched(&self) -> usize {
        self.checked - self.mismatches.len()
    }

    /// One-line summary for logging.
    pub fn summary(&self) -> String {
        format!(
            "boundary diff: {}/{} accounts byte-exact, {} mismatch(es)",
            self.matched(),
            self.checked,
            self.mismatches.len()
        )
    }
}

/// Diff the engine's reconstructed state against the boundary snapshot over `keys` (the
/// accounts the range touched — read-only ones trivially match, written ones are the real
/// test). Both sides are [`AccountStore`]s so this scales: the snapshot is streamed into
/// its own store (disk-backed for a big window), and this walks keys doing point-gets, so
/// nothing wider than one account is ever resident.
///
/// Compares lamports, owner, executable, and data bytes — the consensus-relevant fields
/// the lattice hashes. `rent_epoch` is deliberately excluded: the lattice ignores it and
/// rent is disabled at the epoch-808 floor, so a rent_epoch difference is not a fidelity
/// error. A zero-lamport account is treated as absent on both sides (snapshots drop them
/// and the lattice zeroes them out), so a dead-both-sides account matches.
pub fn boundary_diff(
    engine: &dyn AccountStore,
    keys: &HashSet<Pubkey>,
    snapshot: &dyn AccountStore,
) -> DiffReport {
    let mut mismatches = Vec::new();
    let mut checked = 0usize;
    for key in keys {
        checked += 1;
        let engine_live = engine.get(key).filter(|(a, _)| a.lamports() > 0);
        let snapshot_live = snapshot.get(key).filter(|(a, _)| a.lamports() > 0);
        match (engine_live, snapshot_live) {
            // Both dead/absent — a match (an account drained to zero in the range).
            (None, None) => {}
            (Some((engine_account, _)), Some((snapshot_account, _))) => {
                if let Some(kind) = compare_accounts(&engine_account, &snapshot_account) {
                    mismatches.push(Mismatch { pubkey: *key, kind });
                }
            }
            (engine_side, snapshot_side) => {
                mismatches.push(Mismatch {
                    pubkey: *key,
                    kind: MismatchKind::Presence {
                        engine_lamports: engine_side.map(|(a, _)| a.lamports()),
                        snapshot_lamports: snapshot_side.map(|(a, _)| a.lamports()),
                    },
                });
            }
        }
    }
    DiffReport { checked, mismatches }
}

/// First field that differs, or `None` if the two accounts are byte-identical (across the
/// consensus-relevant fields).
fn compare_accounts(engine: &AccountSharedData, snapshot: &AccountSharedData) -> Option<MismatchKind> {
    if engine.lamports() != snapshot.lamports() {
        return Some(MismatchKind::Lamports {
            engine: engine.lamports(),
            snapshot: snapshot.lamports(),
        });
    }
    if engine.owner() != snapshot.owner() {
        return Some(MismatchKind::Owner {
            engine: *engine.owner(),
            snapshot: *snapshot.owner(),
        });
    }
    if engine.executable() != snapshot.executable() {
        return Some(MismatchKind::Executable {
            engine: engine.executable(),
            snapshot: snapshot.executable(),
        });
    }
    if engine.data() != snapshot.data() {
        let first_diff = engine
            .data()
            .iter()
            .zip(snapshot.data())
            .position(|(a, b)| a != b);
        return Some(MismatchKind::Data {
            engine_len: engine.data().len(),
            snapshot_len: snapshot.data().len(),
            first_diff,
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemStore;
    use solana_account::Account;

    fn acct(lamports: u64, owner: [u8; 32], data: &[u8]) -> AccountSharedData {
        AccountSharedData::from(Account {
            lamports,
            data: data.to_vec(),
            owner: Pubkey::new_from_array(owner),
            executable: false,
            rent_epoch: 0,
        })
    }

    // A store the diff can read from. Slots differ on purpose — the diff compares values,
    // not the slot an account was written at.
    fn store(entries: &[(Pubkey, AccountSharedData, u64)]) -> MemStore {
        let mut s = MemStore::default();
        for (key, account, slot) in entries {
            s.put(*key, account.clone(), *slot);
        }
        s
    }

    #[test]
    fn exact_when_every_key_matches() {
        let k1 = Pubkey::new_unique();
        let k2 = Pubkey::new_unique();
        let engine = store(&[
            (k1, acct(100, [1; 32], &[1, 2, 3]), 5),
            (k2, acct(0, [0; 32], &[]), 5), // dead in engine
        ]);
        let snapshot = store(&[
            (k1, acct(100, [1; 32], &[1, 2, 3]), 9), // same value, later slot
            // k2 absent from the snapshot (dead) → matches the engine's zero-lamport
        ]);
        let keys = HashSet::from([k1, k2]);

        let report = boundary_diff(&engine, &keys, &snapshot);
        assert!(report.is_exact(), "{:?}", report.mismatches);
        assert_eq!(report.checked, 2);
        assert_eq!(report.matched(), 2);
    }

    #[test]
    fn catches_a_data_byte_diff_with_the_offset() {
        let k = Pubkey::new_unique();
        let engine = store(&[(k, acct(50, [7; 32], &[10, 20, 30, 40]), 5)]);
        let snapshot = store(&[(k, acct(50, [7; 32], &[10, 20, 99, 40]), 5)]);
        let keys = HashSet::from([k]);

        let report = boundary_diff(&engine, &keys, &snapshot);
        assert_eq!(report.mismatches.len(), 1);
        assert_eq!(
            report.mismatches[0].kind,
            MismatchKind::Data {
                engine_len: 4,
                snapshot_len: 4,
                first_diff: Some(2),
            }
        );
    }

    #[test]
    fn catches_lamports_owner_and_presence() {
        let lam = Pubkey::new_unique();
        let own = Pubkey::new_unique();
        let gone = Pubkey::new_unique();
        let engine = store(&[
            (lam, acct(100, [1; 32], &[]), 5),
            (own, acct(100, [1; 32], &[]), 5),
            (gone, acct(100, [1; 32], &[]), 5), // live in engine, dead in snapshot
        ]);
        let snapshot = store(&[
            (lam, acct(200, [1; 32], &[]), 5),        // lamports differ
            (own, acct(100, [2; 32], &[]), 5),        // owner differs
            // gone absent → presence mismatch
        ]);
        let keys = HashSet::from([lam, own, gone]);

        let report = boundary_diff(&engine, &keys, &snapshot);
        assert_eq!(report.checked, 3);
        assert_eq!(report.mismatches.len(), 3);

        let kind = |k: &Pubkey| {
            report
                .mismatches
                .iter()
                .find(|m| &m.pubkey == k)
                .map(|m| m.kind.clone())
        };
        assert_eq!(
            kind(&lam),
            Some(MismatchKind::Lamports {
                engine: 100,
                snapshot: 200
            })
        );
        assert!(matches!(kind(&own), Some(MismatchKind::Owner { .. })));
        assert_eq!(
            kind(&gone),
            Some(MismatchKind::Presence {
                engine_lamports: Some(100),
                snapshot_lamports: None
            })
        );
    }
}
