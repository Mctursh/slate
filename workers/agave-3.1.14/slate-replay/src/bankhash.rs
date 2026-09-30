// Typed adapter over slate-hash: converts this era's solana types to bytes. The
// computation lives in slate-hash so every era shares one implementation.

use slate_hash::LtHash;
use solana_account::{AccountSharedData, ReadableAccount, WritableAccount};
use solana_hash::Hash;
use solana_pubkey::Pubkey;

// One slot's changes: (pubkey, pre-value, post-value); None pre-value = created this slot.
pub type SlotChange = (Pubkey, Option<AccountSharedData>, AccountSharedData);

pub fn lt_hash_account(pubkey: &Pubkey, account: &impl ReadableAccount) -> LtHash {
    slate_hash::lt_hash_account(
        &pubkey.to_bytes(),
        account.lamports(),
        account.data(),
        account.executable(),
        &account.owner().to_bytes(),
    )
}

pub fn bank_hash(
    parent_bank_hash: &Hash,
    signature_count: u64,
    last_blockhash: &Hash,
    accounts_lt_hash: &LtHash,
) -> Hash {
    Hash::new_from_array(slate_hash::bank_hash(
        &parent_bank_hash.to_bytes(),
        signature_count,
        &last_blockhash.to_bytes(),
        accounts_lt_hash,
    ))
}

// Divergence-hunt targets, read once: roll_slot runs per slot and must not touch the environment.
static SEARCH_TARGET: std::sync::LazyLock<Option<Hash>> = std::sync::LazyLock::new(|| {
    std::env::var("SLATE_HASH_SEARCH_TARGET")
        .ok()
        .and_then(|raw| raw.parse::<Hash>().ok())
});
static REVERT_LISTS: std::sync::LazyLock<Vec<String>> = std::sync::LazyLock::new(|| {
    std::env::var("SLATE_HASH_REVERT_LIST")
        .map(|v| v.split(',').map(|s| s.trim().to_string()).collect())
        .unwrap_or_default()
});

// Rolls the lattice forward per slot (mix out old, in new) and computes each bank hash, parent for the next slot and the SlotHashes entry.
pub struct BankHashRoller {
    lt_hash: LtHash,
    bank_hash: Hash,
}

impl BankHashRoller {
    pub fn new(lt_hash: LtHash, bank_hash: Hash) -> Self {
        Self { lt_hash, bank_hash }
    }

    // Current bank hash; prepended into SlotHashes for the next slot.
    pub fn bank_hash(&self) -> Hash {
        self.bank_hash
    }

    // By reference: LtHash is large and deliberately not Copy.
    pub fn lt_hash(&self) -> &LtHash {
        &self.lt_hash
    }

    pub fn roll_slot(
        &mut self,
        changes: &[SlotChange],
        signature_count: u64,
        blockhash: &Hash,
    ) -> Hash {
        let parent = self.bank_hash;
        for (pubkey, old, new) in changes {
            if let Some(old) = old {
                self.lt_hash.mix_out(&lt_hash_account(pubkey, old));
            }
            self.lt_hash.mix_in(&lt_hash_account(pubkey, new));
        }
        self.bank_hash = bank_hash(&parent, signature_count, blockhash, &self.lt_hash);
        for pubkey in self.search_single_account(&parent, changes, signature_count, blockhash) {
            eprintln!("hash-search HIT: target reached by reverting {pubkey}");
        }
        for label in self.search_subsets(&parent, changes, signature_count, blockhash) {
            eprintln!("hash-search SUBSET HIT: target reached by reverting {label}");
        }
        for label in self.search_variants(&parent, changes, signature_count, blockhash) {
            eprintln!("hash-search VARIANT HIT: {label}");
        }
        if let Some(label) = self.search_revert_list(&parent, changes, signature_count, blockhash) {
            eprintln!("hash-search LIST HIT: {label}");
        }
        self.bank_hash
    }

    fn search_single_account(
        &self,
        parent: &Hash,
        changes: &[SlotChange],
        signature_count: u64,
        blockhash: &Hash,
    ) -> Vec<Pubkey> {
        let Some(target) = *SEARCH_TARGET else {
            return Vec::new();
        };
        self.revert_candidates(parent, changes, signature_count, blockhash, &target)
    }

    fn search_subsets(
        &self,
        parent: &Hash,
        changes: &[SlotChange],
        signature_count: u64,
        blockhash: &Hash,
    ) -> Vec<&'static str> {
        let Some(target) = *SEARCH_TARGET else {
            return Vec::new();
        };
        if self.bank_hash == target {
            return Vec::new();
        }
        let stake = solana_sdk_ids::stake::id();
        type Predicate = (&'static str, fn(&SlotChange, &Pubkey) -> bool);
        let preds: [Predicate; 5] = [
            (
                "data-only changes (lamports unchanged)",
                |(_, old, new), _| old.as_ref().is_some_and(|o| o.lamports() == new.lamports()),
            ),
            ("data-only stake-owned changes", |(_, old, new), stake| {
                old.as_ref().is_some_and(|o| o.lamports() == new.lamports()) && new.owner() == stake
            }),
            ("all stake-owned changes", |(_, _, new), stake| {
                new.owner() == stake
            }),
            ("accounts created this slot", |(_, old, _), _| old.is_none()),
            ("non-stake-owned changes", |(_, _, new), stake| {
                new.owner() != stake
            }),
        ];
        let mut hits = Vec::new();
        for (label, pred) in preds {
            let mut cand = self.lt_hash.clone();
            let mut n = 0usize;
            for ch in changes {
                if pred(ch, &stake) {
                    let (pubkey, old, new) = ch;
                    cand.mix_out(&lt_hash_account(pubkey, new));
                    if let Some(old) = old {
                        cand.mix_in(&lt_hash_account(pubkey, old));
                    }
                    n += 1;
                }
            }
            if n > 0 && bank_hash(parent, signature_count, blockhash, &cand) == target {
                hits.push(label);
            }
        }
        hits
    }

    fn search_variants(
        &self,
        parent: &Hash,
        changes: &[SlotChange],
        signature_count: u64,
        blockhash: &Hash,
    ) -> Vec<String> {
        let Some(target) = *SEARCH_TARGET else {
            return Vec::new();
        };
        if self.bank_hash == target {
            return Vec::new();
        }
        const CANON: usize = 200;
        let mut hits = Vec::new();
        for (pubkey, old, new) in changes {
            if new.data().len() == CANON || new.owner() != &solana_sdk_ids::stake::id() {
                continue;
            }
            let Some(old) = old else { continue };
            let mut base = self.lt_hash.clone();
            base.mix_out(&lt_hash_account(pubkey, new));

            let mut keep_old_data = old.clone();
            keep_old_data.set_lamports(new.lamports());

            let mut zero_tail = new.clone();
            {
                let d = zero_tail.data_as_mut_slice();
                for b in d.iter_mut().skip(CANON) {
                    *b = 0;
                }
            }

            let mut truncated = new.clone();
            truncated.set_data_from_slice(&new.data()[..CANON.min(new.data().len())]);

            let mut old_truncated = old.clone();
            old_truncated.set_lamports(new.lamports());
            old_truncated.set_data_from_slice(&old.data()[..CANON.min(old.data().len())]);

            for (label, cand) in [
                ("new-lamports + OLD data", &keep_old_data),
                ("new state, tail beyond 200 ZEROED", &zero_tail),
                ("new state TRUNCATED to 200", &truncated),
                ("old data truncated to 200", &old_truncated),
            ] {
                let mut lt = base.clone();
                lt.mix_in(&lt_hash_account(pubkey, cand));
                if bank_hash(parent, signature_count, blockhash, &lt) == target {
                    hits.push(format!("{pubkey}: {label}"));
                }
            }
        }
        hits
    }

    fn search_revert_list(
        &self,
        parent: &Hash,
        changes: &[SlotChange],
        signature_count: u64,
        blockhash: &Hash,
    ) -> Option<String> {
        let target = (*SEARCH_TARGET)?;
        if self.bank_hash == target {
            return None;
        }
        for path in REVERT_LISTS.iter() {
            let Ok(body) = std::fs::read_to_string(path) else {
                continue;
            };
            let want: std::collections::HashSet<String> =
                body.lines().map(|l| l.trim().to_string()).collect();
            let mut cand = self.lt_hash.clone();
            let mut n = 0usize;
            for (pubkey, old, new) in changes {
                if !want.contains(&pubkey.to_string()) {
                    continue;
                }
                cand.mix_out(&lt_hash_account(pubkey, new));
                if let Some(old) = old {
                    cand.mix_in(&lt_hash_account(pubkey, old));
                }
                n += 1;
            }
            if n == 0 {
                continue;
            }
            if bank_hash(parent, signature_count, blockhash, &cand) == target {
                return Some(format!(
                    "{path}: reverting {n} account(s) reaches the target"
                ));
            }
            eprintln!("list-revert: {path} -> {n} reverted, no match");
        }
        None
    }

    fn revert_candidates(
        &self,
        parent: &Hash,
        changes: &[SlotChange],
        signature_count: u64,
        blockhash: &Hash,
        target: &Hash,
    ) -> Vec<Pubkey> {
        if self.bank_hash == *target {
            return Vec::new();
        }
        let mut hits = Vec::new();
        for (pubkey, old, new) in changes {
            let mut cand = self.lt_hash.clone();
            cand.mix_out(&lt_hash_account(pubkey, new));
            if let Some(old) = old {
                cand.mix_in(&lt_hash_account(pubkey, old));
            }
            if bank_hash(parent, signature_count, blockhash, &cand) == *target {
                hits.push(*pubkey);
            }
        }
        hits
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The extraction proof: slate-hash owns its LtHash, so nothing but this test stops
    // the two drifting apart. solana-lattice-hash is a dev-dependency for exactly this.
    #[test]
    fn slate_hash_agrees_with_agave_lattice_hash() {
        use solana_lattice_hash::lt_hash::LtHash as AgaveLtHash;

        let (k1, k2) = (Pubkey::new_unique(), Pubkey::new_unique());
        let a1 = test_account(100, &[1, 2, 3]);
        let a2 = test_account(7_777, &[]);

        let agave_element = |k: &Pubkey, a: &AccountSharedData| {
            let mut h = blake3::Hasher::new();
            h.update(&a.lamports().to_le_bytes());
            h.update(a.data());
            h.update(&[a.executable() as u8]);
            h.update(a.owner().as_ref());
            h.update(k.as_ref());
            AgaveLtHash::with(&h)
        };

        // Per-account elements agree lane for lane.
        assert_eq!(lt_hash_account(&k1, &a1).0, agave_element(&k1, &a1).0);

        // And so does an accumulator built by the same mix_in/mix_out sequence.
        let mut agave = AgaveLtHash::identity();
        agave.mix_in(&agave_element(&k1, &a1));
        agave.mix_in(&agave_element(&k2, &a2));
        agave.mix_out(&agave_element(&k1, &a1));

        let mut ours = LtHash::identity();
        ours.mix_in(&lt_hash_account(&k1, &a1));
        ours.mix_in(&lt_hash_account(&k2, &a2));
        ours.mix_out(&lt_hash_account(&k1, &a1));

        assert_eq!(ours.0, agave.0);
        assert_eq!(ours.checksum().0, agave.checksum().0);
        assert_eq!(ours.checksum().to_string(), agave.checksum().to_string());
    }

    fn test_account(lamports: u64, data: &[u8]) -> AccountSharedData {
        use solana_account::Account;
        AccountSharedData::from(Account {
            lamports,
            data: data.to_vec(),
            owner: Pubkey::new_from_array([7; 32]),
            executable: false,
            rent_epoch: 0,
        })
    }

    // Rolling a slot that creates two accounts equals mixing both elements directly.
    #[test]
    fn roller_creates_accounts_like_a_direct_sum() {
        let (k1, k2) = (Pubkey::new_unique(), Pubkey::new_unique());
        let a1 = test_account(100, &[1, 2, 3]);
        let a2 = test_account(200, &[4, 5]);
        let blockhash = Hash::new_from_array([9; 32]);

        let mut roller = BankHashRoller::new(LtHash::identity(), Hash::default());
        let bh = roller.roll_slot(
            &[(k1, None, a1.clone()), (k2, None, a2.clone())],
            5,
            &blockhash,
        );

        let mut lt = LtHash::identity();
        lt.mix_in(&lt_hash_account(&k1, &a1));
        lt.mix_in(&lt_hash_account(&k2, &a2));
        assert_eq!(bh, bank_hash(&Hash::default(), 5, &blockhash, &lt));
        assert_eq!(roller.bank_hash(), bh);
    }

    // The divergence search must actually find a planted culprit, not silently return nothing.
    #[test]
    fn revert_candidates_names_the_one_account_that_closes_the_gap() {
        let (k1, k2) = (Pubkey::new_unique(), Pubkey::new_unique());
        let before = test_account(100, &[1, 2, 3]);
        let after = test_account(150, &[9, 9, 9]);
        let created = test_account(200, &[4, 5]);
        let blockhash = Hash::new_from_array([3; 32]);
        let parent = Hash::new_from_array([1; 32]);

        let mut lt0 = LtHash::identity();
        lt0.mix_in(&lt_hash_account(&k1, &before));
        let mut roller = BankHashRoller::new(lt0, parent);
        let changes = vec![
            (k1, Some(before.clone()), after.clone()),
            (k2, None, created.clone()),
        ];
        let got = roller.roll_slot(&changes, 4, &blockhash);

        let mut without_k2 = LtHash::identity();
        without_k2.mix_in(&lt_hash_account(&k1, &after));
        let target = bank_hash(&parent, 4, &blockhash, &without_k2);
        assert_ne!(got, target);
        assert_eq!(
            roller.revert_candidates(&parent, &changes, 4, &blockhash, &target),
            vec![k2]
        );

        let mut reverted_k1 = LtHash::identity();
        reverted_k1.mix_in(&lt_hash_account(&k1, &before));
        reverted_k1.mix_in(&lt_hash_account(&k2, &created));
        let target_k1 = bank_hash(&parent, 4, &blockhash, &reverted_k1);
        assert_eq!(
            roller.revert_candidates(&parent, &changes, 4, &blockhash, &target_k1),
            vec![k1]
        );

        assert!(
            roller
                .revert_candidates(&parent, &changes, 4, &blockhash, &got)
                .is_empty()
        );
    }

    // Updating rolls out old + in new, so the lattice ends holding only the new value.
    #[test]
    fn roller_updates_an_account_by_mixing_out_then_in() {
        let k = Pubkey::new_unique();
        let before = test_account(100, &[1, 2, 3]);
        let after = test_account(150, &[9, 9, 9]);

        // Bootstrap lattice already contains `before`.
        let mut lt0 = LtHash::identity();
        lt0.mix_in(&lt_hash_account(&k, &before));
        let mut roller = BankHashRoller::new(lt0, Hash::default());

        roller.roll_slot(&[(k, Some(before), after.clone())], 1, &Hash::default());

        // The lattice should now hold only `after`.
        let mut expected = LtHash::identity();
        expected.mix_in(&lt_hash_account(&k, &after));
        assert_eq!(
            roller.bank_hash(),
            bank_hash(&Hash::default(), 1, &Hash::default(), &expected)
        );
    }

    // KEYSTONE: recompute the real mainnet bank hash from manifest lattice+parent + on-chain sig count/blockhash; proves bit-exact.
    #[test]
    #[ignore = "needs the local mainnet snapshot at /Users/mctursh/slate-data"]
    fn keystone_reproduces_the_mainnet_bank_hash() {
        use crate::snapshot::{read_manifest_fields, read_manifest_lt_hash};
        use std::fs::File;

        let path = "/Users/mctursh/slate-data/\
                    snapshot-349047024-Cv8fHRuDLaRVhB8YTXGMxbMpZBC1BDGpN5MN99GFGqUv.tar.zst";
        let slot = 349047024;

        // Manifest front: bank_hash(s_snap) and parent_hash (= bank_hash(s_snap-1)).
        let mh = read_manifest_fields(File::open(path).unwrap(), slot).unwrap();
        // Manifest tail: the accounts lattice hash.
        let lt = read_manifest_lt_hash(File::open(path).unwrap(), slot)
            .unwrap()
            .expect("accounts_lt_hash is serialized in the manifest at epoch 807");

        // The remaining two inputs are this slot's on-chain values (getBlock 349047024): sig count + blockhash.
        let signature_count = 1890u64;
        let blockhash: Hash = "BaUZWzsjp8aicbMfFQ9Z7xsqT5TbHHHSbzZ6Kd6R1QfP"
            .parse()
            .unwrap();

        let computed = bank_hash(&mh.parent_hash, signature_count, &blockhash, &lt);
        assert_eq!(
            computed, mh.bank_hash,
            "recomputed bank hash must equal the manifest's own bank hash"
        );
        let expected: Hash = "Cv87aY5YPjpDpWfEzbikfxyhthNmfYSJ1rZdbJfQ8gm6"
            .parse()
            .unwrap();
        assert_eq!(
            mh.bank_hash, expected,
            "and it's the real mainnet bank hash"
        );
    }
}
