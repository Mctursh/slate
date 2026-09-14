// Typed adapter over slate-hash: converts this era's solana types to bytes. The
// computation lives in slate-hash so every era shares one implementation.

use slate_hash::LtHash;
use solana_account::{AccountSharedData, ReadableAccount};
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
        for (pubkey, old, new) in changes {
            if let Some(old) = old {
                self.lt_hash.mix_out(&lt_hash_account(pubkey, old));
            }
            self.lt_hash.mix_in(&lt_hash_account(pubkey, new));
        }
        self.bank_hash = bank_hash(&self.bank_hash, signature_count, blockhash, &self.lt_hash);
        self.bank_hash
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
