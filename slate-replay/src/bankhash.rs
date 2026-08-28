// Bank-hash computation for the lattice-hash regime (accounts_lt_hash active epoch 804+, accounts_delta_hash removed epoch 807+); mirrors agave v2.2.20.

use sha2::{Digest, Sha256};
use solana_account::{AccountSharedData, ReadableAccount};
use solana_hash::Hash;
use solana_lattice_hash::lt_hash::LtHash;
use solana_pubkey::Pubkey;

// One slot's changes: (pubkey, pre-value, post-value); None pre-value = created this slot.
pub type SlotChange = (Pubkey, Option<AccountSharedData>, AccountSharedData);

// Lattice element, mirrors agave hash_account_helper (RentEpochInAccountHash::Excluded): blake3 XOF over lamports(LE)||data||executable||owner||pubkey, NO rent_epoch. Dead account = identity.
pub fn lt_hash_account(pubkey: &Pubkey, account: &impl ReadableAccount) -> LtHash {
    if account.lamports() == 0 {
        return LtHash::identity();
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update(&account.lamports().to_le_bytes());
    hasher.update(account.data());
    hasher.update(&[account.executable() as u8]);
    hasher.update(account.owner().as_ref());
    hasher.update(pubkey.as_ref());
    LtHash::with(&hasher)
}

// 1024 lanes as LE u16 (2048 bytes); matches agave bytemuck::must_cast_slice on LE targets.
fn lt_hash_bytes(lt: &LtHash) -> [u8; 2048] {
    let mut bytes = [0u8; 2048];
    for (lane, chunk) in lt.0.iter().zip(bytes.chunks_exact_mut(2)) {
        chunk.copy_from_slice(&lane.to_le_bytes());
    }
    bytes
}

// Lattice-regime bank hash: SHA256(SHA256(parent||sig_count_LE||blockhash)||lt_hash[2048]). No accounts-delta (SIMD-0223), no epoch-accounts-hash (SIMD-0215).
pub fn bank_hash(
    parent_bank_hash: &Hash,
    signature_count: u64,
    last_blockhash: &Hash,
    accounts_lt_hash: &LtHash,
) -> Hash {
    let inner = Sha256::new()
        .chain_update(parent_bank_hash.as_ref())
        .chain_update(signature_count.to_le_bytes())
        .chain_update(last_blockhash.as_ref())
        .finalize();
    let full = Sha256::new()
        .chain_update(inner)
        .chain_update(lt_hash_bytes(accounts_lt_hash))
        .finalize();
    Hash::new_from_array(full.into())
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

    // agave lt_hash.rs::test_checksum_display: identity checksum has a fixed base58 form.
    #[test]
    fn identity_checksum_matches_agave() {
        assert_eq!(
            LtHash::identity().checksum().to_string(),
            "DoL6fvKuTpTQCyUh83NxQw2ewKzWYtq9gsTKp1eQiGC2"
        );
    }

    // agave lt_hash.rs::test_hello_world: checks XOF byte-order (LE u16 lanes) against agave's vector.
    #[test]
    fn with_matches_agave_hello_vector() {
        let mut h = blake3::Hasher::new();
        h.update(b"hello");
        let lt = LtHash::with(&h);
        // First XOF bytes are `ea 8f 16 3d b3 86 ...`, i.e. LE u16 lanes:
        assert_eq!(lt.0[0], 0x8fea);
        assert_eq!(lt.0[1], 0x3d16);
        assert_eq!(lt.0[2], 0x86b3);
        let expected: [u8; 32] = [
            79, 156, 26, 184, 156, 205, 94, 208, 182, 235, 33, 147, 111, 153, 229, 152, 207, 133,
            75, 109, 182, 198, 119, 61, 11, 81, 41, 70, 24, 87, 100, 85,
        ];
        assert_eq!(lt.checksum().0, expected);
    }

    // The bank-hash combine is two nested SHA-256s over the four inputs.
    #[test]
    fn bank_hash_is_two_nested_sha256() {
        let parent = Hash::new_from_array([1u8; 32]);
        let blockhash = Hash::new_from_array([2u8; 32]);
        let lt = LtHash::identity();
        let got = bank_hash(&parent, 7, &blockhash, &lt);

        let inner = Sha256::new()
            .chain_update([1u8; 32])
            .chain_update(7u64.to_le_bytes())
            .chain_update([2u8; 32])
            .finalize();
        let expected = Sha256::new()
            .chain_update(inner)
            .chain_update([0u8; 2048]) // identity lattice = all zeros
            .finalize();
        assert_eq!(got, Hash::new_from_array(expected.into()));
    }

    // Mix in then out returns to start, the homomorphism the roll-forward relies on.
    #[test]
    fn mix_in_then_out_is_identity() {
        let mut acc = LtHash::identity();
        let mut h = blake3::Hasher::new();
        h.update(b"some account element");
        let element = LtHash::with(&h);
        acc.mix_in(&element);
        acc.mix_out(&element);
        assert_eq!(acc.0, LtHash::identity().0);
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
        use crate::snapshot::{read_manifest_hashes, read_manifest_lt_hash};
        use std::fs::File;

        let path = "/Users/mctursh/slate-data/\
                    snapshot-349047024-Cv8fHRuDLaRVhB8YTXGMxbMpZBC1BDGpN5MN99GFGqUv.tar.zst";
        let slot = 349047024;

        // Manifest front: bank_hash(s_snap) and parent_hash (= bank_hash(s_snap-1)).
        let mh = read_manifest_hashes(File::open(path).unwrap(), slot).unwrap();
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
