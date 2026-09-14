//! On-disk byte formats: the account record and the resume checkpoint.
//!
//! Cross-era contracts. A checkpoint may be read by a different binary linking a
//! different agave, so the layouts are spelled out here rather than being whatever
//! `bincode` made of a version-specific type.
//!
//! Decoders refuse an unknown version rather than returning `None`. The failure being
//! replaced is a caller falling back to a plausible default and diverging a thousand
//! slots later with nothing pointing at the cause.

use slate_hash::LtHash;
use std::fmt;

#[derive(Debug, PartialEq, Eq)]
pub enum FormatError {
    BadMagic,
    UnknownCheckpointVersion(u32),
    UnknownAccountFormat(u32),
    Truncated { field: &'static str },
}

impl fmt::Display for FormatError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::BadMagic => write!(f, "not a slate checkpoint (bad magic)"),
            Self::UnknownCheckpointVersion(v) => write!(
                f,
                "checkpoint format v{v}, this build understands v{CHECKPOINT_VERSION}"
            ),
            Self::UnknownAccountFormat(v) => write!(
                f,
                "account record format v{v}, this build understands v{ACCOUNT_FORMAT_VERSION}"
            ),
            Self::Truncated { field } => write!(f, "checkpoint truncated reading `{field}`"),
        }
    }
}

impl std::error::Error for FormatError {}

type Result<T> = std::result::Result<T, FormatError>;

// ---------------------------------------------------------------- account record

pub const ACCOUNT_FORMAT_VERSION: u32 = 1;

/// `slot(8) | lamports(8) | rent_epoch(8) | executable(1) | owner(32) | data(rest)`.
pub const ACCOUNT_HEAD_BYTES: usize = 57;

/// Borrows `data` out of the stored bytes; decoded once per account read.
#[derive(Debug, PartialEq, Eq)]
pub struct AccountRecord<'a> {
    pub slot: u64,
    pub lamports: u64,
    pub rent_epoch: u64,
    pub executable: bool,
    pub owner: [u8; 32],
    pub data: &'a [u8],
}

// Version lives in the checkpoint, not per record: it describes the whole store, and
// 4 bytes times 8.4 million accounts buys nothing.
pub fn encode_account(
    slot: u64,
    lamports: u64,
    rent_epoch: u64,
    executable: bool,
    owner: &[u8; 32],
    data: &[u8],
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(ACCOUNT_HEAD_BYTES + data.len());
    buf.extend_from_slice(&slot.to_le_bytes());
    buf.extend_from_slice(&lamports.to_le_bytes());
    buf.extend_from_slice(&rent_epoch.to_le_bytes());
    buf.push(executable as u8);
    buf.extend_from_slice(owner);
    buf.extend_from_slice(data);
    buf
}

pub fn decode_account(bytes: &[u8]) -> Result<AccountRecord<'_>> {
    if bytes.len() < ACCOUNT_HEAD_BYTES {
        return Err(FormatError::Truncated { field: "account" });
    }
    Ok(AccountRecord {
        slot: u64::from_le_bytes(bytes[0..8].try_into().unwrap()),
        lamports: u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
        rent_epoch: u64::from_le_bytes(bytes[16..24].try_into().unwrap()),
        executable: bytes[24] != 0,
        owner: bytes[25..57].try_into().unwrap(),
        data: &bytes[ACCOUNT_HEAD_BYTES..],
    })
}

// ------------------------------------------------------------------- checkpoint

const MAGIC: &[u8; 8] = b"SLCKPT\0\0";
pub const CHECKPOINT_VERSION: u32 = 1;

/// Where the lattice and parent hash stood at `Checkpoint::slot`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollState {
    pub lt_hash: LtHash,
    pub bank_hash: [u8; 32],
}

/// One stake account's pending payout, flattened out of the era's `StakeReward` type.
#[derive(Debug, Clone, PartialEq)]
pub struct StakeRewardRecord {
    pub stake_pubkey: [u8; 32],
    pub lamports: u64,
    pub voter_pubkey: [u8; 32],
    pub stake: u64,
    pub activation_epoch: u64,
    pub deactivation_epoch: u64,
    pub warmup_cooldown_rate: f64,
    pub credits_observed: u64,
}

pub const STAKE_REWARD_BYTES: usize = 112;

/// Everything a resume needs to continue bit-identically.
///
/// One blob, not a key per field, so "the parts disagree about which slot they're from"
/// is not a state the code can reach. That bug existed; this fixes it by construction
/// rather than by remembering to stage every write into one transaction.
#[derive(Debug, Clone, PartialEq)]
pub struct Checkpoint {
    pub slot: u64,
    pub capitalization: u64,
    /// `None` while the bank-hash roll isn't active (tests, and seeding before bootstrap).
    pub roll: Option<RollState>,
    /// Accumulated across the whole replay, so a resume can't rebuild it from the range.
    pub stake_keys: Vec<[u8; 32]>,
    /// Calculated at a boundary, consumed over the blocks after it.
    pub pending_partitions: Vec<Vec<StakeRewardRecord>>,
}

impl Checkpoint {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(
            96 + self.stake_keys.len() * 32
                + self
                    .pending_partitions
                    .iter()
                    .map(|p| 4 + p.len() * STAKE_REWARD_BYTES)
                    .sum::<usize>(),
        );
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&CHECKPOINT_VERSION.to_le_bytes());
        out.extend_from_slice(&ACCOUNT_FORMAT_VERSION.to_le_bytes());
        out.extend_from_slice(&self.slot.to_le_bytes());
        out.extend_from_slice(&self.capitalization.to_le_bytes());

        match &self.roll {
            Some(roll) => {
                out.push(1);
                out.extend_from_slice(&roll.lt_hash.to_bytes());
                out.extend_from_slice(&roll.bank_hash);
            }
            None => out.push(0),
        }

        out.extend_from_slice(&(self.stake_keys.len() as u32).to_le_bytes());
        for key in &self.stake_keys {
            out.extend_from_slice(key);
        }

        out.extend_from_slice(&(self.pending_partitions.len() as u32).to_le_bytes());
        for partition in &self.pending_partitions {
            out.extend_from_slice(&(partition.len() as u32).to_le_bytes());
            for r in partition {
                out.extend_from_slice(&r.stake_pubkey);
                out.extend_from_slice(&r.lamports.to_le_bytes());
                out.extend_from_slice(&r.voter_pubkey);
                out.extend_from_slice(&r.stake.to_le_bytes());
                out.extend_from_slice(&r.activation_epoch.to_le_bytes());
                out.extend_from_slice(&r.deactivation_epoch.to_le_bytes());
                out.extend_from_slice(&r.warmup_cooldown_rate.to_le_bytes());
                out.extend_from_slice(&r.credits_observed.to_le_bytes());
            }
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut r = Reader::new(bytes);
        if r.take(8, "magic")? != MAGIC {
            return Err(FormatError::BadMagic);
        }
        let version = r.u32("version")?;
        if version != CHECKPOINT_VERSION {
            return Err(FormatError::UnknownCheckpointVersion(version));
        }
        let account_format = r.u32("account_format")?;
        if account_format != ACCOUNT_FORMAT_VERSION {
            return Err(FormatError::UnknownAccountFormat(account_format));
        }
        let slot = r.u64("slot")?;
        let capitalization = r.u64("capitalization")?;

        let roll = match r.take(1, "roll_present")?[0] {
            0 => None,
            _ => Some(RollState {
                lt_hash: LtHash::from_bytes(
                    r.take(LtHash::NUM_BYTES, "lt_hash")?.try_into().unwrap(),
                ),
                bank_hash: r.take(32, "bank_hash")?.try_into().unwrap(),
            }),
        };

        let key_count = r.u32("stake_key_count")? as usize;
        let mut stake_keys = Vec::with_capacity(key_count.min(MAX_PREALLOC));
        for _ in 0..key_count {
            stake_keys.push(r.take(32, "stake_key")?.try_into().unwrap());
        }

        let partition_count = r.u32("partition_count")? as usize;
        let mut pending_partitions = Vec::with_capacity(partition_count.min(MAX_PREALLOC));
        for _ in 0..partition_count {
            let n = r.u32("partition_len")? as usize;
            let mut partition = Vec::with_capacity(n.min(MAX_PREALLOC));
            for _ in 0..n {
                let f = r.take(STAKE_REWARD_BYTES, "stake_reward")?;
                partition.push(StakeRewardRecord {
                    stake_pubkey: f[0..32].try_into().unwrap(),
                    lamports: u64::from_le_bytes(f[32..40].try_into().unwrap()),
                    voter_pubkey: f[40..72].try_into().unwrap(),
                    stake: u64::from_le_bytes(f[72..80].try_into().unwrap()),
                    activation_epoch: u64::from_le_bytes(f[80..88].try_into().unwrap()),
                    deactivation_epoch: u64::from_le_bytes(f[88..96].try_into().unwrap()),
                    warmup_cooldown_rate: f64::from_le_bytes(f[96..104].try_into().unwrap()),
                    credits_observed: u64::from_le_bytes(f[104..112].try_into().unwrap()),
                });
            }
            pending_partitions.push(partition);
        }

        Ok(Self {
            slot,
            capitalization,
            roll,
            stake_keys,
            pending_partitions,
        })
    }
}

// So a corrupt length can't allocate gigabytes before the read that would reject it.
const MAX_PREALLOC: usize = 1 << 20;

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn take(&mut self, n: usize, field: &'static str) -> Result<&'a [u8]> {
        let end = self
            .at
            .checked_add(n)
            .ok_or(FormatError::Truncated { field })?;
        let slice = self
            .bytes
            .get(self.at..end)
            .ok_or(FormatError::Truncated { field })?;
        self.at = end;
        Ok(slice)
    }

    fn u32(&mut self, field: &'static str) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4, field)?.try_into().unwrap()))
    }

    fn u64(&mut self, field: &'static str) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8, field)?.try_into().unwrap()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reward(seed: u8) -> StakeRewardRecord {
        StakeRewardRecord {
            stake_pubkey: [seed; 32],
            lamports: 1_234 + u64::from(seed),
            voter_pubkey: [seed.wrapping_add(1); 32],
            stake: 5_000_000_000,
            activation_epoch: 700,
            deactivation_epoch: u64::MAX,
            warmup_cooldown_rate: 0.25,
            credits_observed: 42,
        }
    }

    fn full() -> Checkpoint {
        let mut lanes = [0u16; LtHash::NUM_ELEMENTS];
        for (i, l) in lanes.iter_mut().enumerate() {
            *l = (i as u16).wrapping_mul(31).wrapping_add(5);
        }
        Checkpoint {
            slot: 349_056_000,
            capitalization: 603_764_711_791_976_464,
            roll: Some(RollState {
                lt_hash: LtHash(lanes),
                bank_hash: [7; 32],
            }),
            stake_keys: vec![[1; 32], [2; 32], [3; 32]],
            pending_partitions: vec![vec![reward(9), reward(10)], vec![], vec![reward(11)]],
        }
    }

    #[test]
    fn round_trips_a_full_checkpoint() {
        let c = full();
        assert_eq!(Checkpoint::decode(&c.encode()).unwrap(), c);
    }

    #[test]
    fn round_trips_with_no_roll_and_nothing_pending() {
        let c = Checkpoint {
            slot: 1,
            capitalization: 0,
            roll: None,
            stake_keys: vec![],
            pending_partitions: vec![],
        };
        assert_eq!(Checkpoint::decode(&c.encode()).unwrap(), c);
    }

    // An empty partition means that block's payout is done; losing the distinction
    // would re-pay or skip it.
    #[test]
    fn an_empty_partition_survives_as_an_empty_partition() {
        let c = Checkpoint {
            slot: 1,
            capitalization: 1,
            roll: None,
            stake_keys: vec![],
            pending_partitions: vec![vec![], vec![]],
        };
        let back = Checkpoint::decode(&c.encode()).unwrap();
        assert_eq!(back.pending_partitions.len(), 2);
        assert!(back.pending_partitions.iter().all(|p| p.is_empty()));
    }

    // Feeds the reward math, where a 6e-8 drift cost a day.
    #[test]
    fn the_warmup_rate_round_trips_bit_exactly() {
        let mut c = full();
        c.pending_partitions[0][0].warmup_cooldown_rate = 0.1 + 0.2;
        let back = Checkpoint::decode(&c.encode()).unwrap();
        assert_eq!(
            back.pending_partitions[0][0].warmup_cooldown_rate.to_bits(),
            (0.1f64 + 0.2f64).to_bits()
        );
    }

    #[test]
    fn a_future_version_is_refused_not_guessed_at() {
        let mut bytes = full().encode();
        bytes[8..12].copy_from_slice(&2u32.to_le_bytes());
        assert_eq!(
            Checkpoint::decode(&bytes),
            Err(FormatError::UnknownCheckpointVersion(2))
        );
    }

    #[test]
    fn a_different_account_format_is_refused() {
        let mut bytes = full().encode();
        bytes[12..16].copy_from_slice(&99u32.to_le_bytes());
        assert_eq!(
            Checkpoint::decode(&bytes),
            Err(FormatError::UnknownAccountFormat(99))
        );
    }

    #[test]
    fn foreign_bytes_are_refused() {
        assert_eq!(
            Checkpoint::decode(b"not a checkpoint"),
            Err(FormatError::BadMagic)
        );
        assert_eq!(
            Checkpoint::decode(&[]),
            Err(FormatError::Truncated { field: "magic" })
        );
    }

    // Never a short read that decodes to something.
    #[test]
    fn every_truncation_is_an_error() {
        let bytes = full().encode();
        for len in 0..bytes.len() {
            assert!(
                Checkpoint::decode(&bytes[..len]).is_err(),
                "decoded a checkpoint truncated to {len} bytes"
            );
        }
        assert!(Checkpoint::decode(&bytes).is_ok());
    }

    // Fails on the read, not on the allocation.
    #[test]
    fn an_absurd_count_fails_without_allocating_it() {
        let mut bytes = full().encode();
        let at = 8 + 4 + 4 + 8 + 8 + 1 + LtHash::NUM_BYTES + 32;
        bytes[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(
            Checkpoint::decode(&bytes),
            Err(FormatError::Truncated { field: "stake_key" })
        );
    }

    #[test]
    fn account_records_round_trip() {
        let owner = [9u8; 32];
        let bytes = encode_account(4242, 9_000, 7, true, &owner, &[1, 2, 3]);
        assert_eq!(
            decode_account(&bytes).unwrap(),
            AccountRecord {
                slot: 4242,
                lamports: 9_000,
                rent_epoch: 7,
                executable: true,
                owner,
                data: &[1, 2, 3],
            }
        );
    }

    #[test]
    fn an_empty_data_account_round_trips() {
        let bytes = encode_account(1, 2, 3, false, &[0; 32], &[]);
        assert_eq!(bytes.len(), ACCOUNT_HEAD_BYTES);
        let got = decode_account(&bytes).unwrap();
        assert!(got.data.is_empty());
        assert!(!got.executable);
    }

    #[test]
    fn a_short_account_record_is_an_error() {
        assert!(decode_account(&[0; ACCOUNT_HEAD_BYTES - 1]).is_err());
    }
}
