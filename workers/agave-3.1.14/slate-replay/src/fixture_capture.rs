//! Captures a slot fixture during a replay.
//!
//! Inputs are snapshotted before the slot executes; the file is written only once the slot's
//! bank hash is confirmed by a consensus vote, so a fixture cannot carry an unverified answer.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use slate_format::fixture::{Fixture, RewardInputsRecord};
use slate_hash::LtHash;
use solana_account::ReadableAccount;
use solana_hash::Hash;
use solana_pubkey::Pubkey;
use solana_svm_callback::TransactionProcessingCallback;

use crate::ReplayBank;
use crate::block::Block;

struct Pending {
    parent_bank_hash: Hash,
    parent_lt_hash: LtHash,
    block: Vec<u8>,
    accounts: Vec<([u8; 32], Vec<u8>)>,
    reward_inputs: Option<RewardInputsRecord>,
}

pub struct FixtureCapture {
    wanted: HashSet<u64>,
    dir: PathBuf,
    pending: HashMap<u64, Pending>,
    written: Vec<u64>,
}

impl FixtureCapture {
    pub fn new(wanted: HashSet<u64>, dir: impl AsRef<Path>) -> Self {
        Self {
            wanted,
            dir: dir.as_ref().to_path_buf(),
            pending: HashMap::new(),
            written: Vec::new(),
        }
    }

    pub fn written(&self) -> &[u64] {
        &self.written
    }

    /// Slots asked for whose vote never arrived, so nothing was written for them.
    pub fn unconfirmed(&self) -> Vec<u64> {
        let mut v: Vec<u64> = self.pending.keys().copied().collect();
        v.sort_unstable();
        v
    }

    /// Read the pre-state a fixture needs. Must run before the block executes.
    pub fn snapshot(&mut self, bank: &ReplayBank, block: &Block) {
        if !self.wanted.contains(&block.slot) || self.pending.contains_key(&block.slot) {
            return;
        }
        let (Some(parent_bank_hash), Some(parent_lt_hash)) =
            (bank.parent_bank_hash(), bank.parent_lt_hash())
        else {
            return;
        };
        let Ok(encoded_block) = bincode::serialize(block) else {
            return;
        };

        let mut keys = crate::block::footprint(std::slice::from_ref(block));
        keys.extend(agave_feature_set::FEATURE_NAMES.keys().copied());
        let programdata = crate::block::programdata_addresses(&keys);
        keys.extend(programdata);

        let mut sorted: Vec<Pubkey> = keys.into_iter().collect();
        sorted.sort_unstable();
        let mut accounts = Vec::with_capacity(sorted.len());
        for key in sorted {
            if let Some((account, slot)) = bank.get_account_shared_data(&key) {
                accounts.push((
                    key.to_bytes(),
                    slate_format::encode_account(
                        slot,
                        account.lamports(),
                        account.rent_epoch(),
                        account.executable(),
                        &account.owner().to_bytes(),
                        account.data(),
                    ),
                ));
            }
        }

        self.pending.insert(
            block.slot,
            Pending {
                parent_bank_hash,
                parent_lt_hash,
                block: encoded_block,
                accounts,
                reward_inputs: None,
            },
        );
    }

    /// Write the fixture for `slot`, but only if the engine and the vote already agree.
    pub fn confirm(&mut self, slot: u64, computed: Hash, vote: Hash) -> anyhow::Result<()> {
        let Some(p) = self.pending.remove(&slot) else {
            return Ok(());
        };
        if computed != vote {
            anyhow::bail!(
                "refusing to write fixture for slot {slot}: computed {computed}, vote {vote}"
            );
        }
        let fixture = Fixture {
            slot,
            parent_bank_hash: p.parent_bank_hash.to_bytes(),
            parent_lt_hash: p.parent_lt_hash,
            expected_bank_hash: vote.to_bytes(),
            block: p.block,
            accounts: p.accounts,
            reward_inputs: p.reward_inputs,
        };
        std::fs::create_dir_all(&self.dir)?;
        let path = self.dir.join(format!("slot-{slot}.slfix"));
        std::fs::write(&path, fixture.encode())?;
        eprintln!(
            "fixture: wrote {} ({} accounts, vote-confirmed)",
            path.display(),
            fixture.accounts.len()
        );
        self.written.push(slot);
        Ok(())
    }
}

/// Replay a fixture and return the bank hash it produces. No snapshot, no network: the 2 KiB
/// lt_hash stands in for the account universe the footprint does not carry.
pub fn replay_fixture(f: &Fixture) -> anyhow::Result<Hash> {
    let mut bank = ReplayBank::default();
    for (pubkey, record) in &f.accounts {
        let r = slate_format::decode_account(record)
            .map_err(|e| anyhow::anyhow!("fixture account: {e}"))?;
        bank.insert(
            Pubkey::new_from_array(*pubkey),
            solana_account::AccountSharedData::from(solana_account::Account {
                lamports: r.lamports,
                data: r.data.to_vec(),
                owner: Pubkey::new_from_array(r.owner),
                executable: r.executable,
                rent_epoch: r.rent_epoch,
            }),
            r.slot,
        );
    }
    bank.bootstrap_bankhash(
        f.parent_lt_hash.clone(),
        Hash::new_from_array(f.parent_bank_hash),
    );

    let block: Block = bincode::deserialize(&f.block)?;
    let epoch = crate::epoch_of(f.slot);
    let feature_set = crate::build_feature_set(&bank, f.slot);
    bank.set_feature_set(feature_set.clone());
    let mut replayer = crate::Replayer::new_with_feature_set(f.slot, epoch, feature_set);
    crate::register_builtins(&mut bank, &replayer.processor, replayer.feature_set());
    crate::compat::register_removed_builtins(
        &mut bank,
        &replayer.processor,
        replayer.feature_set(),
    );
    let replay = replayer.replay_range(&mut bank, std::slice::from_ref(&block));
    if let Some((slot, detail)) = replay.halt {
        anyhow::bail!("fixture slot {slot} halted: {detail:?}");
    }
    bank.parent_bank_hash()
        .ok_or_else(|| anyhow::anyhow!("no bank hash after replaying the fixture"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use slate_format::fixture::Fixture;

    fn capture_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("slate_fixcap_{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn bank_and_block() -> (ReplayBank, Block) {
        let mut bank = crate::fixture::cpi::seed_bank();
        bank.bootstrap_bankhash(LtHash([5u16; LtHash::NUM_ELEMENTS]), Hash::new_unique());
        (bank, crate::fixture::cpi::block())
    }

    /// The fixture derives its feature set from these, so the replay needs them present.
    fn seed_all_features(bank: &mut ReplayBank, slot: u64) {
        let data = bincode::serialize(&Some(0u64)).unwrap();
        for id in agave_feature_set::FEATURE_NAMES.keys() {
            bank.insert(
                *id,
                solana_account::AccountSharedData::from(solana_account::Account {
                    lamports: 1_000_000,
                    data: data.clone(),
                    owner: solana_sdk_ids::feature::id(),
                    executable: false,
                    rent_epoch: 0,
                }),
                slot,
            );
        }
    }

    #[test]
    fn a_confirmed_slot_is_written_and_decodes_back() {
        let (bank, block) = bank_and_block();
        let dir = capture_dir("confirmed");
        let mut cap = FixtureCapture::new(HashSet::from([block.slot]), &dir);
        cap.snapshot(&bank, &block);

        let h = Hash::new_unique();
        cap.confirm(block.slot, h, h).unwrap();

        assert_eq!(cap.written(), &[block.slot]);
        assert!(cap.unconfirmed().is_empty());
        let bytes = std::fs::read(dir.join(format!("slot-{}.slfix", block.slot))).unwrap();
        let f = Fixture::decode(&bytes).unwrap();
        assert_eq!(f.slot, block.slot);
        assert_eq!(f.expected_bank_hash, h.to_bytes());
        assert!(!f.accounts.is_empty(), "the pre-state must be captured");
        assert!(
            f.reward_inputs.is_none(),
            "an ordinary slot has no reward inputs"
        );
    }

    #[test]
    fn a_captured_fixture_replays_to_the_same_bank_hash() {
        let (mut bank, block) = bank_and_block();
        seed_all_features(&mut bank, block.slot - 1);
        let dir = capture_dir("roundtrip");
        let mut cap = FixtureCapture::new(HashSet::from([block.slot]), &dir);

        cap.snapshot(&bank, &block);

        let epoch = crate::epoch_of(block.slot);
        let feature_set = crate::build_feature_set(&bank, block.slot);
        bank.set_feature_set(feature_set.clone());
        let mut replayer = crate::Replayer::new_with_feature_set(block.slot, epoch, feature_set);
        crate::register_builtins(&mut bank, &replayer.processor, replayer.feature_set());
        crate::compat::register_removed_builtins(
            &mut bank,
            &replayer.processor,
            replayer.feature_set(),
        );
        let replay = replayer.replay_range(&mut bank, std::slice::from_ref(&block));
        assert!(
            replay.halt.is_none(),
            "source replay halted: {:?}",
            replay.halt
        );
        let expected = bank.parent_bank_hash().expect("a bank hash");

        cap.confirm(block.slot, expected, expected).unwrap();

        let bytes = std::fs::read(dir.join(format!("slot-{}.slfix", block.slot))).unwrap();
        let f = Fixture::decode(&bytes).unwrap();
        let got = replay_fixture(&f).expect("the fixture replays");

        assert_eq!(
            got, expected,
            "a fixture must carry everything the slot needs"
        );
    }

    #[test]
    fn a_mismatch_refuses_to_write_the_answer() {
        let (bank, block) = bank_and_block();
        let dir = capture_dir("mismatch");
        let mut cap = FixtureCapture::new(HashSet::from([block.slot]), &dir);
        cap.snapshot(&bank, &block);

        let err = cap
            .confirm(block.slot, Hash::new_unique(), Hash::new_unique())
            .unwrap_err();
        assert!(err.to_string().contains("refusing to write fixture"));
        assert!(cap.written().is_empty());
        assert!(!dir.join(format!("slot-{}.slfix", block.slot)).exists());
    }

    #[test]
    fn a_slot_never_confirmed_is_reported_not_written() {
        let (bank, block) = bank_and_block();
        let dir = capture_dir("unconfirmed");
        let mut cap = FixtureCapture::new(HashSet::from([block.slot]), &dir);
        cap.snapshot(&bank, &block);
        assert_eq!(cap.unconfirmed(), vec![block.slot]);
        assert!(cap.written().is_empty());
    }

    #[test]
    fn an_unrequested_slot_is_not_captured() {
        let (bank, block) = bank_and_block();
        let dir = capture_dir("unrequested");
        let mut cap = FixtureCapture::new(HashSet::from([block.slot + 1]), &dir);
        cap.snapshot(&bank, &block);
        assert!(cap.unconfirmed().is_empty());
    }
}
