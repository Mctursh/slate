//! Bank-hash computation for the lattice regime, shared by every era's worker.
//!
//! Owns its `LtHash` instead of depending on `solana-lattice-hash`, which is already
//! multi-major in Slate's lockfile: two copies of a consensus-frozen algorithm is the
//! drift this crate exists to prevent. Verified byte-identical to agave's `lt_hash.rs`
//! at 2.2.20, 3.1.14 and 4.2.1; `slate_replay::bankhash` cross-checks it.

use sha2::{Digest, Sha256};
use std::fmt;

/// A 16-bit, 1024-element lattice hash over blake3. Homomorphic, so a slot's changes
/// roll the accumulator forward without rehashing the account universe.
//
// Deliberately not Copy (2 KiB) and not Default (the identity is a specific value).
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct LtHash(pub [u16; LtHash::NUM_ELEMENTS]);

impl LtHash {
    pub const NUM_ELEMENTS: usize = 1024;
    pub const NUM_BYTES: usize = 2048;

    #[must_use]
    pub const fn identity() -> Self {
        Self([0; Self::NUM_ELEMENTS])
    }

    /// Builds a lattice element from everything already hashed into `hasher`.
    #[must_use]
    pub fn with(hasher: &blake3::Hasher) -> Self {
        let mut bytes = [0u8; Self::NUM_BYTES];
        hasher.finalize_xof().fill(&mut bytes);
        Self::from_bytes(&bytes)
    }

    pub fn mix_in(&mut self, other: &Self) {
        for (lane, add) in self.0.iter_mut().zip(other.0.iter()) {
            *lane = lane.wrapping_add(*add);
        }
    }

    pub fn mix_out(&mut self, other: &Self) {
        for (lane, sub) in self.0.iter_mut().zip(other.0.iter()) {
            *lane = lane.wrapping_sub(*sub);
        }
    }

    pub fn checksum(&self) -> Checksum {
        Checksum(blake3::hash(&self.to_bytes()).into())
    }

    /// Canonical 2048-byte form, lanes as little-endian `u16`.
    ///
    /// agave gets here via `bytemuck::must_cast_slice`, which is native-endian. Spelling
    /// out LE keeps the on-disk format a contract rather than a property of the host, and
    /// agrees with agave on every target it supports.
    pub fn to_bytes(&self) -> [u8; Self::NUM_BYTES] {
        let mut bytes = [0u8; Self::NUM_BYTES];
        for (lane, chunk) in self.0.iter().zip(bytes.chunks_exact_mut(2)) {
            chunk.copy_from_slice(&lane.to_le_bytes());
        }
        bytes
    }

    pub fn from_bytes(bytes: &[u8; Self::NUM_BYTES]) -> Self {
        let mut lanes = [0u16; Self::NUM_ELEMENTS];
        for (lane, chunk) in lanes.iter_mut().zip(bytes.chunks_exact(2)) {
            *lane = u16::from_le_bytes([chunk[0], chunk[1]]);
        }
        Self(lanes)
    }
}

impl fmt::Display for LtHash {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.checksum())
    }
}

/// A 32-byte digest of an `LtHash`, for the places 2 KiB is too large to log or compare.
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct Checksum(pub [u8; 32]);

impl fmt::Display for Checksum {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", bs58::encode(&self.0).into_string())
    }
}

/// One account's lattice element: blake3 XOF over
/// `lamports(LE) || data || executable || owner || pubkey`, with **no rent_epoch**
/// (agave's `hash_account_helper` under `RentEpochInAccountHash::Excluded`).
///
/// A zero-lamport account is dead and hashes to the identity, so a deletion mixes out
/// exactly what was mixed in.
pub fn lt_hash_account(
    pubkey: &[u8; 32],
    lamports: u64,
    data: &[u8],
    executable: bool,
    owner: &[u8; 32],
) -> LtHash {
    if lamports == 0 {
        return LtHash::identity();
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update(&lamports.to_le_bytes());
    hasher.update(data);
    hasher.update(&[executable as u8]);
    hasher.update(owner);
    hasher.update(pubkey);
    LtHash::with(&hasher)
}

/// `SHA256(SHA256(parent || sig_count_LE || blockhash) || lt_hash[2048])`.
///
/// Lattice regime only: no accounts-delta hash (SIMD-0223, removed at epoch 807), no
/// epoch-accounts hash (SIMD-0215). An era predating those removals needs a different
/// combine, so this stays a plain function rather than something the roller hides.
pub fn bank_hash(
    parent_bank_hash: &[u8; 32],
    signature_count: u64,
    last_blockhash: &[u8; 32],
    accounts_lt_hash: &LtHash,
) -> [u8; 32] {
    let inner = Sha256::new()
        .chain_update(parent_bank_hash)
        .chain_update(signature_count.to_le_bytes())
        .chain_update(last_blockhash)
        .finalize();
    Sha256::new()
        .chain_update(inner)
        .chain_update(accounts_lt_hash.to_bytes())
        .finalize()
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    // agave lt_hash.rs::test_checksum_display.
    #[test]
    fn identity_checksum_matches_agave() {
        assert_eq!(
            LtHash::identity().checksum().to_string(),
            "DoL6fvKuTpTQCyUh83NxQw2ewKzWYtq9gsTKp1eQiGC2"
        );
    }

    // agave lt_hash.rs::test_hello_world; pins the XOF byte order (LE u16 lanes).
    #[test]
    fn with_matches_agave_hello_vector() {
        let mut h = blake3::Hasher::new();
        h.update(b"hello");
        let lt = LtHash::with(&h);
        // XOF starts `ea 8f 16 3d b3 86 ...`.
        assert_eq!(lt.0[0], 0x8fea);
        assert_eq!(lt.0[1], 0x3d16);
        assert_eq!(lt.0[2], 0x86b3);
        assert_eq!(
            lt.checksum().0,
            [
                79, 156, 26, 184, 156, 205, 94, 208, 182, 235, 33, 147, 111, 153, 229, 152, 207,
                133, 75, 109, 182, 198, 119, 61, 11, 81, 41, 70, 24, 87, 100, 85,
            ]
        );
    }

    #[test]
    fn bytes_round_trip() {
        let mut lanes = [0u16; LtHash::NUM_ELEMENTS];
        for (i, l) in lanes.iter_mut().enumerate() {
            *l = (i as u16).wrapping_mul(7).wrapping_add(1);
        }
        let lt = LtHash(lanes);
        assert_eq!(LtHash::from_bytes(&lt.to_bytes()), lt);
    }

    // The homomorphism the roll-forward depends on.
    #[test]
    fn mix_in_then_out_is_identity() {
        let mut acc = LtHash::identity();
        let mut h = blake3::Hasher::new();
        h.update(b"some account element");
        let element = LtHash::with(&h);
        acc.mix_in(&element);
        acc.mix_out(&element);
        assert_eq!(acc, LtHash::identity());
    }

    #[test]
    fn a_dead_account_is_the_identity() {
        let lt = lt_hash_account(&[1; 32], 0, &[1, 2, 3], false, &[2; 32]);
        assert_eq!(lt, LtHash::identity());
    }

    // Pins the five fields and their order.
    #[test]
    fn account_element_hashes_its_five_fields_in_order() {
        let (pubkey, owner) = ([3u8; 32], [9u8; 32]);
        let got = lt_hash_account(&pubkey, 42, &[7, 7], true, &owner);

        let mut h = blake3::Hasher::new();
        h.update(&42u64.to_le_bytes());
        h.update(&[7, 7]);
        h.update(&[1]);
        h.update(&owner);
        h.update(&pubkey);
        assert_eq!(got, LtHash::with(&h));
    }

    #[test]
    fn bank_hash_is_two_nested_sha256() {
        let got = bank_hash(&[1; 32], 7, &[2; 32], &LtHash::identity());

        let inner = Sha256::new()
            .chain_update([1u8; 32])
            .chain_update(7u64.to_le_bytes())
            .chain_update([2u8; 32])
            .finalize();
        let expected: [u8; 32] = Sha256::new()
            .chain_update(inner)
            .chain_update([0u8; 2048]) // identity lattice
            .finalize()
            .into();
        assert_eq!(got, expected);
    }
}
