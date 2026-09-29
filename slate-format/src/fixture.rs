//! Slot fixture: replay one slot and check its bank hash with no snapshot and no network.
//!
//! Sections are tag-length-value and a reader SKIPS unknown tags, so a new field is additive
//! and does not invalidate committed fixtures. Bump `FIXTURE_VERSION` only to break a reader.

use crate::{FormatError, Reader, Result};
use slate_hash::LtHash;

const MAGIC: &[u8; 8] = b"SLFIXT\0\0";
pub const FIXTURE_VERSION: u32 = 1;

pub mod tag {
    pub const SLOT: u16 = 1;
    pub const PARENT_BANK_HASH: u16 = 2;
    pub const PARENT_LT_HASH: u16 = 3;
    pub const EXPECTED_BANK_HASH: u16 = 4;
    pub const BLOCK: u16 = 5;
    pub const ACCOUNTS: u16 = 6;
    pub const REWARD_INPUTS: u16 = 7;
    pub const STAKE_DELEGATIONS: u16 = 8;
    pub const CAPITALIZATION: u16 = 9;
}

/// Reward-pass inputs a boundary slot needs and an ordinary slot does not. Scalars, not
/// agave types: this crate is read by binaries linking different agave majors.
#[derive(Debug, Clone, PartialEq)]
pub struct RewardInputsRecord {
    pub inflation_initial: f64,
    pub inflation_terminal: f64,
    pub inflation_taper: f64,
    pub inflation_foundation: f64,
    pub inflation_foundation_term: f64,
    pub capitalization: u64,
    pub slots_per_year: f64,
    pub vote_accounts: Vec<[u8; 32]>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Fixture {
    pub slot: u64,
    pub parent_bank_hash: [u8; 32],
    pub parent_lt_hash: LtHash,
    pub expected_bank_hash: [u8; 32],
    /// Opaque here; the worker owns the block encoding.
    pub block: Vec<u8>,
    /// `(pubkey, encode_account(..) bytes)`. Must include the feature accounts: the feature
    /// set is derived from them rather than stored, so there is one source of truth.
    pub accounts: Vec<([u8; 32], Vec<u8>)>,
    pub reward_inputs: Option<RewardInputsRecord>,
    /// agave's stake cache, which a scan of `accounts` cannot reproduce: at slot 349047024 a scan
    /// finds 1,100,650 delegated accounts against the cache's 1,097,015.
    pub stake_delegations: Vec<[u8; 32]>,
    /// Absent in fixtures captured before tag 9; freeze burns fees, so a bank starting at 0 underflows.
    pub capitalization: Option<u64>,
}

fn section(out: &mut Vec<u8>, tag: u16, payload: &[u8]) {
    out.extend_from_slice(&tag.to_le_bytes());
    out.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    out.extend_from_slice(payload);
}

impl Fixture {
    pub fn encode(&self) -> Vec<u8> {
        let mut accounts = Vec::new();
        accounts.extend_from_slice(&(self.accounts.len() as u32).to_le_bytes());
        for (pubkey, record) in &self.accounts {
            accounts.extend_from_slice(pubkey);
            accounts.extend_from_slice(&(record.len() as u32).to_le_bytes());
            accounts.extend_from_slice(record);
        }

        let mut sections = Vec::new();
        let mut count = 0u32;
        section(&mut sections, tag::SLOT, &self.slot.to_le_bytes());
        section(&mut sections, tag::PARENT_BANK_HASH, &self.parent_bank_hash);
        section(
            &mut sections,
            tag::PARENT_LT_HASH,
            &self.parent_lt_hash.to_bytes(),
        );
        section(
            &mut sections,
            tag::EXPECTED_BANK_HASH,
            &self.expected_bank_hash,
        );
        section(&mut sections, tag::BLOCK, &self.block);
        section(&mut sections, tag::ACCOUNTS, &accounts);
        count += 6;
        if let Some(r) = &self.reward_inputs {
            let mut p = Vec::new();
            p.extend_from_slice(&r.inflation_initial.to_le_bytes());
            p.extend_from_slice(&r.inflation_terminal.to_le_bytes());
            p.extend_from_slice(&r.inflation_taper.to_le_bytes());
            p.extend_from_slice(&r.inflation_foundation.to_le_bytes());
            p.extend_from_slice(&r.inflation_foundation_term.to_le_bytes());
            p.extend_from_slice(&r.capitalization.to_le_bytes());
            p.extend_from_slice(&r.slots_per_year.to_le_bytes());
            p.extend_from_slice(&(r.vote_accounts.len() as u32).to_le_bytes());
            for v in &r.vote_accounts {
                p.extend_from_slice(v);
            }
            section(&mut sections, tag::REWARD_INPUTS, &p);
            count += 1;
        }
        if !self.stake_delegations.is_empty() {
            let mut p = Vec::with_capacity(4 + self.stake_delegations.len() * 32);
            p.extend_from_slice(&(self.stake_delegations.len() as u32).to_le_bytes());
            for k in &self.stake_delegations {
                p.extend_from_slice(k);
            }
            section(&mut sections, tag::STAKE_DELEGATIONS, &p);
            count += 1;
        }
        if let Some(c) = self.capitalization {
            section(&mut sections, tag::CAPITALIZATION, &c.to_le_bytes());
            count += 1;
        }

        let mut out = Vec::with_capacity(20 + sections.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&FIXTURE_VERSION.to_le_bytes());
        out.extend_from_slice(&crate::ACCOUNT_FORMAT_VERSION.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(&sections);
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut r = Reader::new(bytes);
        if r.take(8, "fixture magic")? != MAGIC {
            return Err(FormatError::BadFixtureMagic);
        }
        let version = u32::from_le_bytes(r.take(4, "fixture version")?.try_into().unwrap());
        if version != FIXTURE_VERSION {
            return Err(FormatError::UnknownFixtureVersion(version));
        }
        let account_format = u32::from_le_bytes(r.take(4, "account format")?.try_into().unwrap());
        if account_format != crate::ACCOUNT_FORMAT_VERSION {
            return Err(FormatError::UnknownAccountFormat(account_format));
        }
        let count = u32::from_le_bytes(r.take(4, "section count")?.try_into().unwrap());

        let mut slot = None;
        let mut parent_bank_hash = None;
        let mut parent_lt_hash = None;
        let mut expected_bank_hash = None;
        let mut block = None;
        let mut accounts = None;
        let mut reward_inputs = None;
        let mut stake_delegations = Vec::new();
        let mut capitalization = None;

        for _ in 0..count {
            let tag = u16::from_le_bytes(r.take(2, "section tag")?.try_into().unwrap());
            let len = u64::from_le_bytes(r.take(8, "section len")?.try_into().unwrap());
            let len: usize = len.try_into().map_err(|_| FormatError::Truncated {
                field: "section len",
            })?;
            let payload = r.take(len, "section payload")?;
            match tag {
                tag::SLOT => slot = Some(u64::from_le_bytes(take_exact(payload, 8, "slot")?)),
                tag::PARENT_BANK_HASH => {
                    parent_bank_hash = Some(take_exact(payload, 32, "parent bank hash")?)
                }
                tag::PARENT_LT_HASH => {
                    let b: [u8; LtHash::NUM_BYTES] =
                        take_exact(payload, LtHash::NUM_BYTES, "parent lt_hash")?;
                    parent_lt_hash = Some(LtHash::from_bytes(&b));
                }
                tag::EXPECTED_BANK_HASH => {
                    expected_bank_hash = Some(take_exact(payload, 32, "expected bank hash")?)
                }
                tag::BLOCK => block = Some(payload.to_vec()),
                tag::ACCOUNTS => accounts = Some(decode_accounts(payload)?),
                tag::REWARD_INPUTS => reward_inputs = Some(decode_reward_inputs(payload)?),
                tag::STAKE_DELEGATIONS => {
                    stake_delegations = decode_pubkeys(payload, "stake delegation")?
                }
                tag::CAPITALIZATION => {
                    capitalization = Some(u64::from_le_bytes(take_exact(payload, 8, "capitalization")?))
                }
                _ => {}
            }
        }

        Ok(Self {
            slot: slot.ok_or(FormatError::MissingSection { tag: tag::SLOT })?,
            parent_bank_hash: parent_bank_hash.ok_or(FormatError::MissingSection {
                tag: tag::PARENT_BANK_HASH,
            })?,
            parent_lt_hash: parent_lt_hash.ok_or(FormatError::MissingSection {
                tag: tag::PARENT_LT_HASH,
            })?,
            expected_bank_hash: expected_bank_hash.ok_or(FormatError::MissingSection {
                tag: tag::EXPECTED_BANK_HASH,
            })?,
            block: block.ok_or(FormatError::MissingSection { tag: tag::BLOCK })?,
            accounts: accounts.ok_or(FormatError::MissingSection { tag: tag::ACCOUNTS })?,
            reward_inputs,
            stake_delegations,
            capitalization,
        })
    }
}

fn take_exact<const N: usize>(p: &[u8], n: usize, field: &'static str) -> Result<[u8; N]> {
    if p.len() != n {
        return Err(FormatError::Truncated { field });
    }
    Ok(p.try_into().unwrap())
}

fn decode_accounts(p: &[u8]) -> Result<Vec<([u8; 32], Vec<u8>)>> {
    let mut r = Reader::new(p);
    let n = u32::from_le_bytes(r.take(4, "account count")?.try_into().unwrap());
    let mut out = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let pubkey: [u8; 32] = r.take(32, "account pubkey")?.try_into().unwrap();
        let len = u32::from_le_bytes(r.take(4, "account len")?.try_into().unwrap());
        out.push((pubkey, r.take(len as usize, "account record")?.to_vec()));
    }
    Ok(out)
}

fn decode_pubkeys(p: &[u8], field: &'static str) -> Result<Vec<[u8; 32]>> {
    let mut r = Reader::new(p);
    let n = u32::from_le_bytes(r.take(4, field)?.try_into().unwrap());
    let mut out = Vec::with_capacity(n as usize);
    for _ in 0..n {
        out.push(r.take(32, field)?.try_into().unwrap());
    }
    Ok(out)
}

fn decode_reward_inputs(p: &[u8]) -> Result<RewardInputsRecord> {
    let mut r = Reader::new(p);
    let f = |r: &mut Reader, field: &'static str| -> Result<f64> {
        Ok(f64::from_le_bytes(r.take(8, field)?.try_into().unwrap()))
    };
    let inflation_initial = f(&mut r, "inflation initial")?;
    let inflation_terminal = f(&mut r, "inflation terminal")?;
    let inflation_taper = f(&mut r, "inflation taper")?;
    let inflation_foundation = f(&mut r, "inflation foundation")?;
    let inflation_foundation_term = f(&mut r, "inflation foundation term")?;
    let capitalization = u64::from_le_bytes(r.take(8, "capitalization")?.try_into().unwrap());
    let slots_per_year = f(&mut r, "slots per year")?;
    let n = u32::from_le_bytes(r.take(4, "vote account count")?.try_into().unwrap());
    let mut vote_accounts = Vec::with_capacity(n as usize);
    for _ in 0..n {
        vote_accounts.push(r.take(32, "vote account")?.try_into().unwrap());
    }
    Ok(RewardInputsRecord {
        inflation_initial,
        inflation_terminal,
        inflation_taper,
        inflation_foundation,
        inflation_foundation_term,
        capitalization,
        slots_per_year,
        vote_accounts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ordinary() -> Fixture {
        Fixture {
            slot: 349_047_030,
            parent_bank_hash: [7u8; 32],
            parent_lt_hash: LtHash([3u16; LtHash::NUM_ELEMENTS]),
            expected_bank_hash: [9u8; 32],
            block: b"bincoded block".to_vec(),
            accounts: vec![
                (
                    [1u8; 32],
                    crate::encode_account(5, 100, 0, false, &[2u8; 32], b"abc"),
                ),
                (
                    [4u8; 32],
                    crate::encode_account(5, 0, 1, true, &[6u8; 32], b""),
                ),
            ],
            reward_inputs: None,
            stake_delegations: Vec::new(),
        }
    }

    fn boundary() -> Fixture {
        let mut f = ordinary();
        f.reward_inputs = Some(RewardInputsRecord {
            inflation_initial: 0.08,
            inflation_terminal: 0.015,
            inflation_taper: 0.15,
            inflation_foundation: 0.0,
            inflation_foundation_term: 0.0,
            capitalization: 623_328_118_246_651_420,
            slots_per_year: 78_892_314.984,
            vote_accounts: vec![[11u8; 32], [12u8; 32]],
        });
        f.stake_delegations = vec![[21u8; 32], [22u8; 32], [23u8; 32]];
        f
    }

    #[test]
    fn an_ordinary_fixture_round_trips() {
        let f = ordinary();
        assert_eq!(Fixture::decode(&f.encode()).unwrap(), f);
    }

    #[test]
    fn a_boundary_fixture_round_trips_with_its_reward_inputs() {
        let f = boundary();
        let back = Fixture::decode(&f.encode()).unwrap();
        assert_eq!(back, f);
        assert_eq!(back.reward_inputs.unwrap().vote_accounts.len(), 2);
    }

    #[test]
    fn stake_delegations_survive_and_an_older_fixture_without_them_still_decodes() {
        let f = boundary();
        let back = Fixture::decode(&f.encode()).unwrap();
        assert_eq!(back.stake_delegations.len(), 3);

        let mut older = f.clone();
        older.stake_delegations.clear();
        let back = Fixture::decode(&older.encode()).unwrap();
        assert!(
            back.stake_delegations.is_empty(),
            "a fixture written before tag 8 existed must still read"
        );
        assert_eq!(back.reward_inputs, f.reward_inputs);
    }

    #[test]
    fn an_unknown_section_is_skipped_so_a_later_field_stays_additive() {
        let f = ordinary();
        let mut bytes = f.encode();
        let count = u32::from_le_bytes(bytes[16..20].try_into().unwrap());
        bytes[16..20].copy_from_slice(&(count + 1).to_le_bytes());
        section(&mut bytes, 4242, b"a field this build has never heard of");
        assert_eq!(Fixture::decode(&bytes).unwrap(), f);
    }

    #[test]
    fn a_checkpoint_is_not_mistaken_for_a_fixture() {
        assert_eq!(
            Fixture::decode(b"SLCKPT\0\0____________").unwrap_err(),
            FormatError::BadFixtureMagic
        );
    }

    #[test]
    fn a_future_version_is_refused_not_guessed() {
        let mut bytes = ordinary().encode();
        bytes[8..12].copy_from_slice(&(FIXTURE_VERSION + 1).to_le_bytes());
        assert_eq!(
            Fixture::decode(&bytes).unwrap_err(),
            FormatError::UnknownFixtureVersion(FIXTURE_VERSION + 1)
        );
    }

    #[test]
    fn a_missing_required_section_is_an_error() {
        let f = ordinary();
        let mut bytes = f.encode();
        let count = u32::from_le_bytes(bytes[16..20].try_into().unwrap());
        let mut rebuilt = bytes[..20].to_vec();
        rebuilt[16..20].copy_from_slice(&(count - 1).to_le_bytes());
        let mut r = Reader::new(&bytes[20..]);
        for _ in 0..count {
            let tag = u16::from_le_bytes(r.take(2, "t").unwrap().try_into().unwrap());
            let len = u64::from_le_bytes(r.take(8, "l").unwrap().try_into().unwrap()) as usize;
            let payload = r.take(len, "p").unwrap();
            if tag != tag::BLOCK {
                section(&mut rebuilt, tag, payload);
            }
        }
        bytes = rebuilt;
        assert_eq!(
            Fixture::decode(&bytes).unwrap_err(),
            FormatError::MissingSection { tag: tag::BLOCK }
        );
    }
}
