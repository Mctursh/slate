pub mod backfill;
pub mod bankhash;
pub mod block;
pub mod boundary;
pub mod compat;
pub mod source;
pub mod store;
use store::{AccountStore, MemStore};
pub mod fixture;
pub mod oracle;
pub mod persist;
pub mod rewards;
pub mod snapshot;

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, RwLock},
};

use agave_feature_set::FeatureSet;
use agave_syscalls::create_program_runtime_environment_v1;
use slate_hash::LtHash;
use solana_account::{Account, AccountSharedData, ReadableAccount, WritableAccount};
use solana_clock::Clock;
use solana_compute_budget_instruction::instructions_processor::process_compute_budget_instructions;
use solana_epoch_schedule::EpochSchedule;
use solana_fee_structure::FeeDetails;
use solana_hash::Hash;
use solana_instruction_error::InstructionError;
use solana_precompile_error::PrecompileError;
use solana_program_runtime::{
    execution_budget::SVMTransactionExecutionBudget,
    loaded_programs::{
        BlockRelation, ForkGraph, ProgramCacheEntry, ProgramRuntimeEnvironment,
        ProgramRuntimeEnvironments,
    },
};
use solana_pubkey::Pubkey;
use solana_rent::Rent;
use solana_slot_hashes::SlotHashes;
use solana_svm::{
    account_loader::CheckedTransactionDetails,
    rollback_accounts::RollbackAccounts,
    transaction_processing_result::{ProcessedTransaction, TransactionProcessingResult},
    transaction_processor::{
        ExecutionRecordingConfig, TransactionBatchProcessor, TransactionProcessingConfig,
        TransactionProcessingEnvironment,
    },
};
use solana_svm_callback::{InvokeContextCallback, TransactionProcessingCallback};
use solana_svm_feature_set::SVMFeatureSet;
use solana_svm_transaction::svm_message::SVMMessage;
use solana_sysvar_id::SysvarId;
use solana_transaction::sanitized::SanitizedTransaction;
use solana_transaction_error::TransactionError;

use crate::{
    bankhash::BankHashRoller,
    block::{Block, sanitize},
    oracle::reconcile,
};

pub struct SlateForkGraph;

impl ForkGraph for SlateForkGraph {
    fn relationship(&self, a: u64, b: u64) -> BlockRelation {
        // Single linear replay chain: earlier slot = ancestor, so the program cache sees deployed programs.
        match a.cmp(&b) {
            std::cmp::Ordering::Less => BlockRelation::Ancestor,
            std::cmp::Ordering::Equal => BlockRelation::Equal,
            std::cmp::Ordering::Greater => BlockRelation::Descendant,
        }
    }
}

// What a resume found in the checkpoint; the caller decides what still has to come
// from the snapshot manifest.
pub struct RestoredCheckpoint {
    pub slot: u64,
    // Zero = the checkpoint predates seeding, not a real supply.
    pub capitalization: u64,
    pub stake_keys: usize,
    pub pending_partitions: usize,
}

// Slate's stand-in for the validator's Bank: the account source the SVM reads and writes during replay.
pub struct ReplayBank {
    store: Box<dyn AccountStore>,
    // Tx-committed writes in commit order; setup writes (seeds/builtins/sysvars) deliberately not logged.
    writes: Vec<WriteRecord>,
    // Monotonic counter so same-slot writes to one account order correctly.
    write_version: u64,
    // Per-slot pre-write account values (None = didn't exist); drives the lattice-hash roll.
    slot_dirty: Option<HashMap<Pubkey, Option<AccountSharedData>>>,
    // Running lattice + bank hash; None for tests that don't need forward bank hashes.
    bankhash_roller: Option<BankHashRoller>,
    // Per-slot feature set, consulted by the precompile callbacks. Defaults to all-enabled so
    // fixtures keep working; a faithful replay overwrites it via set_feature_set once seeded.
    feature_set: FeatureSet,
    // Keys only; membership is re-read at the boundary, so a stale key cannot skew the set.
    stake_keys: HashSet<Pubkey>,
    // Manifest-derived reward inputs; None means no crossing can be processed.
    reward_inputs: Option<rewards::RewardInputs>,
    // Calculated at a boundary, consumed over the following blocks.
    pending_partitions: Vec<Vec<rewards::StakeReward>>,
    // Rolled forward per slot: the epoch reward pool reads it at the boundary, and the seed
    // slot's value is stale by every fee burned since.
    capitalization: u64,
}

impl Default for ReplayBank {
    fn default() -> Self {
        Self {
            store: Box::new(MemStore::default()),
            writes: Vec::new(),
            write_version: 0,
            slot_dirty: None,
            bankhash_roller: None,
            feature_set: FeatureSet::all_enabled(),
            stake_keys: HashSet::new(),
            reward_inputs: None,
            pending_partitions: Vec::new(),
            capitalization: 0,
        }
    }
}

#[derive(Clone)]
pub struct WriteRecord {
    pub slot: u64,
    pub write_version: u64,
    pub pubkey: Pubkey,
    pub account: AccountSharedData,
}

impl ReplayBank {
    // Unlike get_account_shared_data, counts zero-lamport tombstones.
    pub fn contains(&self, key: &Pubkey) -> bool {
        self.store.contains(key)
    }

    pub fn with_store(store: Box<dyn AccountStore>) -> Self {
        Self {
            store,
            ..Self::default()
        }
    }

    // The precompile callbacks read this. Set it after seeding, since build_feature_set derives the
    // set from the feature accounts the seed just supplied; the default is all-enabled, which is
    // right for fixtures and wrong for any range below a precompile's activation slot.
    pub fn set_feature_set(&mut self, feature_set: FeatureSet) {
        self.feature_set = feature_set;
    }

    pub fn set_stake_keys(&mut self, keys: HashSet<Pubkey>) {
        self.stake_keys = keys;
    }

    pub fn set_reward_inputs(&mut self, inputs: rewards::RewardInputs) {
        self.reward_inputs = Some(inputs);
    }

    // agave's stake_delegations. Sorted: the count drives num_partitions.
    pub fn stake_delegations(&self) -> Vec<(Pubkey, solana_stake_interface::state::Delegation)> {
        let mut out: Vec<_> = self
            .stake_keys
            .iter()
            .filter_map(|key| {
                let (account, _) = self.get_account_shared_data(key)?;
                Some((*key, stake_delegation(&account)?))
            })
            .collect();
        out.sort_unstable_by_key(|(key, _)| *key);
        out
    }

    // Flush buffered writes to disk (no-op for the in-memory store).
    pub fn flush(&mut self) {
        self.store.flush();
    }

    // Raw store access with no zero-lamport filter (unlike get_account_shared_data); the boundary diff handles dead accounts itself.
    pub fn store(&self) -> &dyn AccountStore {
        self.store.as_ref()
    }

    pub fn insert(&mut self, key: Pubkey, account: AccountSharedData, slot: u64) {
        // On first write this slot, capture the pre-slot value so the lattice can mix it out before mixing the new in.
        if self
            .slot_dirty
            .as_ref()
            .is_some_and(|d| !d.contains_key(&key))
        {
            let old = self.store.get(&key).map(|(a, _)| a);
            self.slot_dirty.as_mut().unwrap().insert(key, old);
        }
        self.store.put(key, account, slot);
    }

    // Log the write (for persistence) then apply it; unlike insert, which is for un-persisted setup.
    fn commit_write(&mut self, key: Pubkey, account: AccountSharedData, slot: u64) {
        use solana_account::ReadableAccount;
        if *account.owner() == solana_sdk_ids::stake::id() {
            self.stake_keys.insert(key);
        }
        self.write_version += 1;
        self.writes.push(WriteRecord {
            slot,
            write_version: self.write_version,
            pubkey: key,
            account: account.clone(),
        });
        self.insert(key, account, slot);
    }

    pub fn writes(&self) -> &[WriteRecord] {
        &self.writes
    }

    // Drain the write log; per-chunk draining keeps it from growing with range length (it holds account data).
    pub fn take_writes(&mut self) -> Vec<WriteRecord> {
        std::mem::take(&mut self.writes)
    }

    // Start recording the accounts a slot writes (with pre-slot values) for the lattice-hash roll.
    pub fn begin_slot(&mut self) {
        self.slot_dirty = Some(HashMap::new());
    }

    // (pubkey, pre-slot value, post-slot value) for accounts written since begin_slot; None pre = slot created it. Stops recording.
    pub fn take_slot_changes(
        &mut self,
    ) -> Vec<(Pubkey, Option<AccountSharedData>, AccountSharedData)> {
        self.slot_dirty
            .take()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(key, old)| self.store.get(&key).map(|(new, _)| (key, old, new)))
            .collect()
    }

    // Start the bank-hash roll from the snapshot manifest's lattice + bank hash at s_snap.
    pub fn bootstrap_bankhash(&mut self, lt_hash: LtHash, bank_hash: Hash) {
        self.bankhash_roller = Some(BankHashRoller::new(lt_hash, bank_hash));
    }

    // Last finalized slot's bank hash, prepended into SlotHashes for the next slot; None if the roll isn't active.
    pub fn parent_bank_hash(&self) -> Option<Hash> {
        self.bankhash_roller.as_ref().map(|r| r.bank_hash())
    }

    // Durably checkpoint at `slot`: accounts + everything a resume needs, in one atomic commit.
    pub fn checkpoint(&mut self, slot: u64) -> anyhow::Result<()> {
        let checkpoint = slate_format::Checkpoint {
            slot,
            capitalization: self.capitalization,
            roll: self
                .bankhash_roller
                .as_ref()
                .map(|r| slate_format::RollState {
                    lt_hash: r.lt_hash().clone(),
                    bank_hash: r.bank_hash().to_bytes(),
                }),
            stake_keys: self.stake_keys.iter().map(Pubkey::to_bytes).collect(),
            pending_partitions: self
                .pending_partitions
                .iter()
                .map(|p| p.iter().map(rewards::to_reward_record).collect())
                .collect(),
        };
        self.store.checkpoint_flush(&checkpoint.encode())
    }

    /// `Ok(None)` = no checkpoint. One that exists but can't be read is an error, not a
    /// silent fall back to manifest defaults, which is a resume that diverges quietly.
    pub fn restore_checkpoint(&mut self) -> anyhow::Result<Option<RestoredCheckpoint>> {
        let Some(blob) = self.store.read_checkpoint() else {
            return Ok(None);
        };
        let checkpoint = slate_format::Checkpoint::decode(&blob)?;

        self.capitalization = checkpoint.capitalization;
        self.stake_keys = checkpoint
            .stake_keys
            .iter()
            .map(|k| Pubkey::new_from_array(*k))
            .collect();
        self.pending_partitions = checkpoint
            .pending_partitions
            .iter()
            .map(|p| p.iter().map(rewards::from_reward_record).collect())
            .collect();
        // Absent = the original run had the bank-hash roll off; keep it off.
        if let Some(roll) = checkpoint.roll {
            self.bootstrap_bankhash(roll.lt_hash, Hash::new_from_array(roll.bank_hash));
        }

        Ok(Some(RestoredCheckpoint {
            slot: checkpoint.slot,
            capitalization: checkpoint.capitalization,
            stake_keys: self.stake_keys.len(),
            pending_partitions: self.pending_partitions.len(),
        }))
    }

    // Roll the lattice over this slot's changes and compute its bank hash; None (no-op) if the roll isn't active.
    pub fn finalize_slot_bankhash(
        &mut self,
        changes: &[(Pubkey, Option<AccountSharedData>, AccountSharedData)],
        signature_count: u64,
        blockhash: &Hash,
    ) -> Option<Hash> {
        use solana_account::ReadableAccount;
        // Same deltas the lattice rolls, so burned fees leave the supply as they do on-chain.
        let delta: i128 = changes
            .iter()
            .map(|(_, old, new)| {
                i128::from(new.lamports()) - old.as_ref().map_or(0, |a| i128::from(a.lamports()))
            })
            .sum();
        self.capitalization = u64::try_from(i128::from(self.capitalization) + delta)
            .expect("capitalization stays within u64");
        self.bankhash_roller
            .as_mut()
            .map(|r| r.roll_slot(changes, signature_count, blockhash))
    }

    // Mid-slot value: freeze folds the slot's deltas in later, but rewards read it before that.
    pub fn capitalization_now(&self) -> u64 {
        use solana_account::ReadableAccount;
        let Some(dirty) = self.slot_dirty.as_ref() else {
            return self.capitalization;
        };
        let delta: i128 = dirty
            .iter()
            .filter_map(|(key, old)| {
                let (new, _) = self.store.get(key)?;
                Some(
                    i128::from(new.lamports())
                        - old.as_ref().map_or(0, |a| i128::from(a.lamports())),
                )
            })
            .sum();
        u64::try_from(i128::from(self.capitalization) + delta)
            .expect("capitalization stays within u64")
    }

    pub fn set_capitalization(&mut self, capitalization: u64) {
        self.capitalization = capitalization;
    }

    pub fn capitalization(&self) -> u64 {
        self.capitalization
    }

    // Prepend (slot, bank_hash) to SlotHashes like the runtime's update_slot_hashes (newest-first, truncated to 512).
    pub fn roll_slot_hashes(&mut self, slot: u64, bank_hash: Hash) {
        let mut slot_hashes = self
            .get_account_shared_data(&SlotHashes::id())
            .and_then(|(account, _)| bincode::deserialize::<SlotHashes>(account.data()).ok())
            .unwrap_or_else(|| SlotHashes::new(&[]));
        slot_hashes.add(slot, bank_hash);
        self.set_sysvar_account(SlotHashes::id(), bincode::serialize(&slot_hashes).unwrap());
    }

    // Register a builtin: put its loader-owned account in the bank and hand its entrypoint to the processor's program cache.
    pub fn add_builtin(
        &mut self,
        processor: &TransactionBatchProcessor<SlateForkGraph>,
        program_id: Pubkey,
        name: &str,
        entry: ProgramCacheEntry,
    ) {
        // Only stub when the real on-chain account is absent: its data is the runtime name (21 bytes for system) vs solana_builtins' short name (14), and overwriting shifts every following VM input-region account by +8 (BPF u128 align), corrupting programs that persist raw pointers (Neon) and the bank hash.
        if !self.store.contains(&program_id) {
            let account = AccountSharedData::from(Account {
                lamports: 1,
                data: name.as_bytes().to_vec(),
                owner: solana_sdk_ids::native_loader::id(),
                executable: true,
                rent_epoch: 0,
            });
            self.insert(program_id, account, 0);
        }
        processor.add_builtin(program_id, entry);
    }

    // Build the sysvars this replay needs and insert them as accounts; the processor pulls them in via fill_missing_sysvar_cache_entries.
    pub fn configure_sysvars(&mut self, slot: u64, unix_timestamp: i64) {
        let epoch = epoch_of(slot);
        // Derive Clock from the snapshot's real Clock (epoch fields are constant within an epoch; only slot + timestamp advance); synthesize one only for tests.
        let clock = match self
            .get_account_shared_data(&Clock::id())
            .and_then(|(account, _)| bincode::deserialize::<Clock>(account.data()).ok())
        {
            Some(mut clock) => {
                clock.slot = slot;
                clock.unix_timestamp = unix_timestamp;
                // ...except across a boundary, which no in-epoch slot exercises.
                if clock.epoch != epoch {
                    clock.epoch = epoch;
                    clock.leader_schedule_epoch = epoch + 1;
                    clock.epoch_start_timestamp = unix_timestamp;
                }
                clock
            }
            None => Clock {
                slot,
                epoch,
                epoch_start_timestamp: unix_timestamp,
                leader_schedule_epoch: epoch,
                unix_timestamp,
            },
        };
        self.set_sysvar_account(Clock::id(), bincode::serialize(&clock).unwrap());
        // Rent/EpochSchedule are seeded from the snapshot; only synthesize when absent (tests), else it's a spurious lattice change.
        if self.get_account_shared_data(&Rent::id()).is_none() {
            self.set_sysvar_account(Rent::id(), bincode::serialize(&Rent::default()).unwrap());
        }
        if self.get_account_shared_data(&EpochSchedule::id()).is_none() {
            self.set_sysvar_account(
                EpochSchedule::id(),
                bincode::serialize(&EpochSchedule::without_warmup()).unwrap(),
            );
        }
        // SlotHashes is seeded from the snapshot and prepended per slot by roll_slot_hashes; only default to empty when absent.
        if self.get_account_shared_data(&SlotHashes::id()).is_none() {
            self.set_slot_hashes(&[]);
        }
        // Seeded from the snapshot, rolled at freeze; absent (tests) fill 150 placeholders, AdvanceNonceAccount errors if empty, and full size fixes the rent-exempt lamports the oracle checks.
        #[allow(deprecated)]
        if self
            .get_account_shared_data(&solana_sdk_ids::sysvar::recent_blockhashes::id())
            .is_none()
        {
            let placeholder = Hash::new_from_array([1u8; 32]);
            let recent = solana_sysvar::recent_blockhashes::RecentBlockhashes::from_iter(
                (0..150u64)
                    .map(|i| solana_sysvar::recent_blockhashes::IterItem(i, &placeholder, 5_000)),
            );
            self.set_sysvar_account(
                solana_sdk_ids::sysvar::recent_blockhashes::id(),
                bincode::serialize(&recent).unwrap(),
            );
        }
    }

    // Apply the runtime's freeze-time sysvar writes the lattice must include (SlotHistory + RecentBlockhashes); no-op for sysvars the snapshot didn't supply (tests).
    pub fn freeze_slot(&mut self, slot: u64, blockhash: Hash, fee_reward: Option<(Pubkey, u64)>) {
        // Leader fee credit: the runtime pays the leader 50% of fees at freeze; getBlock's "Fee" reward is the exact amount.
        if let Some((leader, lamports)) = fee_reward
            && let Some((mut account, _)) = self.get_account_shared_data(&leader)
        {
            account.set_lamports(account.lamports() + lamports);
            self.insert(leader, account, slot);
        }
        if let Some(mut history) = self
            .get_account_shared_data(&solana_sdk_ids::sysvar::slot_history::id())
            .and_then(|(account, _)| {
                bincode::deserialize::<solana_sysvar::slot_history::SlotHistory>(account.data())
                    .ok()
            })
        {
            history.add(slot);
            self.set_sysvar_account(
                solana_sdk_ids::sysvar::slot_history::id(),
                bincode::serialize(&history).unwrap(),
            );
        }
        // Incinerator burn (agave Bank::run_incinerator): lamports sent here are destroyed at freeze,
        // only when the account was written this slot. Must land before the lattice roll or the burned
        // lamports stay in the bank hash.
        let incinerator = solana_sdk_ids::incinerator::id();
        if self
            .slot_dirty
            .as_ref()
            .is_some_and(|dirty| dirty.contains_key(&incinerator))
        {
            self.insert(incinerator, AccountSharedData::default(), slot);
        }
        #[allow(deprecated)]
        if let Some(current) = self
            .get_account_shared_data(&solana_sdk_ids::sysvar::recent_blockhashes::id())
            .and_then(|(account, _)| {
                bincode::deserialize::<solana_sysvar::recent_blockhashes::RecentBlockhashes>(
                    account.data(),
                )
                .ok()
            })
        {
            // Newest-first, this slot's blockhash prepended, capped at 150 like the runtime; mainnet fee is fixed 5000.
            let entries: Vec<(Hash, u64)> = std::iter::once((blockhash, 5_000u64))
                .chain(
                    current
                        .iter()
                        .map(|e| (e.blockhash, e.fee_calculator.lamports_per_signature)),
                )
                .take(150)
                .collect();
            let recent = solana_sysvar::recent_blockhashes::RecentBlockhashes::from_iter(
                entries.iter().enumerate().map(|(i, (hash, lamports))| {
                    solana_sysvar::recent_blockhashes::IterItem(i as u64, hash, *lamports)
                }),
            );
            self.set_sysvar_account(
                solana_sdk_ids::sysvar::recent_blockhashes::id(),
                bincode::serialize(&recent).unwrap(),
            );
        }
    }

    // Valid-age blockhashes (RecentBlockhashes sysvar, capped 150); a tx whose recent_blockhash isn't here is durable-nonce or too old. Empty for test banks.
    pub fn recent_blockhashes(&self) -> std::collections::HashSet<Hash> {
        #[allow(deprecated)]
        self.get_account_shared_data(&solana_sdk_ids::sysvar::recent_blockhashes::id())
            .and_then(|(account, _)| {
                bincode::deserialize::<solana_sysvar::recent_blockhashes::RecentBlockhashes>(
                    account.data(),
                )
                .ok()
            })
            .map(|recent| recent.iter().map(|entry| entry.blockhash).collect())
            .unwrap_or_default()
    }

    // The blockhash an initialized nonce account stores (None if not one). A durable-nonce tx's recent_blockhash equals this exactly, exact, unlike the RecentBlockhashes window that stops one short of agave's age-150 validity and mis-routed normal txs.
    pub fn stored_durable_nonce(&self, address: &Pubkey) -> Option<Hash> {
        let (account, _) = self.get_account_shared_data(address)?;
        if *account.owner() != solana_sdk_ids::system_program::id() {
            return None;
        }
        match bincode::deserialize::<solana_nonce::versions::Versions>(account.data())
            .ok()?
            .state()
        {
            solana_nonce::state::State::Initialized(data) => Some(*data.durable_nonce.as_hash()),
            solana_nonce::state::State::Uninitialized => None,
        }
    }

    // Populate SlotHashes from (slot, hash) pairs (newest-first); vote txs read it to check the voted slot is real.
    pub fn set_slot_hashes(&mut self, entries: &[(u64, Hash)]) {
        self.set_sysvar_account(
            SlotHashes::id(),
            bincode::serialize(&SlotHashes::new(entries)).unwrap(),
        );
    }

    // The on-chain Rent, not Rent::default(): mainnet's differs from the default from epoch 943
    // (deprecate_rent_exemption_threshold), and again at 1028/1033, so a default-derived balance
    // would be lamports mainnet never wrote.
    pub fn rent(&self) -> Rent {
        self.get_account_shared_data(&Rent::id())
            .and_then(|(account, _)| bincode::deserialize::<Rent>(account.data()).ok())
            .unwrap_or_default()
    }

    pub fn minimum_balance(&self, data_len: usize) -> u64 {
        self.rent().minimum_balance(data_len)
    }

    pub fn set_sysvar_at(&mut self, id: Pubkey, data: Vec<u8>, slot: u64) {
        let lamports = self.minimum_balance(data.len());
        let account = AccountSharedData::from(Account {
            lamports,
            data,
            owner: solana_sdk_ids::sysvar::id(),
            executable: false,
            rent_epoch: 0,
        });
        self.insert(id, account, slot);
    }

    fn set_sysvar_account(&mut self, id: Pubkey, data: Vec<u8>) {
        // Sysvar accounts are rent-exempt for their exact size; a wrong balance would fail the oracle's balance check and halt the replay.
        let lamports = self.minimum_balance(data.len());
        let account = AccountSharedData::from(Account {
            lamports,
            data,
            owner: solana_sdk_ids::sysvar::id(),
            executable: false,
            rent_epoch: 0,
        });
        self.insert(id, account, 0);
    }
}

// Mainnet cleared the genesis warmup long before Slate's floor, so epochs are a fixed 432k slots.
pub const SLOTS_PER_EPOCH: u64 = 432_000;

pub fn epoch_of(slot: u64) -> u64 {
    slot / SLOTS_PER_EPOCH
}

// From agave's canonical BUILTINS so the set stays in sync. Stake/Config/ALT are absent, migrated to Core BPF, so they run as ordinary loaded programs.
pub fn register_builtins(
    bank: &mut ReplayBank,
    processor: &TransactionBatchProcessor<SlateForkGraph>,
    feature_set: &FeatureSet,
) {
    for builtin in solana_builtins::BUILTINS {
        if let Some(feature_id) = builtin.enable_feature_id
            && !feature_set.is_active(&feature_id)
        {
            continue; // gated behind a feature not yet active at this slot: it did not exist here
        }
        bank.add_builtin(
            processor,
            builtin.program_id,
            builtin.name,
            ProgramCacheEntry::new_builtin(0, builtin.name.len(), builtin.entrypoint),
        );
    }
}

// A feature account's activation slot, or None (wrong owner/too small/unparsable/inactive). Feature is a lone Option<u64>, so it decodes straight as one.
// Outer None = not a feature account; inner None = a feature account still pending.
fn read_feature(account: &AccountSharedData) -> Option<Option<u64>> {
    use solana_account::ReadableAccount;
    // 9 == Feature::size_of() (1-byte Option tag + u64).
    if *account.owner() != solana_sdk_ids::feature::id() || account.data().len() < 9 {
        return None;
    }
    bincode::deserialize::<Option<u64>>(account.data()).ok()
}

fn feature_activation(account: &AccountSharedData) -> Option<u64> {
    read_feature(account).flatten()
}

// agave's apply_feature_activations: a pending feature activates here and the slot is written back.
// In place like agave's to_account, so data length and rent padding survive into the bank hash.
pub fn activate_pending_features(bank: &mut ReplayBank, slot: u64) -> Vec<Pubkey> {
    use solana_account::WritableAccount;
    let mut activated = Vec::new();
    for feature_id in agave_feature_set::FEATURE_NAMES.keys() {
        let Some((mut account, _)) = bank.get_account_shared_data(feature_id) else {
            continue;
        };
        if read_feature(&account) != Some(None) {
            continue;
        }
        if bincode::serialize_into(account.data_as_mut_slice(), &Some(slot)).is_ok() {
            bank.insert(*feature_id, account, slot);
            activated.push(*feature_id);
        }
    }
    activated
}

// agave's StakesCache membership (stakes.rs check_and_store): stake-owned is not enough, it must be
// delegated. Counting Initialized-but-undelegated accounts would inflate the reward partition count.
pub fn stake_state_of(
    account: &AccountSharedData,
) -> Option<solana_stake_interface::state::StakeStateV2> {
    use solana_account::ReadableAccount;
    if account.lamports() == 0 || *account.owner() != solana_sdk_ids::stake::id() {
        return None;
    }
    bincode::deserialize(account.data()).ok()
}

pub fn stake_delegation(
    account: &AccountSharedData,
) -> Option<solana_stake_interface::state::Delegation> {
    stake_state_of(account)?.delegation()
}

// Build the feature set active at slot from on-chain feature accounts (not all_enabled), the exact set the runtime executed against; feature-gated behavior depends on it.
pub fn build_feature_set(bank: &ReplayBank, slot: u64) -> FeatureSet {
    let mut feature_set = FeatureSet::default(); // everything inactive to start
    for feature_id in agave_feature_set::FEATURE_NAMES.keys() {
        if let Some((account, _)) = bank.get_account_shared_data(feature_id)
            && let Some(activated_slot) = feature_activation(&account)
            && activated_slot <= slot
        {
            feature_set.activate(feature_id, activated_slot);
        }
    }
    feature_set
}

// Precompile verification wired to agave-precompiles, gated on the bank's per-slot feature set.
// secp256k1 and ed25519 are always enabled; secp256r1 is behind enable_secp256r1_precompile, so a
// range before its activation must not resolve it as a precompile at all.
impl InvokeContextCallback for ReplayBank {
    fn is_precompile(&self, program_id: &Pubkey) -> bool {
        agave_precompiles::is_precompile(program_id, |id| self.feature_set.is_active(id))
    }

    fn process_precompile(
        &self,
        program_id: &Pubkey,
        data: &[u8],
        instruction_datas: Vec<&[u8]>,
    ) -> Result<(), PrecompileError> {
        match agave_precompiles::get_precompile(program_id, |id| self.feature_set.is_active(id)) {
            Some(precompile) => precompile.verify(data, &instruction_datas, &self.feature_set),
            None => Err(PrecompileError::InvalidPublicKey),
        }
    }
}

impl TransactionProcessingCallback for ReplayBank {
    fn get_account_shared_data(&self, pubkey: &Pubkey) -> Option<(AccountSharedData, u64)> {
        // Zero-lamport = dead: hand reads nothing (else stale bytes fail a later System Allocate "already in use"), but keep it in the map so the lattice can mix it out.
        self.store
            .get(pubkey)
            .filter(|(account, _)| account.lamports() > 0)
    }
}

// Owns the fork graph, the processor holds only a Weak, so someone must keep the Arc alive.
pub struct Replayer {
    _fork_graph: Arc<RwLock<SlateForkGraph>>,
    pub processor: TransactionBatchProcessor<SlateForkGraph>,
    // Runtime feature set, used to parse each tx's compute-budget limits.
    feature_set: FeatureSet,
    // SVM view of feature_set, held once for the per-batch environment.
    svm_feature_set: SVMFeatureSet,
}

fn program_runtime_v1(svm_feature_set: &SVMFeatureSet) -> ProgramRuntimeEnvironment {
    Arc::new(
        create_program_runtime_environment_v1(
            svm_feature_set,
            &SVMTransactionExecutionBudget::default(),
            false,
            false,
        )
        .expect("build v1 program runtime environment"),
    )
}

impl Replayer {
    // The exact per-slot feature set, used to gate compat shims (e.g. re-supplied removed builtins).
    pub fn feature_set(&self) -> &FeatureSet {
        &self.feature_set
    }

    // Processor with every feature enabled, fine for fixtures; a faithful replay uses new_with_feature_set.
    pub fn new(slot: u64, epoch: u64) -> Self {
        Self::new_with_feature_set(slot, epoch, FeatureSet::all_enabled())
    }

    // Build against an explicit per-slot feature_set; the VM syscall set/costs and the compute-budget parser key off it.
    pub fn new_with_feature_set(slot: u64, epoch: u64, feature_set: FeatureSet) -> Self {
        let fork_graph = Arc::new(RwLock::new(SlateForkGraph));
        // SVM env takes the derived SVMFeatureSet; the compute-budget parser takes the runtime FeatureSet.
        let svm_feature_set = feature_set.runtime_features();
        let processor = TransactionBatchProcessor::new(
            slot,
            epoch,
            Arc::downgrade(&fork_graph),
            Some(program_runtime_v1(&svm_feature_set)),
            None,
        );
        Self {
            _fork_graph: fork_graph,
            processor,
            feature_set,
            svm_feature_set,
        }
    }

    // Follow a feature set that changed at an epoch crossing. agave's own mechanism: the new
    // environment is staged as `upcoming` for that epoch, so earlier epochs keep the old one and
    // programs compiled against it stop matching on extraction and get recompiled.
    pub fn enter_epoch(&mut self, epoch: u64, feature_set: FeatureSet) {
        self.svm_feature_set = feature_set.runtime_features();
        self.feature_set = feature_set;
        let environments = ProgramRuntimeEnvironments {
            program_runtime_v1: program_runtime_v1(&self.svm_feature_set),
            program_runtime_v2: self.processor.environments.program_runtime_v2.clone(),
        };
        let mut prep = self.processor.epoch_boundary_preparation.write().unwrap();
        prep.upcoming_epoch = epoch;
        prep.upcoming_environments = Some(environments);
    }

    // Per-batch runtime settings; epoch_total_stake defaults to 0, fine for anything that doesn't touch stake.
    pub fn environment(
        &self,
        blockhash: Hash,
        epoch: u64,
        rent: Rent,
    ) -> TransactionProcessingEnvironment {
        TransactionProcessingEnvironment {
            blockhash,
            // non-zero so fees aren't disabled; the exact fee comes from the per-tx check results.
            blockhash_lamports_per_signature: 5_000,
            feature_set: self.svm_feature_set,
            program_runtime_environments_for_execution: self
                .processor
                .get_environments_for_epoch(epoch),
            program_runtime_environments_for_deployment: self
                .processor
                .get_environments_for_epoch(epoch),
            rent,
            ..Default::default()
        }
    }

    // Execute one sanitized tx; fee is the block meta's total, used to build the pre-validated check result.
    pub fn execute(
        &self,
        bank: &ReplayBank,
        tx: &SanitizedTransaction,
        fee: u64,
        epoch: u64,
        blockhash: Hash,
    ) -> TransactionProcessingResult {
        self.execute_with(&self.processor, bank, tx, fee, epoch, blockhash)
    }

    // A mid-range upgrade rewrites a program's bytecode but leaves the old compiled copy in the shared
    // cache; evict every written program account so the next invocation reloads from the account.
    pub fn invalidate_upgraded_programs(
        &self,
        changes: &[(Pubkey, Option<AccountSharedData>, AccountSharedData)],
    ) {
        let ids: Vec<Pubkey> = changes
            .iter()
            .filter(|(_, _, a)| {
                a.executable()
                    && (solana_sdk_ids::bpf_loader_upgradeable::check_id(a.owner())
                        || solana_sdk_ids::bpf_loader::check_id(a.owner())
                        || solana_sdk_ids::bpf_loader_deprecated::check_id(a.owner()))
            })
            .map(|(k, _, _)| *k)
            .collect();
        if !ids.is_empty() {
            self.processor
                .global_program_cache
                .write()
                .unwrap()
                .remove_programs(ids.into_iter());
        }
    }

    // Like execute but against an explicit processor (new_from gives a fresh sysvar cache while sharing the program cache/builtins).
    fn execute_with(
        &self,
        processor: &TransactionBatchProcessor<SlateForkGraph>,
        bank: &ReplayBank,
        tx: &SanitizedTransaction,
        fee: u64,
        epoch: u64,
        blockhash: Hash,
    ) -> TransactionProcessingResult {
        // Environment blockhash is the block's, not the tx's recent_blockhash, a durable nonce advances from the block's previousBlockhash; non-nonce txs don't observe it.
        let env = self.environment(blockhash, epoch, bank.rent());
        // Parse the tx's own compute-budget instructions for its real CU limit (not a default), so it exhausts/fails exactly as on chain.
        let check_result = match process_compute_budget_instructions(
            SVMMessage::program_instructions_iter(tx),
            &self.feature_set,
        ) {
            Ok(limits) => {
                let budget = limits.get_compute_budget_and_limits(
                    limits.loaded_accounts_bytes,
                    FeeDetails::new(fee, 0),
                    true,
                );
                // Durable-nonce only when recent_blockhash IS the account's stored nonce (compare directly, not via the 150-entry RecentBlockhashes window that's one short of agave's age-150 check and mis-routed normal txs). On the nonce path a failed tx still rolls the nonce forward advanced.
                let nonce = match tx.get_durable_nonce().copied() {
                    Some(address)
                        if bank.stored_durable_nonce(&address).as_ref()
                            == Some(tx.message().recent_blockhash()) =>
                    {
                        Some(address)
                    }
                    _ => None,
                };
                Ok(CheckedTransactionDetails::new(nonce, budget))
            }
            Err(err) => Err(err),
        };
        // Record program logs to diagnose divergences against getBlock logMessages; CPI/return-data recording stay off.
        let config = TransactionProcessingConfig {
            recording_config: ExecutionRecordingConfig {
                enable_log_recording: true,
                enable_cpi_recording: false,
                enable_return_data_recording: false,
                enable_transaction_balance_recording: false,
            },
            ..Default::default()
        };
        let mut output = processor.load_and_execute_sanitized_transactions(
            bank,
            std::slice::from_ref(tx),
            vec![check_result],
            &env,
            &config,
        );
        output.processing_results.remove(0)
    }

    // Replay every tx in block in order, committing successes so the next tx sees them; stops at the first tx that can't replay or diverges. bank must be seeded with builtins registered.
    pub fn replay_block(
        &mut self,
        bank: &mut ReplayBank,
        block: &Block,
        epoch: u64,
    ) -> BlockReplay {
        // Own the per-slot processor, as replay_range does: replay_block_with needs &mut self,
        // which rules out holding a borrow of self.processor across the call.
        let processor = self.processor.new_from(block.slot, epoch);
        self.replay_block_with(&processor, bank, block, epoch)
    }

    // Replay block against an explicit processor (see execute_with).
    fn replay_block_with(
        &mut self,
        processor: &TransactionBatchProcessor<SlateForkGraph>,
        bank: &mut ReplayBank,
        block: &Block,
        epoch: u64,
    ) -> BlockReplay {
        if let Ok(t) = std::env::var("SLATE_DUMP_FOOTPRINT")
            && t == block.slot.to_string()
            && let Ok(out) = std::env::var("SLATE_FOOTPRINT_OUT")
        {
            use solana_account::ReadableAccount;
            use std::io::Write as _;
            let mut set: std::collections::HashSet<Pubkey> = std::collections::HashSet::new();
            crate::block::extend_footprint(&mut set, std::slice::from_ref(block));
            crate::block::footprint_fixed(&mut set);
            // agave builds EpochStakes from the stakes cache, so rewards need these seeded.
            if std::env::var("SLATE_FOOTPRINT_STAKES").is_ok() {
                for (stake_pk, d) in bank.stake_delegations() {
                    set.insert(stake_pk);
                    set.insert(d.voter_pubkey);
                }
            }
            set.extend(crate::block::programdata_addresses(&set));
            set.insert(solana_sdk_ids::sysvar::slot_hashes::id());
            let mut f = std::io::BufWriter::new(std::fs::File::create(&out).expect("footprint out"));
            let mut n = 0usize;
            for pk in &set {
                if let Some((a, _)) = bank.get_account_shared_data(pk) {
                    let hex: String = a.data().iter().map(|b| format!("{b:02x}")).collect();
                    writeln!(
                        f,
                        "{pk} lam={} own={} exec={} dlen={} data={hex}",
                        a.lamports(),
                        a.owner(),
                        a.executable() as u8,
                        a.data().len()
                    )
                    .unwrap();
                    n += 1;
                }
            }
            f.flush().unwrap();
            eprintln!("FOOTPRINT slot {} wrote {n} of {} keys -> {out}", block.slot, set.len());
        }
        // With the roll active, record slot writes and prepend the parent's bank hash into SlotHashes (like the runtime) so votes read real recent history.
        let rolling = bank.parent_bank_hash().is_some();
        if rolling {
            bank.begin_slot();
        }
        // First BLOCK of the epoch, not the boundary slot, which can be skipped.
        if epoch_of(block.slot) != epoch_of(block.parent_slot) {
            if let Some(inputs) = bank.reward_inputs.clone() {
                // agave order: activate, then apply_builtin_program_feature_transitions, then
                // rewards. The migration burns and funds accounts, so it moves capitalization,
                // and validator_rewards is computed from capitalization. Running it after the
                // reward pass leaves every reward a hair low (epoch 823: 284 lamports).
                let activated_features = activate_pending_features(bank, block.slot);
                // Gate on the NEWLY activated set, not the feature set.
                compat::apply_core_bpf_migrations(
                    bank,
                    processor,
                    &self.processor,
                    &activated_features,
                    epoch,
                    block.slot,
                );
                let outcome = rewards::process_epoch_boundary(
                    bank,
                    &inputs,
                    epoch,
                    block.slot,
                    block.previous_blockhash,
                    block.block_height,
                    activated_features,
                );
                bank.pending_partitions = outcome.partitions;
            }
            // Read after the rewards pass, which is what activates this epoch's features.
            // Both the bank's gating and the SVM's program-runtime environment follow it.
            let feature_set = build_feature_set(bank, block.slot);
            self.enter_epoch(epoch, feature_set.clone());
            bank.set_feature_set(feature_set);
        }
        rewards::distribute_due_partition(bank, block.block_height, block.slot);
        bank.configure_sysvars(block.slot, block.block_time);
        if let Some(parent_hash) = bank.parent_bank_hash() {
            bank.roll_slot_hashes(block.parent_slot, parent_hash);
        }
        processor.fill_missing_sysvar_cache_entries(bank);

        for (i, block_tx) in block.transactions.iter().enumerate() {
            let tx = match sanitize(&block_tx.transaction, &block_tx.meta.loaded_addresses) {
                Ok(tx) => tx,
                Err(err) => return BlockReplay::halted(i, format!("cannot sanitize: {err}")),
            };
            let account_keys: Vec<Pubkey> = tx.message().account_keys().iter().copied().collect();

            let mut result = self.execute_with(
                processor,
                bank,
                &tx,
                block_tx.meta.fee,
                epoch,
                block.previous_blockhash,
            );
            // SIMD-0162 compat: the 3.1.x SVM permanently dropped the "no modifying an executable account" check, but slots before the feature activated still enforced it, re-supply it, marking the tx failed (ExecutableLamportChange) so commit rolls it back to fees-only.
            if !self
                .feature_set
                .is_active(&agave_feature_set::remove_accounts_executable_flag_checks::id())
                && let Ok(ProcessedTransaction::Executed(executed)) = &mut result
                && executed.was_successful()
                && let Some(idx) =
                    executable_modification(bank, &tx, &executed.loaded_transaction.accounts)
            {
                executed.execution_details.status = Err(TransactionError::InstructionError(
                    idx as u8,
                    InstructionError::ExecutableLamportChange,
                ));
            }
            let reconciliation = reconcile(&account_keys, &block_tx.meta, &result);
            if !reconciliation.matched() {
                return BlockReplay::halted(i, reconciliation.issues.join("; "));
            }

            // A deploy or upgrade hands back cache entries whose effective_slot is deployment_slot+1,
            // so the program is not yet visible for the rest of this slot. Merging them on commit (as
            // the runtime does) makes later transactions in the same block fail as "not deployed"
            // instead of running the superseded bytecode.
            if let Ok(ProcessedTransaction::Executed(executed)) = &result
                && executed.was_successful()
                && !executed.programs_modified_by_tx.is_empty()
            {
                // Per-epoch: across a boundary processor.environments is still the previous
                // epoch's, and a mis-tagged entry fails the ptr_eq check on extraction.
                processor.global_program_cache.write().unwrap().merge(
                    &processor.get_environments_for_epoch(epoch),
                    &executed.programs_modified_by_tx,
                );
            }
            commit_writes(bank, &tx, &result, block.slot);
        }

        // Apply freeze-time sysvar writes, then roll the lattice and compute this slot's bank hash (parent for the next slot's SlotHashes prepend).
        if rolling {
            bank.freeze_slot(block.slot, block.blockhash, block.fee_reward);
            let signature_count = block
                .transactions
                .iter()
                .map(|tx| tx.transaction.signatures.len() as u64)
                .sum();
            // Evict any program upgraded this slot; the same changes then roll into the lattice.
            let changes = bank.take_slot_changes();
            self.invalidate_upgraded_programs(&changes);
            if let Some(bank_hash) =
                bank.finalize_slot_bankhash(&changes, signature_count, &block.blockhash)
            {
                eprintln!("slot {} computed bank_hash {bank_hash}", block.slot);
            }
        }

        BlockReplay::complete(block.transactions.len())
    }

    // Replay a contiguous range in slot order against one bank that rolls forward; each block gets a fresh per-slot processor (new_from) sharing the program cache. Intra-epoch only, crossing an epoch needs machinery not built yet.
    pub fn replay_range(&mut self, bank: &mut ReplayBank, blocks: &[Block]) -> RangeReplay {
        // Self-verify against consensus: a vote carries the voted slot's bank hash, so later votes confirm earlier computed hashes. computed/confirmed pair them up (bounded to the ~30-slot vote lag); a mismatch means we diverged from a stake supermajority, halt.
        let first_slot = blocks.first().map_or(0, |b| b.slot);
        let mut computed: HashMap<u64, (Hash, usize)> = HashMap::new();
        let mut confirmed: HashMap<u64, Hash> = HashMap::new();
        let mut verified = 0usize;

        for (completed, block) in blocks.iter().enumerate() {
            // Harvest this block's votes; reconcile any slot we've already computed.
            for (slot, vote_hash) in crate::block::vote_confirmations(block) {
                if slot < first_slot {
                    continue; // a vote for a slot before the range; never computed here
                }
                match computed.remove(&slot) {
                    Some((got, idx)) if got != vote_hash => {
                        return RangeReplay {
                            blocks_completed: idx,
                            halt: Some((
                                slot,
                                BlockReplay::halted(
                                    0,
                                    format!(
                                        "bank-hash mismatch vs consensus vote: computed {got}, vote {vote_hash}"
                                    ),
                                ),
                            )),
                        };
                    }
                    Some(_) => verified += 1,
                    None => {
                        confirmed.entry(slot).or_insert(vote_hash);
                    }
                }
            }

            let epoch = epoch_of(block.slot);
            let processor = self.processor.new_from(block.slot, epoch);
            let block_replay = self.replay_block_with(&processor, bank, block, epoch);
            if !block_replay.is_complete() {
                return RangeReplay {
                    blocks_completed: completed,
                    halt: Some((block.slot, block_replay)),
                };
            }

            // Record the slot's computed hash; reconcile if its vote already arrived.
            if let Some(got) = bank.parent_bank_hash() {
                match confirmed.remove(&block.slot) {
                    Some(vote_hash) if got != vote_hash => {
                        return RangeReplay {
                            blocks_completed: completed,
                            halt: Some((
                                block.slot,
                                BlockReplay::halted(
                                    0,
                                    format!(
                                        "bank-hash mismatch vs consensus vote: computed {got}, vote {vote_hash}"
                                    ),
                                ),
                            )),
                        };
                    }
                    Some(_) => verified += 1,
                    None => {
                        computed.insert(block.slot, (got, completed));
                    }
                }
            }
        }

        // The tail (~30 slots) can't be confirmed here, their votes fall past the range end, so they're unverified, not wrong.
        let mut unconfirmed: Vec<u64> = computed.keys().copied().collect();
        unconfirmed.sort_unstable();
        if unconfirmed.is_empty() {
            eprintln!("hash-check: all {verified} replayed slots consensus-verified against votes");
        } else {
            eprintln!(
                "hash-check: {verified} slots consensus-verified, {} unconfirmed (votes past range end): {unconfirmed:?}",
                unconfirmed.len()
            );
        }

        // Flush buffered writes so the disk store's file is complete (no-op for the in-memory store).
        bank.flush();

        RangeReplay {
            blocks_completed: blocks.len(),
            halt: None,
        }
    }
}

// Re-supply SIMD-0162's removed check: flag a writable account that was already executable and came out changed (returns its index); skips freshly created accounts (a legit program deploy).
fn executable_modification(
    bank: &ReplayBank,
    message: &impl SVMMessage,
    accounts: &[(Pubkey, AccountSharedData)],
) -> Option<usize> {
    for (i, (key, post)) in accounts.iter().enumerate() {
        if !message.is_writable(i) {
            continue;
        }
        let Some((pre, _)) = bank.get_account_shared_data(key) else {
            continue;
        };
        if !pre.executable() {
            continue;
        }
        if pre.lamports() != post.lamports()
            || pre.owner() != post.owner()
            || pre.executable() != post.executable()
            || pre.data() != post.data()
        {
            return Some(i);
        }
    }
    None
}

// Commit a tx's account changes so later txs see them: success writes back only writable accounts; failed/fees-only commit just the fee-payer + advanced-nonce rollback.
fn commit_writes(
    bank: &mut ReplayBank,
    message: &impl SVMMessage,
    result: &TransactionProcessingResult,
    slot: u64,
) {
    match result {
        Ok(ProcessedTransaction::Executed(executed)) => {
            if executed.was_successful() {
                for (i, (key, account)) in executed.loaded_transaction.accounts.iter().enumerate() {
                    if message.is_writable(i) {
                        bank.commit_write(*key, account.clone(), slot);
                    }
                }
            } else {
                commit_rollback(bank, &executed.loaded_transaction.rollback_accounts, slot);
            }
        }
        Ok(ProcessedTransaction::FeesOnly(fees_only)) => {
            commit_rollback(bank, &fees_only.rollback_accounts, slot);
        }
        Err(_) => {}
    }
}

// Write a failed/fees-only tx's rollback accounts (charged fee payer + advanced nonce) back into the bank.
fn commit_rollback(bank: &mut ReplayBank, rollback: &RollbackAccounts, slot: u64) {
    for (address, account) in rollback {
        bank.commit_write(*address, account.clone(), slot);
    }
}

#[derive(Debug)]
pub struct BlockReplay {
    pub replayed: usize,
    pub halt: Option<Halt>,
}

#[derive(Debug)]
pub struct Halt {
    pub tx_index: usize,
    pub reason: String,
}

impl BlockReplay {
    fn complete(replayed: usize) -> Self {
        Self {
            replayed,
            halt: None,
        }
    }

    fn halted(tx_index: usize, reason: String) -> Self {
        Self {
            replayed: tx_index,
            halt: Some(Halt { tx_index, reason }),
        }
    }

    pub fn is_complete(&self) -> bool {
        self.halt.is_none()
    }
}

#[derive(Debug)]
pub struct RangeReplay {
    pub blocks_completed: usize,
    pub halt: Option<(u64, BlockReplay)>,
}

impl RangeReplay {
    pub fn is_complete(&self) -> bool {
        self.halt.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crossing_an_epoch_rolls_the_clock_epoch_fields() {
        use solana_account::ReadableAccount;
        let read = |b: &ReplayBank| {
            bincode::deserialize::<Clock>(b.get_account_shared_data(&Clock::id()).unwrap().0.data())
                .unwrap()
        };
        let mut bank = ReplayBank::default();

        bank.configure_sysvars(SLOTS_PER_EPOCH * 807 + 10, 1_000);
        let before = read(&bank);
        assert_eq!(before.epoch, 807);

        bank.configure_sysvars(SLOTS_PER_EPOCH * 807 + 20, 1_500);
        let within = read(&bank);
        assert_eq!(within.epoch, 807);
        assert_eq!(within.epoch_start_timestamp, before.epoch_start_timestamp);

        bank.configure_sysvars(SLOTS_PER_EPOCH * 808, 2_000);
        let crossed = read(&bank);
        assert_eq!(crossed.epoch, 808);
        assert_eq!(crossed.leader_schedule_epoch, 809);
        assert_eq!(crossed.epoch_start_timestamp, 2_000);
    }

    // The epoch-823 bug: a core-BPF migration burns and funds mid-slot, and validator_rewards is
    // computed from capitalization before the slot freezes. Reading the frozen value there left
    // every reward a hair low and broke the boundary bank hash.
    #[test]
    fn capitalization_now_includes_writes_made_in_the_open_slot() {
        let account = |lamports| {
            AccountSharedData::from(Account {
                lamports,
                ..Account::default()
            })
        };
        let mut bank = ReplayBank::default();
        let (burned, funded) = (Pubkey::new_unique(), Pubkey::new_unique());
        bank.insert(burned, account(700), 0);
        bank.set_capitalization(1_700);

        // Mid-slot: 700 burned, 900 funded, a net +200 agave would already have applied.
        bank.begin_slot();
        bank.insert(burned, account(0), 1);
        bank.insert(funded, account(900), 1);
        assert_eq!(
            bank.capitalization(),
            1_700,
            "the frozen value does not move until the slot ends"
        );
        assert_eq!(
            bank.capitalization_now(),
            1_900,
            "but a mid-slot reader must see the burn and the funding"
        );

        // And folding the slot in at freeze must not double-count them.
        let changes = bank.take_slot_changes();
        bank.finalize_slot_bankhash(&changes, 0, &Hash::default());
        assert_eq!(bank.capitalization(), 1_900);
        assert_eq!(bank.capitalization_now(), 1_900);
    }

    #[test]
    fn capitalization_follows_the_lamports_written_each_slot() {
        let account = |lamports| {
            AccountSharedData::from(Account {
                lamports,
                ..Account::default()
            })
        };
        let mut bank = ReplayBank::default();
        bank.set_capitalization(1_000);
        let key = Pubkey::new_unique();

        bank.finalize_slot_bankhash(&[(key, None, account(400))], 0, &Hash::default());
        assert_eq!(bank.capitalization(), 1_400);

        bank.finalize_slot_bankhash(
            &[(key, Some(account(400)), account(150))],
            0,
            &Hash::default(),
        );
        assert_eq!(bank.capitalization(), 1_150);
    }

    // One value, so a resume can only see a slot and capitalization written together.
    // The bug: committed separately, a crash between the two resumed with a supply that
    // already counted the chunk about to be replayed.
    #[test]
    fn a_checkpoint_restores_the_capitalization_of_its_own_slot() {
        let path = std::env::temp_dir().join("slate_checkpoint_is_one_value.redb");
        let _ = std::fs::remove_file(&path);

        {
            let mut disk = crate::store::DiskStore::create(&path, 16 * 1024 * 1024).unwrap();
            disk.set_checkpoint_mode(true);
            let mut bank = ReplayBank::with_store(Box::new(disk));
            bank.set_capitalization(1_000);
            bank.set_stake_keys(HashSet::from([Pubkey::new_from_array([4; 32])]));
            bank.checkpoint(100).unwrap();
            // Carry on past it; this work never gets a checkpoint of its own.
            bank.set_capitalization(2_000);
        } // drop closes the db, standing in for a crash

        let disk = crate::store::DiskStore::create(&path, 16 * 1024 * 1024).unwrap();
        let mut bank = ReplayBank::with_store(Box::new(disk));
        let restored = bank
            .restore_checkpoint()
            .unwrap()
            .expect("the checkpoint is there");

        assert_eq!(restored.slot, 100);
        assert_eq!(
            restored.capitalization, 1_000,
            "not the uncheckpointed 2_000"
        );
        assert_eq!(bank.capitalization(), 1_000);
        assert_eq!(restored.stake_keys, 1);

        let _ = std::fs::remove_file(&path);
    }

    // Must stop the resume, not fall back to defaults and diverge later.
    #[test]
    fn an_unreadable_checkpoint_is_an_error_not_a_silent_default() {
        let path = std::env::temp_dir().join("slate_checkpoint_unreadable.redb");
        let _ = std::fs::remove_file(&path);

        {
            let mut disk = crate::store::DiskStore::create(&path, 16 * 1024 * 1024).unwrap();
            disk.set_checkpoint_mode(true);
            disk.checkpoint_flush(b"written by some other build")
                .unwrap();
        }

        let disk = crate::store::DiskStore::create(&path, 16 * 1024 * 1024).unwrap();
        let mut bank = ReplayBank::with_store(Box::new(disk));
        assert!(bank.restore_checkpoint().is_err());

        let _ = std::fs::remove_file(&path);
    }

    // No checkpoint at all is not an error: it's a fresh run.
    #[test]
    fn no_checkpoint_restores_nothing() {
        let mut bank = ReplayBank::default();
        assert!(bank.restore_checkpoint().unwrap().is_none());
    }

    // The strong one: crossing into an epoch must produce the SAME environment as building a
    // replayer fresh at that epoch. new_with_feature_set is the path the 807->808 and
    // 50,079-slot mainnet proofs ran through, so equivalence carries that evidence over to the
    // crossing path. BuiltinProgram's PartialEq compares the VM config and the syscall registry,
    // which is exactly what a feature change moves.
    #[test]
    fn crossing_produces_the_same_environment_as_building_fresh() {
        use agave_feature_set::{
            enable_poseidon_syscall, enable_sbpf_v2_deployment_and_execution as sbpf_v2,
        };

        let epoch = 825u64;
        let first_slot = epoch * 432_000;
        let mut feature_set = FeatureSet::default();
        let mut crossed =
            Replayer::new_with_feature_set(first_slot - 1, epoch - 1, feature_set.clone());

        // One gate that moves the VM config, one that moves the syscall registry.
        feature_set.activate(&sbpf_v2::id(), first_slot);
        feature_set.activate(&enable_poseidon_syscall::id(), first_slot);
        crossed.enter_epoch(epoch, feature_set.clone());

        let fresh = Replayer::new_with_feature_set(first_slot, epoch, feature_set);

        assert!(
            crossed
                .processor
                .get_environments_for_epoch(epoch)
                .program_runtime_v1
                == fresh
                    .processor
                    .get_environments_for_epoch(epoch)
                    .program_runtime_v1,
            "VM config and syscall registry must match a fresh build"
        );
    }

    // The crossing must roll the SVM environment, not just the bank's gating. Without it a
    // range's tail executes against the environment its first slot was built with.
    #[test]
    fn crossing_an_epoch_rolls_the_svm_environment() {
        use agave_feature_set::enable_sbpf_v2_deployment_and_execution as sbpf_v2;

        let epoch = 825u64;
        let first_slot = epoch * 432_000;
        let mut bank = ReplayBank::default();
        // Already activated on chain at the crossing; build_feature_set reads it back.
        bank.insert(
            sbpf_v2::id(),
            feature_account(Some(first_slot), 0),
            first_slot,
        );

        let mut replayer = Replayer::new_with_feature_set(first_slot, epoch, FeatureSet::default());
        assert!(
            !replayer
                .svm_feature_set
                .enable_sbpf_v2_deployment_and_execution
        );
        let before = replayer.processor.get_environments_for_epoch(epoch);

        let block = block::Block {
            slot: first_slot,
            parent_slot: first_slot - 1, // previous epoch, so this is the crossing
            blockhash: Hash::default(),
            block_height: 0,
            previous_blockhash: Hash::default(),
            block_time: 1_700_000_000,
            transactions: vec![],
            fee_reward: None,
        };
        assert!(
            replayer
                .replay_block(&mut bank, &block, epoch)
                .is_complete()
        );

        assert!(
            replayer
                .svm_feature_set
                .enable_sbpf_v2_deployment_and_execution,
            "the crossing must pick up the feature"
        );
        assert!(
            !Arc::ptr_eq(
                &before.program_runtime_v1,
                &replayer
                    .processor
                    .get_environments_for_epoch(epoch)
                    .program_runtime_v1
            ),
            "and rebuild the program runtime environment for it"
        );
    }

    // The SVM environment has to follow a feature set that changed at a crossing, and only for
    // the new epoch: the previous one keeps the environment its slots actually executed against.
    #[test]
    fn entering_an_epoch_swaps_the_program_runtime_environment() {
        use agave_feature_set::enable_sbpf_v2_deployment_and_execution as sbpf_v2;

        let mut feature_set = FeatureSet::default();
        let mut replayer = Replayer::new_with_feature_set(0, 10, feature_set.clone());
        assert!(
            !replayer
                .svm_feature_set
                .enable_sbpf_v2_deployment_and_execution
        );
        let before = replayer.processor.get_environments_for_epoch(11);

        feature_set.activate(&sbpf_v2::id(), 11 * 432_000);
        replayer.enter_epoch(11, feature_set);

        assert!(
            replayer
                .svm_feature_set
                .enable_sbpf_v2_deployment_and_execution
        );
        let after = replayer.processor.get_environments_for_epoch(11);
        assert!(
            !Arc::ptr_eq(&before.program_runtime_v1, &after.program_runtime_v1),
            "epoch 11 must get the rebuilt environment"
        );
        assert!(
            Arc::ptr_eq(
                &replayer
                    .processor
                    .get_environments_for_epoch(10)
                    .program_runtime_v1,
                &before.program_runtime_v1
            ),
            "epoch 10 must keep the environment its slots ran against"
        );
    }

    #[test]
    fn skeleton_constructs() {
        // The harness + bank construct and link.
        let _bank = ReplayBank::default();
        let _replayer = Replayer::new(fixture::SLOT, fixture::SLOT / 432_000);
    }

    // 9 bytes of `Option<u64>`, plus optional rent padding.
    fn feature_account(activated_at: Option<u64>, pad: usize) -> AccountSharedData {
        let mut data = bincode::serialize(&activated_at).unwrap();
        data.resize(9 + pad, 0xAB);
        AccountSharedData::from(Account {
            lamports: 1_000_000,
            data,
            owner: solana_sdk_ids::feature::id(),
            executable: false,
            rent_epoch: 0,
        })
    }

    fn some_feature_id() -> Pubkey {
        *agave_feature_set::FEATURE_NAMES
            .keys()
            .next()
            .expect("agave ships feature ids")
    }

    fn stake_account_of(
        state: &solana_stake_interface::state::StakeStateV2,
        lamports: u64,
    ) -> AccountSharedData {
        let mut data = vec![0u8; 200];
        bincode::serialize_into(data.as_mut_slice(), state).unwrap();
        AccountSharedData::from(Account {
            lamports,
            data,
            owner: solana_sdk_ids::stake::id(),
            executable: false,
            rent_epoch: 0,
        })
    }

    fn delegated_state(voter: Pubkey) -> solana_stake_interface::state::StakeStateV2 {
        use solana_stake_interface::{
            stake_flags::StakeFlags,
            state::{Delegation, Meta, Stake, StakeStateV2},
        };
        StakeStateV2::Stake(
            Meta::default(),
            Stake {
                delegation: Delegation {
                    voter_pubkey: voter,
                    stake: 5_000_000_000,
                    ..Delegation::default()
                },
                credits_observed: 7,
            },
            StakeFlags::empty(),
        )
    }

    #[test]
    fn the_boundary_set_re_reads_state_so_a_stale_key_is_harmless() {
        let voter = Pubkey::new_unique();
        let (delegated, undelegated, closed) = (
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        );
        let mut bank = ReplayBank::default();
        bank.insert(
            delegated,
            stake_account_of(&delegated_state(voter), 5_000_000_000),
            0,
        );
        bank.insert(
            undelegated,
            stake_account_of(
                &solana_stake_interface::state::StakeStateV2::Initialized(Default::default()),
                5_000_000_000,
            ),
            0,
        );
        bank.insert(closed, stake_account_of(&delegated_state(voter), 0), 0);

        // Every key is tracked, including two that must not survive the re-read, plus one absent.
        bank.set_stake_keys(HashSet::from([
            delegated,
            undelegated,
            closed,
            Pubkey::new_unique(),
        ]));

        let set = bank.stake_delegations();
        assert_eq!(set.len(), 1, "only the delegated, funded account counts");
        assert_eq!(set[0].0, delegated);
        assert_eq!(set[0].1.voter_pubkey, voter);
    }

    #[test]
    fn the_boundary_set_is_deterministically_ordered() {
        let mut bank = ReplayBank::default();
        let mut keys: Vec<Pubkey> = (0..8).map(|_| Pubkey::new_unique()).collect();
        for key in &keys {
            bank.insert(
                *key,
                stake_account_of(&delegated_state(Pubkey::new_unique()), 5_000_000_000),
                0,
            );
        }
        bank.set_stake_keys(keys.iter().copied().collect());
        keys.sort_unstable();

        let got: Vec<Pubkey> = bank
            .stake_delegations()
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(got, keys);
    }

    #[test]
    fn a_transaction_write_adds_a_stake_key() {
        let key = Pubkey::new_unique();
        let mut bank = ReplayBank::default();
        assert!(bank.stake_delegations().is_empty());

        bank.commit_write(
            key,
            stake_account_of(&delegated_state(Pubkey::new_unique()), 5_000_000_000),
            10,
        );
        assert_eq!(
            bank.stake_delegations().len(),
            1,
            "a stake write must be tracked"
        );
    }

    #[test]
    fn a_delegated_stake_account_is_counted() {
        let voter = Pubkey::new_unique();
        let account = stake_account_of(&delegated_state(voter), 5_000_000_000);
        assert_eq!(stake_delegation(&account).unwrap().voter_pubkey, voter);
    }

    #[test]
    fn an_initialized_but_undelegated_stake_account_is_not_counted() {
        use solana_stake_interface::state::{Meta, StakeStateV2};
        let account = stake_account_of(&StakeStateV2::Initialized(Meta::default()), 5_000_000_000);
        assert!(stake_delegation(&account).is_none());
    }

    #[test]
    fn an_uninitialized_stake_account_is_not_counted() {
        use solana_stake_interface::state::StakeStateV2;
        let account = stake_account_of(&StakeStateV2::Uninitialized, 5_000_000_000);
        assert!(stake_delegation(&account).is_none());
    }

    #[test]
    fn a_closed_stake_account_is_not_counted() {
        let account = stake_account_of(&delegated_state(Pubkey::new_unique()), 0);
        assert!(stake_delegation(&account).is_none());
    }

    #[test]
    fn a_delegated_state_under_another_owner_is_not_counted() {
        use solana_account::WritableAccount;
        let mut account = stake_account_of(&delegated_state(Pubkey::new_unique()), 5_000_000_000);
        account.set_owner(solana_sdk_ids::system_program::id());
        assert!(stake_delegation(&account).is_none());
    }

    #[test]
    fn a_pending_feature_activates_at_the_boundary_slot() {
        let id = some_feature_id();
        let mut bank = ReplayBank::default();
        bank.insert(id, feature_account(None, 0), 0);

        assert_eq!(activate_pending_features(&mut bank, 500), vec![id]);

        let (account, _) = bank.get_account_shared_data(&id).unwrap();
        assert_eq!(feature_activation(&account), Some(500));
    }

    #[test]
    fn activation_preserves_data_length_and_padding() {
        use solana_account::ReadableAccount;
        let id = some_feature_id();
        let mut bank = ReplayBank::default();
        bank.insert(id, feature_account(None, 16), 0);

        activate_pending_features(&mut bank, 500);

        let (account, _) = bank.get_account_shared_data(&id).unwrap();
        assert_eq!(account.data().len(), 25, "length must not change");
        assert!(
            account.data()[9..].iter().all(|b| *b == 0xAB),
            "padding past the 9-byte Feature must survive untouched"
        );
        assert_eq!(feature_activation(&account), Some(500));
    }

    #[test]
    fn activation_is_recorded_for_the_lattice() {
        let id = some_feature_id();
        let mut bank = ReplayBank::default();
        bank.insert(id, feature_account(None, 0), 0);

        bank.begin_slot();
        activate_pending_features(&mut bank, 500);
        let changes = bank.take_slot_changes();

        assert!(
            changes.iter().any(|(pubkey, _, _)| *pubkey == id),
            "the activation write must be in the slot's dirty set"
        );
    }

    #[test]
    fn an_already_active_feature_is_left_alone() {
        let id = some_feature_id();
        let mut bank = ReplayBank::default();
        bank.insert(id, feature_account(Some(42), 0), 0);

        assert!(activate_pending_features(&mut bank, 500).is_empty());
        let (account, _) = bank.get_account_shared_data(&id).unwrap();
        assert_eq!(feature_activation(&account), Some(42), "slot must not move");
    }

    #[test]
    fn a_non_feature_account_at_a_feature_id_is_ignored() {
        let id = some_feature_id();
        let mut bank = ReplayBank::default();
        let mut wrong_owner = feature_account(None, 0);
        wrong_owner.set_owner(solana_sdk_ids::system_program::id());
        bank.insert(id, wrong_owner, 0);

        assert!(activate_pending_features(&mut bank, 500).is_empty());
    }

    #[test]
    fn registers_system_builtin() {
        use solana_account::ReadableAccount;
        let mut bank = fixture::seed_bank();
        let replayer = Replayer::new(fixture::SLOT, fixture::SLOT / 432_000);
        register_builtins(&mut bank, &replayer.processor, &FeatureSet::all_enabled());

        let (acct, _) = bank
            .get_account_shared_data(&solana_system_program::id())
            .expect("system program should be registered");
        assert!(acct.executable());
        assert_eq!(*acct.owner(), solana_sdk_ids::native_loader::id());
    }

    #[test]
    fn registers_the_full_builtin_set() {
        use solana_account::ReadableAccount;
        let mut bank = fixture::seed_bank();
        let replayer = Replayer::new(fixture::SLOT, fixture::SLOT / 432_000);
        register_builtins(&mut bank, &replayer.processor, &FeatureSet::all_enabled());

        // Every builtin agave lists is registered, executable, and native-owned.
        for builtin in solana_builtins::BUILTINS {
            let (acct, _) = bank
                .get_account_shared_data(&builtin.program_id)
                .unwrap_or_else(|| panic!("{} not registered", builtin.name));
            assert!(acct.executable(), "{} should be executable", builtin.name);
            assert_eq!(*acct.owner(), solana_sdk_ids::native_loader::id());
        }
        // Vote and loader-v4 specifically, loader-v4 was missing from the old hand-written set, so this pins the expansion.
        assert!(
            bank.get_account_shared_data(&solana_sdk_ids::vote::id())
                .is_some()
        );
        assert!(
            bank.get_account_shared_data(&solana_sdk_ids::loader_v4::id())
                .is_some()
        );
    }

    // A builtin behind an inactive feature did not exist at that slot, so registering it would let
    // transactions invoke a program the real cluster would have rejected. zk_elgamal is the gate that
    // matters in practice: it is the only one of the three that ever activated on mainnet (slot
    // 315_792_000), so every range before that must not have it.
    #[test]
    fn a_builtin_gated_by_an_inactive_feature_is_not_registered() {
        let gated = solana_sdk_ids::zk_elgamal_proof_program::id();
        let mut feature_set = FeatureSet::all_enabled();
        feature_set.deactivate(&agave_feature_set::zk_elgamal_proof_program_enabled::id());

        let mut bank = fixture::seed_bank();
        let replayer = Replayer::new(fixture::SLOT, fixture::SLOT / 432_000);
        register_builtins(&mut bank, &replayer.processor, &feature_set);

        let cache = replayer.processor.global_program_cache.read().unwrap();
        assert!(
            cache.get_slot_versions_for_tests(&gated).is_empty(),
            "zk_elgamal is gated off, so it must not reach the program cache"
        );
    }

    // The inverse, and the one that actually earns its keep: a gate that reads the wrong field skips
    // every builtin, which the test above would still pass. This one fails on it.
    #[test]
    fn an_ungated_builtin_survives_a_partial_feature_set() {
        let mut feature_set = FeatureSet::all_enabled();
        feature_set.deactivate(&agave_feature_set::zk_elgamal_proof_program_enabled::id());

        let mut bank = fixture::seed_bank();
        let replayer = Replayer::new(fixture::SLOT, fixture::SLOT / 432_000);
        register_builtins(&mut bank, &replayer.processor, &feature_set);

        let cache = replayer.processor.global_program_cache.read().unwrap();
        for builtin in solana_builtins::BUILTINS
            .iter()
            .filter(|b| b.enable_feature_id.is_none())
        {
            assert!(
                !cache
                    .get_slot_versions_for_tests(&builtin.program_id)
                    .is_empty(),
                "{} has no feature gate, deactivating an unrelated feature must not drop it",
                builtin.name
            );
        }
    }

    #[test]
    fn configures_slot_hashes_sysvar() {
        use solana_account::ReadableAccount;
        let mut bank = ReplayBank::default();
        let hash = Hash::new_unique();
        bank.set_slot_hashes(&[(7, hash)]);

        let (acct, _) = bank
            .get_account_shared_data(&SlotHashes::id())
            .expect("slot hashes sysvar present");
        assert_eq!(*acct.owner(), solana_sdk_ids::sysvar::id());
        let decoded: SlotHashes = bincode::deserialize(acct.data()).unwrap();
        assert_eq!(decoded, SlotHashes::new(&[(7, hash)]));
    }

    #[test]
    fn register_builtins_keeps_the_real_snapshot_builtin_account() {
        use solana_account::ReadableAccount;
        let system = solana_sdk_ids::system_program::id();

        // Real system_program data is the 21-byte runtime name "solana_system_program"; solana_builtins' 14-byte short name would shrink it by 8 (BPF u128 align) and shift following input-region accounts, the slot-030 +8 pointer bug.
        let mut bank = ReplayBank::default();
        bank.insert(
            system,
            AccountSharedData::from(Account {
                lamports: 1,
                data: b"solana_system_program".to_vec(),
                owner: solana_sdk_ids::native_loader::id(),
                executable: true,
                rent_epoch: 0,
            }),
            349_042_767,
        );

        let replayer = Replayer::new(0, 0);
        register_builtins(&mut bank, &replayer.processor, &FeatureSet::all_enabled());

        let (acct, _) = bank
            .get_account_shared_data(&system)
            .expect("system_program present");
        assert_eq!(
            acct.data(),
            b"solana_system_program",
            "register_builtins overwrote the real 21-byte snapshot account with a name-stub"
        );
    }

    #[test]
    fn builds_the_feature_set_from_accounts() {
        let slot = 1_000u64;
        let feature_program = solana_sdk_ids::feature::id();

        // Real feature ids; which ones don't matter, only that they're distinct.
        let ids: Vec<Pubkey> = agave_feature_set::FEATURE_NAMES
            .keys()
            .take(4)
            .copied()
            .collect();
        let (active, future, inactive, absent) = (ids[0], ids[1], ids[2], ids[3]);

        let feature_account = |activated_at: Option<u64>| {
            let mut data = vec![0u8; 9]; // Feature::size_of()
            bincode::serialize_into(&mut data[..], &activated_at).unwrap();
            AccountSharedData::from(Account {
                lamports: 1_000_000,
                data,
                owner: feature_program,
                executable: false,
                rent_epoch: 0,
            })
        };

        let mut bank = ReplayBank::default();
        bank.insert(active, feature_account(Some(500)), slot); // active: 500 <= 1000
        bank.insert(future, feature_account(Some(2_000)), slot); // future: 2000 > 1000
        bank.insert(inactive, feature_account(None), slot); // present but never activated
        // `absent` is never inserted.

        let fs = build_feature_set(&bank, slot);
        assert!(fs.is_active(&active), "activated at 500 <= slot 1000");
        assert_eq!(fs.activated_slot(&active), Some(500));
        assert!(
            !fs.is_active(&future),
            "activation slot 2000 is after slot 1000"
        );
        assert!(!fs.is_active(&inactive), "activated_at is None");
        assert!(!fs.is_active(&absent), "no feature account at all");
    }

    #[test]
    fn configures_sysvars() {
        use solana_account::ReadableAccount;
        let mut bank = fixture::seed_bank();
        bank.configure_sysvars(fixture::SLOT, fixture::BLOCK_TIME);

        // the Clock sysvar account exists and holds serialized data
        let (clock, _) = bank
            .get_account_shared_data(&Clock::id())
            .expect("clock sysvar present");
        assert!(!clock.data().is_empty());

        // and the processor can pull them into its cache without complaint
        let replayer = Replayer::new(fixture::SLOT, fixture::SLOT / 432_000);
        replayer.processor.fill_missing_sysvar_cache_entries(&bank);
    }

    // Rent::default() is 3480 / 2.0. Mainnet diverges from epoch 943
    // (deprecate_rent_exemption_threshold) and again at 1028/1033, so anything computing a
    // rent-exempt minimum has to read the bank's sysvar or it writes balances mainnet never had.
    #[test]
    // Builds mainnet's post-943 Rent by field; Rent::new_with_lamports_per_byte, which the
    // deprecation points at, only exists from solana-rent 4.x and this worker pins 3.1.0.
    #[allow(deprecated)]
    fn rent_comes_from_the_sysvar_not_the_default() {
        let mut bank = ReplayBank::default();
        assert_eq!(
            bank.rent(),
            Rent::default(),
            "absent sysvar falls back to default"
        );

        let post_943 = Rent {
            lamports_per_byte_year: 5080,
            exemption_threshold: 1.0,
            burn_percent: 50,
        };
        bank.set_sysvar_account(Rent::id(), bincode::serialize(&post_943).unwrap());

        assert_eq!(bank.rent(), post_943);
        assert_ne!(
            bank.minimum_balance(165),
            Rent::default().minimum_balance(165),
            "a post-943 rent must not produce default-derived balances"
        );
    }

    #[test]
    fn builds_environment() {
        let replayer = Replayer::new(fixture::SLOT, fixture::SLOT / 432_000);
        let env = replayer.environment(Hash::default(), fixture::SLOT / 432_000, Rent::default());
        assert_eq!(env.blockhash_lamports_per_signature, 5_000);
        assert_eq!(env.epoch_total_stake, 0);
    }

    #[test]
    fn executes_and_reconciles() {
        use solana_account::ReadableAccount;
        use solana_svm::transaction_processing_result::ProcessedTransaction;

        let epoch = fixture::SLOT / 432_000;

        let mut bank = fixture::seed_bank();
        let replayer = Replayer::new(fixture::SLOT, epoch);
        register_builtins(&mut bank, &replayer.processor, &FeatureSet::all_enabled());
        bank.configure_sysvars(fixture::SLOT, fixture::BLOCK_TIME);
        replayer.processor.fill_missing_sysvar_cache_entries(&bank);

        let result = replayer.execute(
            &bank,
            &fixture::sanitized_transaction(),
            fixture::FEE,
            epoch,
            Hash::default(),
        );

        let executed = match &result {
            Ok(ProcessedTransaction::Executed(e)) => e,
            other => panic!("expected an executed transaction, got {other:?}"),
        };
        assert!(
            executed.was_successful(),
            "tx should succeed on-chain and here"
        );
        assert_eq!(
            executed.execution_details.executed_units,
            fixture::COMPUTE_UNITS,
            "compute units"
        );
        assert_eq!(
            executed.loaded_transaction.fee_details.total_fee(),
            fixture::FEE,
            "fee"
        );

        let lamports = |pk: &Pubkey| {
            executed
                .loaded_transaction
                .accounts
                .iter()
                .find_map(|(k, a)| (k == pk).then(|| a.lamports()))
                .unwrap()
        };
        assert_eq!(
            lamports(&fixture::SENDER.parse().unwrap()),
            fixture::SENDER_POST,
            "sender post-balance"
        );
        assert_eq!(
            lamports(&fixture::RECIPIENT.parse().unwrap()),
            fixture::RECIPIENT_POST,
            "recipient post-balance"
        );
    }

    #[test]
    fn executes_memo_and_reconciles() {
        use solana_account::ReadableAccount;
        use solana_svm::transaction_processing_result::ProcessedTransaction;

        let m = fixture::memo::SLOT;
        let epoch = m / 432_000;

        let mut bank = fixture::memo::seed_bank();
        let replayer = Replayer::new(m, epoch);
        register_builtins(&mut bank, &replayer.processor, &FeatureSet::all_enabled());
        bank.configure_sysvars(m, fixture::memo::BLOCK_TIME);
        replayer.processor.fill_missing_sysvar_cache_entries(&bank);

        let result = replayer.execute(
            &bank,
            &fixture::memo::sanitized_transaction(),
            fixture::memo::FEE,
            epoch,
            Hash::default(),
        );

        let executed = match &result {
            Ok(ProcessedTransaction::Executed(e)) => e,
            other => panic!("expected an executed transaction, got {other:?}"),
        };
        assert!(executed.was_successful(), "memo tx should succeed");
        // the load-bearing check: running the Memo BPF program costs 11,295 CU.
        assert_eq!(
            executed.execution_details.executed_units,
            fixture::memo::COMPUTE_UNITS,
            "compute units (proves the BPF program executed)"
        );
        assert_eq!(
            executed.loaded_transaction.fee_details.total_fee(),
            fixture::memo::FEE,
            "fee"
        );
        let lamports = |pk: &Pubkey| {
            executed
                .loaded_transaction
                .accounts
                .iter()
                .find_map(|(k, a)| (k == pk).then(|| a.lamports()))
                .unwrap()
        };
        assert_eq!(
            lamports(&fixture::memo::SENDER.parse().unwrap()),
            fixture::memo::SENDER_POST,
            "sender"
        );
        assert_eq!(
            lamports(&fixture::memo::RECIPIENT.parse().unwrap()),
            fixture::memo::RECIPIENT_POST,
            "recipient"
        );
    }

    #[test]
    fn executes_cpi_and_reconciles() {
        use solana_account::ReadableAccount;
        use solana_svm::transaction_processing_result::ProcessedTransaction;

        let s = fixture::cpi::SLOT;
        let epoch = s / 432_000;

        let mut bank = fixture::cpi::seed_bank();
        let replayer = Replayer::new(s, epoch);
        register_builtins(&mut bank, &replayer.processor, &FeatureSet::all_enabled());
        bank.configure_sysvars(s, fixture::cpi::BLOCK_TIME);
        replayer.processor.fill_missing_sysvar_cache_entries(&bank);

        let result = replayer.execute(
            &bank,
            &fixture::cpi::sanitized_transaction(),
            fixture::cpi::FEE,
            epoch,
            Hash::default(),
        );

        let executed = match &result {
            Ok(ProcessedTransaction::Executed(e)) => e,
            other => panic!("expected an executed transaction, got {other:?}"),
        };
        assert!(executed.was_successful(), "CPI tx should succeed");
        let lamports = |pk: &Pubkey| {
            executed
                .loaded_transaction
                .accounts
                .iter()
                .find_map(|(k, a)| (k == pk).then(|| a.lamports()))
                .unwrap()
        };
        // The proof that matters: state reconstructs bit-exact through the CPI (inner System::transfer), and state is all Slate serves.
        assert_eq!(
            lamports(&fixture::cpi::PAYER.parse().unwrap()),
            fixture::cpi::PAYER_POST,
            "payer"
        );
        assert_eq!(
            lamports(&fixture::cpi::WALLET1.parse().unwrap()),
            fixture::cpi::WALLET1_POST,
            "wallet1 (CPI source)"
        );
        assert_eq!(
            lamports(&fixture::cpi::WALLET2.parse().unwrap()),
            fixture::cpi::WALLET2_POST,
            "wallet2 (CPI recipient)"
        );
        assert_eq!(
            executed.loaded_transaction.fee_details.total_fee(),
            fixture::cpi::FEE,
            "fee"
        );
        // CU does NOT reconcile here (replay 11_451 vs chain 11_343, ~1%): all_enabled() accounts a CPI cost differently than the exact per-epoch feature set. A CU-accounting gap, not a state error; we guard our number so drift surfaces.
        assert_eq!(
            executed.execution_details.executed_units,
            11_451,
            "replay CU (chain was {})",
            fixture::cpi::COMPUTE_UNITS
        );
    }

    // Lamports sent to the incinerator are destroyed at freeze. Leaving them on the account keeps
    // lamports in the lattice that consensus burned, which is what diverged slot 349276865.
    #[test]
    fn the_incinerator_is_burned_at_freeze() {
        use solana_account::{Account, ReadableAccount};

        let incinerator = solana_sdk_ids::incinerator::id();
        let mut bank = ReplayBank::default();
        bank.begin_slot();
        bank.insert(
            incinerator,
            AccountSharedData::from(Account {
                lamports: 6216,
                data: vec![],
                owner: solana_sdk_ids::system_program::id(),
                executable: false,
                rent_epoch: 0,
            }),
            9,
        );

        bank.freeze_slot(9, Hash::default(), None);

        let (account, _) = bank
            .store()
            .get(&incinerator)
            .expect("the burn stores a zeroed account, it does not drop the key");
        assert_eq!(
            account.lamports(),
            0,
            "freeze must destroy the incinerator balance, not carry it into the lattice"
        );
        assert!(account.data().is_empty());
    }

    // The burn is guarded on "written this slot" (the runtime's modified-since-parent). A balance
    // left from an earlier slot is not this slot's to destroy, and firing anyway would write an
    // account the real slot never touched.
    #[test]
    fn freeze_leaves_an_untouched_incinerator_alone() {
        use solana_account::{Account, ReadableAccount};

        let incinerator = solana_sdk_ids::incinerator::id();
        let mut bank = ReplayBank::default();
        // Seeded before begin_slot, so it is not recorded as written this slot.
        bank.insert(
            incinerator,
            AccountSharedData::from(Account {
                lamports: 4242,
                data: vec![],
                owner: solana_sdk_ids::system_program::id(),
                executable: false,
                rent_epoch: 0,
            }),
            8,
        );
        bank.begin_slot();

        bank.freeze_slot(9, Hash::default(), None);

        let (account, _) = bank.store().get(&incinerator).expect("still present");
        assert_eq!(
            account.lamports(),
            4242,
            "an incinerator the slot never wrote must be left untouched"
        );
    }

    // A program deployed or upgraded in a slot is not visible until the next one: the runtime loads
    // it with effective_slot = deployment_slot + 1, so invoking it in its own deploy slot must fail
    // as "not deployed" rather than run the bytecode. Chain enforced this at 349296581, where seven
    // transactions after a mid-block Raydium upgrade failed while Slate ran them.
    #[test]
    fn a_program_is_not_invokable_in_its_own_deploy_slot() {
        use solana_account::{Account, ReadableAccount};
        use solana_svm::transaction_processing_result::ProcessedTransaction;

        let deploy_slot = fixture::cpi::SLOT;
        let programdata: Pubkey = fixture::cpi::PROGRAMDATA.parse().unwrap();

        // programData is [variant u32][deployment slot u64][Option<authority>][ELF]; stamping the
        // slot is exactly what an upgrade does, and is what the loader reads to date the program.
        let run_at = |slot: u64| {
            let mut bank = fixture::cpi::seed_bank();
            let (pd, _) = bank.store().get(&programdata).unwrap();
            let mut bytes = pd.data().to_vec();
            bytes[4..12].copy_from_slice(&deploy_slot.to_le_bytes());
            let pd = AccountSharedData::from(Account {
                lamports: pd.lamports(),
                data: bytes,
                owner: *pd.owner(),
                executable: pd.executable(),
                rent_epoch: pd.rent_epoch(),
            });
            bank.insert(programdata, pd, deploy_slot);

            let epoch = slot / 432_000;
            let replayer = Replayer::new(slot, epoch);
            register_builtins(&mut bank, &replayer.processor, &FeatureSet::all_enabled());
            bank.configure_sysvars(slot, fixture::cpi::BLOCK_TIME);
            replayer.processor.fill_missing_sysvar_cache_entries(&bank);
            let tx = fixture::cpi::sanitized_transaction();
            replayer.execute(&bank, &tx, fixture::cpi::FEE, epoch, Hash::default())
        };

        let in_deploy_slot = run_at(deploy_slot);
        assert!(
            !matches!(&in_deploy_slot, Ok(ProcessedTransaction::Executed(e)) if e.was_successful()),
            "a program deployed this slot must not be invokable in it: {in_deploy_slot:?}"
        );

        // The slot after, it goes live. This half also proves the stamp landed on the slot field
        // rather than corrupting the header, which would fail both halves.
        let next_slot = run_at(deploy_slot + 1);
        assert!(
            matches!(&next_slot, Ok(ProcessedTransaction::Executed(e)) if e.was_successful()),
            "the program must become invokable the slot after deployment: {next_slot:?}"
        );
    }

    // A tx that deploys or upgrades hands back program-cache entries, and they have to reach the
    // shared cache or the rest of the block keeps invoking the superseded program. That is what ran
    // seven transactions at 349296581 which chain rejected. A tombstone stands in for what a real
    // upgrade hands back; what is under test is that merging reaches the cache execution reads.
    #[test]
    fn program_entries_modified_by_a_tx_reach_the_shared_cache() {
        use solana_program_runtime::loaded_programs::{
            ProgramCacheEntry, ProgramCacheEntryOwner, ProgramCacheEntryType,
        };
        use solana_svm::transaction_processing_result::ProcessedTransaction;
        use std::collections::HashMap;

        let s = fixture::cpi::SLOT;
        let epoch = s / 432_000;
        let mut bank = fixture::cpi::seed_bank();
        let replayer = Replayer::new(s, epoch);
        register_builtins(&mut bank, &replayer.processor, &FeatureSet::all_enabled());
        bank.configure_sysvars(s, fixture::cpi::BLOCK_TIME);
        replayer.processor.fill_missing_sysvar_cache_entries(&bank);

        let tx = fixture::cpi::sanitized_transaction();
        let program: Pubkey = fixture::cpi::PROGRAM.parse().unwrap();

        let warm = replayer.execute(&bank, &tx, fixture::cpi::FEE, epoch, Hash::default());
        assert!(
            matches!(&warm, Ok(ProcessedTransaction::Executed(e)) if e.was_successful()),
            "warm-up should succeed and cache the program: {warm:?}"
        );

        let modified: HashMap<Pubkey, std::sync::Arc<ProgramCacheEntry>> = HashMap::from([(
            program,
            std::sync::Arc::new(ProgramCacheEntry::new_tombstone(
                s,
                ProgramCacheEntryOwner::LoaderV3,
                ProgramCacheEntryType::Closed,
            )),
        )]);
        replayer
            .processor
            .global_program_cache
            .write()
            .unwrap()
            .merge(
                &replayer.processor.get_environments_for_epoch(epoch),
                &modified,
            );

        let after = replayer.execute(&bank, &tx, fixture::cpi::FEE, epoch, Hash::default());
        assert!(
            !matches!(&after, Ok(ProcessedTransaction::Executed(e)) if e.was_successful()),
            "entries merged from a tx must be visible to the cache execution reads, but the \
             superseded program still ran: {after:?}"
        );
    }

    // A program upgraded mid-range must run the new bytecode, not a stale copy from the shared cache.
    #[test]
    fn a_mid_range_program_upgrade_is_not_served_stale() {
        use solana_account::{Account, ReadableAccount};
        use solana_svm::transaction_processing_result::ProcessedTransaction;

        let s = fixture::cpi::SLOT;
        let epoch = s / 432_000;
        let mut bank = fixture::cpi::seed_bank();
        let replayer = Replayer::new(s, epoch);
        register_builtins(&mut bank, &replayer.processor, &FeatureSet::all_enabled());
        bank.configure_sysvars(s, fixture::cpi::BLOCK_TIME);
        replayer.processor.fill_missing_sysvar_cache_entries(&bank);

        let tx = fixture::cpi::sanitized_transaction();
        let program: Pubkey = fixture::cpi::PROGRAM.parse().unwrap();
        let programdata: Pubkey = fixture::cpi::PROGRAMDATA.parse().unwrap();
        let wallet2: Pubkey = fixture::cpi::WALLET2.parse().unwrap();

        let warm = replayer.execute(&bank, &tx, fixture::cpi::FEE, epoch, Hash::default());
        assert!(
            matches!(&warm, Ok(ProcessedTransaction::Executed(e)) if e.was_successful()),
            "warm-up CPI tx should succeed and cache v1, got {warm:?}"
        );

        // Upgrade it: swap the CPI bytecode for Memo (no transfer), via begin_slot so it's a recorded write.
        let (pd, _) = bank.store().get(&programdata).unwrap();
        let mut bytes = pd.data().to_vec();
        let elf_at = bytes
            .windows(4)
            .position(|w| w == [0x7f, b'E', b'L', b'F'])
            .expect("upgradeable programData carries an ELF");
        bytes.truncate(elf_at);
        bytes.extend_from_slice(fixture::memo::program_bytecode());
        let pd = AccountSharedData::from(Account {
            lamports: pd.lamports(),
            data: bytes,
            owner: *pd.owner(),
            executable: pd.executable(),
            rent_epoch: pd.rent_epoch(),
        });
        let (proxy, _) = bank.store().get(&program).unwrap();
        let upgrade_slot = s + 1;
        bank.begin_slot();
        bank.insert(programdata, pd, upgrade_slot);
        bank.insert(program, proxy, upgrade_slot);
        let changes = bank.take_slot_changes();
        replayer.invalidate_upgraded_programs(&changes);

        // Invoke again. execute() doesn't commit, so only the bytecode changed: a faithful replay runs
        // Memo (no transfer), the stale-cache bug re-runs the cached CPI and refunds WALLET2.
        let after = replayer.execute(&bank, &tx, fixture::cpi::FEE, epoch, Hash::default());
        let ran_stale_cpi = matches!(
            &after,
            Ok(ProcessedTransaction::Executed(e))
                if e.was_successful()
                    && e.loaded_transaction.accounts.iter()
                        .any(|(k, a)| k == &wallet2 && a.lamports() == fixture::cpi::WALLET2_POST)
        );
        assert!(
            !ran_stale_cpi,
            "after the programData was upgraded away from the CPI program, invoking it ran the \
             STALE cached CPI bytecode (transferred to WALLET2) instead of the new program: {after:?}"
        );
    }

    #[test]
    fn replays_a_block_and_commits() {
        use solana_account::ReadableAccount;

        let s = fixture::cpi::SLOT;
        let epoch = s / 432_000;
        let mut bank = fixture::cpi::seed_bank();
        let mut replayer = Replayer::new(s, epoch);
        register_builtins(&mut bank, &replayer.processor, &FeatureSet::all_enabled());

        let block = fixture::cpi::block();
        let outcome = replayer.replay_block(&mut bank, &block, epoch);

        assert!(
            outcome.is_complete(),
            "block should replay clean, halted: {:?}",
            outcome.halt
        );
        assert_eq!(outcome.replayed, 1);

        // The commit landed: the CPI recipient holds its post balance, so a following tx would read it.
        let (recipient, _) = bank
            .get_account_shared_data(&fixture::cpi::WALLET2.parse().unwrap())
            .expect("recipient present after commit");
        assert_eq!(recipient.lamports(), fixture::cpi::WALLET2_POST);
    }

    #[test]
    fn seeds_from_snapshot_and_replays_chained_transfers() {
        use solana_account::ReadableAccount;
        use solana_instruction::{AccountMeta, Instruction};
        use solana_message::{Message, VersionedMessage};
        use solana_signature::Signature;
        use solana_transaction::versioned::VersionedTransaction;

        // The real test-validator snapshot we developed the loader against.
        const SNAPSHOT: &[u8] = include_bytes!("test_snapshot.tar.zst");
        let system = solana_sdk_ids::system_program::id();

        // Fund from the richest dataless system-owned account in the snapshot.
        let accounts = snapshot::load_accounts(SNAPSHOT, None, None).unwrap();
        let (src, src_balance) = accounts
            .iter()
            .filter(|(_, (a, _))| *a.owner() == system && a.data().is_empty())
            .map(|(k, (a, _))| (*k, a.lamports()))
            .max_by_key(|&(_, bal)| bal)
            .expect("a funded system wallet in the snapshot");

        let mid = Pubkey::new_unique();
        let dst = Pubkey::new_unique();
        let (amount1, amount2, fee) = (2_000_000u64, 1_000_000u64, 5_000u64);

        let slot = 201; // just after the snapshot's slot (200)
        let epoch = slot / 432_000;
        let mut bank = snapshot::seed_bank_from_snapshot(SNAPSHOT, None).unwrap();
        let mut replayer = Replayer::new(slot, epoch);
        register_builtins(&mut bank, &replayer.processor, &FeatureSet::all_enabled());

        let transfer = |from: &Pubkey, to: &Pubkey, lamports: u64| -> VersionedTransaction {
            let mut data = vec![2u8, 0, 0, 0]; // SystemInstruction::Transfer discriminant
            data.extend_from_slice(&lamports.to_le_bytes());
            let ix = Instruction {
                program_id: system,
                accounts: vec![AccountMeta::new(*from, true), AccountMeta::new(*to, false)],
                data,
            };
            let message = Message::new_with_blockhash(&[ix], Some(from), &Hash::default());
            VersionedTransaction {
                signatures: vec![Signature::default()],
                message: VersionedMessage::Legacy(message),
            }
        };
        let meta = |pre: [u64; 3], post: [u64; 3]| block::TxMeta {
            err: None,
            fee,
            compute_units_consumed: 150,
            pre_balances: pre.to_vec(),
            post_balances: post.to_vec(),
            loaded_addresses: block::LoadedAddresses::default(),
            post_token_balances: vec![],
        };

        // src -> mid then mid -> dst: tx2's pre-balance only lines up if tx1 committed first, exercising tx-to-tx chaining.
        let block = block::Block {
            slot,
            parent_slot: slot - 1,
            blockhash: Hash::default(),
            block_height: 0,
            previous_blockhash: Hash::default(),
            block_time: 1_700_000_000,
            transactions: vec![
                block::BlockTx {
                    transaction: transfer(&src, &mid, amount1),
                    meta: meta(
                        [src_balance, 0, 1],
                        [src_balance - amount1 - fee, amount1, 1],
                    ),
                },
                block::BlockTx {
                    transaction: transfer(&mid, &dst, amount2),
                    meta: meta([amount1, 0, 1], [amount1 - amount2 - fee, amount2, 1]),
                },
            ],
            fee_reward: None,
        };

        let outcome = replayer.replay_block(&mut bank, &block, epoch);
        assert!(outcome.is_complete(), "block halted: {:?}", outcome.halt);
        assert_eq!(outcome.replayed, 2);

        let balance = |pk: &Pubkey| bank.get_account_shared_data(pk).unwrap().0.lamports();
        assert_eq!(balance(&src), src_balance - amount1 - fee);
        assert_eq!(balance(&mid), amount1 - amount2 - fee);
        assert_eq!(balance(&dst), amount2);
    }

    #[test]
    fn re_supplies_the_executable_account_check() {
        // SIMD-0162 compat: the 3.1.x SVM lets a transfer to an executable account succeed, but pre-activation the chain rejected it (ExecutableLamportChange); executable_modification re-supplies that check.
        use solana_instruction::{AccountMeta, Instruction};
        use solana_message::{Message, VersionedMessage};
        use solana_signature::Signature;
        use solana_transaction::versioned::VersionedTransaction;

        let system = solana_sdk_ids::system_program::id();
        let loader = solana_sdk_ids::native_loader::id();
        let payer = Pubkey::new_unique();
        let program = Pubkey::new_unique();

        // Pre-state: `program` is an existing executable account.
        let mut bank = ReplayBank::default();
        bank.insert(payer, AccountSharedData::new(5_000_000, 0, &system), 100);
        let mut pre_prog = AccountSharedData::new(1_000_000, 0, &loader);
        pre_prog.set_executable(true);
        bank.insert(program, pre_prog.clone(), 100);

        // transfer(payer -> program): account order is [payer (writable signer), program (writable), system (readonly)].
        let mut data = vec![2u8, 0, 0, 0]; // SystemInstruction::Transfer discriminant
        data.extend_from_slice(&500_000u64.to_le_bytes());
        let ix = Instruction {
            program_id: system,
            accounts: vec![
                AccountMeta::new(payer, true),
                AccountMeta::new(program, false),
            ],
            data,
        };
        let message = Message::new_with_blockhash(&[ix], Some(&payer), &Hash::default());
        let vtx = VersionedTransaction {
            signatures: vec![Signature::default()],
            message: VersionedMessage::Legacy(message),
        };
        let tx = sanitize(&vtx, &block::LoadedAddresses::default()).unwrap();

        let system_acct = AccountSharedData::new(1, 0, &loader);
        // Post-state where the transfer landed: program gained lamports, the violation, at account index 1.
        let mut post_prog = pre_prog.clone();
        post_prog.set_lamports(1_500_000);
        let changed = vec![
            (payer, AccountSharedData::new(4_495_000, 0, &system)),
            (program, post_prog),
            (system, system_acct.clone()),
        ];
        assert_eq!(
            executable_modification(&bank, &tx, &changed),
            Some(1),
            "a write to an existing executable account is flagged"
        );

        // Control: the executable account is untouched, nothing to flag.
        let untouched = vec![
            (payer, AccountSharedData::new(4_495_000, 0, &system)),
            (program, pre_prog.clone()),
            (system, system_acct.clone()),
        ];
        assert_eq!(
            executable_modification(&bank, &tx, &untouched),
            None,
            "an unchanged executable account is fine"
        );

        // Control: same change, but the account was never executable, allowed.
        bank.insert(program, AccountSharedData::new(1_000_000, 0, &system), 100);
        assert_eq!(
            executable_modification(&bank, &tx, &changed),
            None,
            "changing a non-executable account is allowed"
        );
    }

    #[test]
    fn honors_the_transactions_compute_unit_limit() {
        use solana_instruction::{AccountMeta, Instruction};
        use solana_message::{Message, VersionedMessage};
        use solana_signature::Signature;
        use solana_transaction::versioned::VersionedTransaction;

        let system = solana_sdk_ids::system_program::id();
        let compute_budget = solana_sdk_ids::compute_budget::id();
        let payer = Pubkey::new_unique();
        let dst = Pubkey::new_unique();

        // A transfer prefixed with an explicit SetComputeUnitLimit.
        let build = |cu_limit: u32| -> VersionedTransaction {
            let mut cu_data = vec![2u8]; // ComputeBudget SetComputeUnitLimit (1-byte disc)
            cu_data.extend_from_slice(&cu_limit.to_le_bytes());
            let cu_ix = Instruction {
                program_id: compute_budget,
                accounts: vec![],
                data: cu_data,
            };
            let mut tr_data = vec![2u8, 0, 0, 0]; // System Transfer (4-byte disc)
            tr_data.extend_from_slice(&1_000_000u64.to_le_bytes());
            let tr_ix = Instruction {
                program_id: system,
                accounts: vec![AccountMeta::new(payer, true), AccountMeta::new(dst, false)],
                data: tr_data,
            };
            let message =
                Message::new_with_blockhash(&[cu_ix, tr_ix], Some(&payer), &Hash::default());
            VersionedTransaction {
                signatures: vec![Signature::default()],
                message: VersionedMessage::Legacy(message),
            }
        };

        let succeeds = |cu_limit: u32| -> bool {
            let slot = 300;
            let epoch = slot / 432_000;
            let mut bank = ReplayBank::default();
            bank.insert(
                payer,
                AccountSharedData::from(Account {
                    lamports: 10_000_000,
                    data: vec![],
                    owner: system,
                    executable: false,
                    rent_epoch: 0,
                }),
                slot,
            );
            let replayer = Replayer::new(slot, epoch);
            register_builtins(&mut bank, &replayer.processor, &FeatureSet::all_enabled());
            let tx = sanitize(&build(cu_limit), &crate::block::LoadedAddresses::default()).unwrap();
            let result = replayer.execute(&bank, &tx, 5_000, epoch, Hash::default());
            matches!(&result, Ok(ProcessedTransaction::Executed(e)) if e.was_successful())
        };

        // A transfer needs ~150 CU: a tiny limit must exhaust and fail; ample succeeds.
        assert!(!succeeds(10), "10 CU can't fit a transfer, tx must fail");
        assert!(succeeds(50_000), "50k CU is ample, tx must succeed");
    }

    #[test]
    fn commits_the_fee_payer_rollback_for_a_failed_tx() {
        use solana_account::ReadableAccount;
        use solana_instruction::{AccountMeta, Instruction};
        use solana_message::{Message, VersionedMessage};
        use solana_signature::Signature;
        use solana_transaction::versioned::VersionedTransaction;

        let system = solana_sdk_ids::system_program::id();
        let payer = Pubkey::new_unique();
        let dst = Pubkey::new_unique();
        let slot = 300;
        let epoch = slot / 432_000;
        let fee = 5_000u64;
        let start = 1_000_000u64;

        let mut bank = ReplayBank::default();
        bank.insert(
            payer,
            AccountSharedData::from(Account {
                lamports: start,
                data: vec![],
                owner: system,
                executable: false,
                rent_epoch: 0,
            }),
            slot,
        );
        let replayer = Replayer::new(slot, epoch);
        register_builtins(&mut bank, &replayer.processor, &FeatureSet::all_enabled());

        // Transfer 10x the payer's balance: loads and executes, then fails on insufficient funds, fee charged, transfer rolled back.
        let mut data = vec![2u8, 0, 0, 0]; // System Transfer (4-byte disc)
        data.extend_from_slice(&(start * 10).to_le_bytes());
        let ix = Instruction {
            program_id: system,
            accounts: vec![AccountMeta::new(payer, true), AccountMeta::new(dst, false)],
            data,
        };
        let message = Message::new_with_blockhash(&[ix], Some(&payer), &Hash::default());
        let tx = sanitize(
            &VersionedTransaction {
                signatures: vec![Signature::default()],
                message: VersionedMessage::Legacy(message),
            },
            &crate::block::LoadedAddresses::default(),
        )
        .unwrap();

        let result = replayer.execute(&bank, &tx, fee, epoch, Hash::default());

        // Executed but not successful, the branch that used to commit nothing.
        assert!(
            matches!(&result, Ok(ProcessedTransaction::Executed(e)) if !e.was_successful()),
            "over-transfer should execute then fail, got {result:?}"
        );

        commit_writes(&mut bank, &tx, &result, slot);

        // Fee charged, transfer reverted: payer down exactly the fee, dst never funded.
        let (payer_acct, _) = bank
            .get_account_shared_data(&payer)
            .expect("payer still present");
        assert_eq!(
            payer_acct.lamports(),
            start - fee,
            "payer should be charged the fee and nothing else"
        );
        assert!(
            bank.get_account_shared_data(&dst).is_none(),
            "recipient must not exist, the transfer rolled back"
        );
    }

    #[test]
    fn advances_the_nonce_on_a_failed_durable_nonce_tx() {
        use solana_account::ReadableAccount;
        use solana_instruction::{AccountMeta, Instruction};
        use solana_message::{Message, VersionedMessage};
        use solana_nonce::{
            state::{DurableNonce, State},
            versions::Versions,
        };
        use solana_signature::Signature;
        use solana_transaction::versioned::VersionedTransaction;

        let system = solana_sdk_ids::system_program::id();
        let recent_blockhashes = solana_sdk_ids::sysvar::recent_blockhashes::id();
        let payer = Pubkey::new_unique(); // fee payer AND nonce authority
        let nonce_key = Pubkey::new_unique();
        let dst = Pubkey::new_unique();
        let slot = 300;
        let epoch = slot / 432_000;
        let fee = 5_000u64;
        let start = 1_000_000u64;

        // A nonce account holding a durable nonce derived from some old blockhash.
        let stored = DurableNonce::from_blockhash(&Hash::new_from_array([9u8; 32]));
        let nonce_data =
            bincode::serialize(&Versions::new(State::new_initialized(&payer, stored, fee)))
                .unwrap();

        let mut bank = ReplayBank::default();
        bank.insert(
            payer,
            AccountSharedData::from(Account {
                lamports: start,
                data: vec![],
                owner: system,
                executable: false,
                rent_epoch: 0,
            }),
            slot,
        );
        bank.insert(
            nonce_key,
            AccountSharedData::from(Account {
                lamports: 1_500_000,
                data: nonce_data,
                owner: system,
                executable: false,
                rent_epoch: 0,
            }),
            slot,
        );
        let replayer = Replayer::new(slot, epoch);
        register_builtins(&mut bank, &replayer.processor, &FeatureSet::all_enabled());
        // AdvanceNonceAccount reads RecentBlockhashes from the sysvar cache, so configure + pull it in first.
        bank.configure_sysvars(slot, 1_700_000_000);
        replayer.processor.fill_missing_sysvar_cache_entries(&bank);

        // Durable-nonce tx: advance the nonce then over-transfer so it fails. recent_blockhash IS the stored nonce, that's what makes it durable.
        let advance = Instruction {
            program_id: system,
            accounts: vec![
                AccountMeta::new(nonce_key, false),
                AccountMeta::new_readonly(recent_blockhashes, false),
                AccountMeta::new_readonly(payer, true),
            ],
            data: vec![4, 0, 0, 0], // AdvanceNonceAccount
        };
        let mut xfer = vec![2u8, 0, 0, 0]; // Transfer
        xfer.extend_from_slice(&(start * 10).to_le_bytes());
        let transfer = Instruction {
            program_id: system,
            accounts: vec![AccountMeta::new(payer, true), AccountMeta::new(dst, false)],
            data: xfer,
        };
        let message =
            Message::new_with_blockhash(&[advance, transfer], Some(&payer), stored.as_hash());
        let tx = sanitize(
            &VersionedTransaction {
                signatures: vec![Signature::default()],
                message: VersionedMessage::Legacy(message),
            },
            &crate::block::LoadedAddresses::default(),
        )
        .unwrap();

        // Block blockhash, what a durable nonce advances FROM; deliberately different from the tx's nonce so the test proves the advance uses the block's.
        let block_blockhash = Hash::new_from_array([7u8; 32]);
        let result = replayer.execute(&bank, &tx, fee, epoch, block_blockhash);
        assert!(
            matches!(&result, Ok(ProcessedTransaction::Executed(e)) if !e.was_successful()),
            "durable-nonce tx should execute then fail on the transfer, got {result:?}"
        );

        commit_writes(&mut bank, &tx, &result, slot);

        // The payoff: transfer failed but the nonce still advanced (a regular failed tx wouldn't), and the fee payer was charged.
        let (nonce_acct, _) = bank
            .get_account_shared_data(&nonce_key)
            .expect("nonce account present");
        let advanced: Versions = bincode::deserialize(nonce_acct.data()).unwrap();
        let State::Initialized(data) = advanced.state() else {
            panic!("nonce should still be initialized");
        };
        assert_ne!(
            data.durable_nonce, stored,
            "the nonce must advance even though the tx failed"
        );
        assert_eq!(
            data.durable_nonce,
            DurableNonce::from_blockhash(&block_blockhash),
            "the nonce advances from the block's blockhash, not the tx's nonce value"
        );
        let (payer_acct, _) = bank.get_account_shared_data(&payer).unwrap();
        assert_eq!(
            payer_acct.lamports(),
            start - fee,
            "fee payer charged the fee"
        );
    }

    #[test]
    fn a_normal_tx_topping_up_its_own_nonce_is_not_durable() {
        // Gap-#2 regression: an AdvanceNonce tx whose recent_blockhash is a REAL blockhash (not the stored nonce) is normal, so a failure rolls the nonce back. The old 150-entry RecentBlockhashes check mis-routed it (agave accepts age 150; the sysvar holds only 0-149); we now compare the stored nonce.
        use solana_account::ReadableAccount;
        use solana_instruction::{AccountMeta, Instruction};
        use solana_message::{Message, VersionedMessage};
        use solana_nonce::{
            state::{DurableNonce, State},
            versions::Versions,
        };
        use solana_signature::Signature;
        use solana_transaction::versioned::VersionedTransaction;

        let system = solana_sdk_ids::system_program::id();
        let recent_blockhashes = solana_sdk_ids::sysvar::recent_blockhashes::id();
        let payer = Pubkey::new_unique();
        let nonce_key = Pubkey::new_unique();
        let dst = Pubkey::new_unique();
        let slot = 300;
        let epoch = slot / 432_000;
        let fee = 5_000u64;
        let start = 1_000_000u64;

        let stored = DurableNonce::from_blockhash(&Hash::new_from_array([9u8; 32]));
        let nonce_data =
            bincode::serialize(&Versions::new(State::new_initialized(&payer, stored, fee)))
                .unwrap();

        let mut bank = ReplayBank::default();
        bank.insert(
            payer,
            AccountSharedData::from(Account {
                lamports: start,
                data: vec![],
                owner: system,
                executable: false,
                rent_epoch: 0,
            }),
            slot,
        );
        bank.insert(
            nonce_key,
            AccountSharedData::from(Account {
                lamports: 1_500_000,
                data: nonce_data,
                owner: system,
                executable: false,
                rent_epoch: 0,
            }),
            slot,
        );
        let replayer = Replayer::new(slot, epoch);
        register_builtins(&mut bank, &replayer.processor, &FeatureSet::all_enabled());
        bank.configure_sysvars(slot, 1_700_000_000);
        replayer.processor.fill_missing_sysvar_cache_entries(&bank);

        let advance = Instruction {
            program_id: system,
            accounts: vec![
                AccountMeta::new(nonce_key, false),
                AccountMeta::new_readonly(recent_blockhashes, false),
                AccountMeta::new_readonly(payer, true),
            ],
            data: vec![4, 0, 0, 0], // AdvanceNonceAccount
        };
        let mut xfer = vec![2u8, 0, 0, 0]; // Transfer, over-balance so the tx fails
        xfer.extend_from_slice(&(start * 10).to_le_bytes());
        let transfer = Instruction {
            program_id: system,
            accounts: vec![AccountMeta::new(payer, true), AccountMeta::new(dst, false)],
            data: xfer,
        };

        // The tell: recent_blockhash is a real blockhash, NOT the stored nonce value.
        let real_blockhash = Hash::new_from_array([3u8; 32]);
        assert_ne!(real_blockhash, *stored.as_hash());
        let message =
            Message::new_with_blockhash(&[advance, transfer], Some(&payer), &real_blockhash);
        let tx = sanitize(
            &VersionedTransaction {
                signatures: vec![Signature::default()],
                message: VersionedMessage::Legacy(message),
            },
            &crate::block::LoadedAddresses::default(),
        )
        .unwrap();

        let result = replayer.execute(&bank, &tx, fee, epoch, Hash::new_from_array([7u8; 32]));
        assert!(
            matches!(&result, Ok(ProcessedTransaction::Executed(e)) if !e.was_successful()),
            "should execute then fail on the transfer (not fail to load), got {result:?}"
        );
        commit_writes(&mut bank, &tx, &result, slot);

        // Normal-path payoff: the nonce did NOT advance (a durable tx would keep it), and only the fee was charged.
        let (nonce_acct, _) = bank
            .get_account_shared_data(&nonce_key)
            .expect("nonce account present");
        let after: Versions = bincode::deserialize(nonce_acct.data()).unwrap();
        let State::Initialized(data) = after.state() else {
            panic!("nonce should still be initialized");
        };
        assert_eq!(
            data.durable_nonce, stored,
            "a normal failed tx must roll the nonce back, not advance it"
        );
        let (payer_acct, _) = bank.get_account_shared_data(&payer).unwrap();
        assert_eq!(
            payer_acct.lamports(),
            start - fee,
            "fee payer charged the fee"
        );
    }

    #[test]
    fn executes_a_vote_through_the_builtin() {
        use solana_instruction::Instruction;
        use solana_message::{Message, VersionedMessage};
        use solana_signature::Signature;
        use solana_transaction::versioned::VersionedTransaction;
        use solana_vote_interface::instruction::{
            CreateVoteAccountConfig, create_account_with_config, tower_sync,
        };
        use solana_vote_interface::state::{TowerSync, VoteInit};

        let current_slot = 10u64;
        let voted_slot = 9u64;
        let epoch = current_slot / 432_000;
        let system = solana_sdk_ids::system_program::id();
        let payer = Pubkey::new_unique();
        let vote_pubkey = Pubkey::new_unique();
        // One identity plays node, authorized voter, and withdrawer.
        let identity = Pubkey::new_unique();
        let voted_hash = Hash::new_unique();

        let wallet = |lamports| {
            AccountSharedData::from(Account {
                lamports,
                data: vec![],
                owner: system,
                executable: false,
                rent_epoch: 0,
            })
        };
        let mut bank = ReplayBank::default();
        bank.insert(payer, wallet(1_000_000_000), current_slot);
        bank.insert(vote_pubkey, wallet(0), current_slot); // tx1 creates it

        let replayer = Replayer::new(current_slot, epoch);
        register_builtins(&mut bank, &replayer.processor, &FeatureSet::all_enabled());
        bank.configure_sysvars(current_slot, 1_700_000_000);
        bank.set_slot_hashes(&[(voted_slot, voted_hash)]);
        replayer.processor.fill_missing_sysvar_cache_entries(&bank);

        let run = |bank: &mut ReplayBank, ixs: &[Instruction]| -> TransactionProcessingResult {
            let msg = Message::new(ixs, Some(&payer));
            let tx = VersionedTransaction {
                signatures: vec![Signature::default(); msg.header.num_required_signatures as usize],
                message: VersionedMessage::Legacy(msg),
            };
            let sanitized = sanitize(&tx, &crate::block::LoadedAddresses::default()).unwrap();
            let result = replayer.execute(bank, &sanitized, 5_000, epoch, Hash::default());
            commit_writes(bank, &sanitized, &result, current_slot);
            result
        };

        // Tx1: create the account and let the Vote program initialize its state (never hand-build a VoteState).
        let vote_init = VoteInit {
            node_pubkey: identity,
            authorized_voter: identity,
            authorized_withdrawer: identity,
            commission: 0,
        };
        let create = create_account_with_config(
            &payer,
            &vote_pubkey,
            &vote_init,
            30_000_000,
            CreateVoteAccountConfig::default(),
        );
        let r1 = run(&mut bank, &create);
        assert!(
            matches!(&r1, Ok(ProcessedTransaction::Executed(e)) if e.was_successful()),
            "create + initialize should succeed (proves the Vote builtin runs): {r1:?}"
        );

        // Tx2: vote via TowerSync (mainnet's path; legacy Vote is deprecated now). The program reads SlotHashes to check the slot is real, so this passes only because we seeded (voted_slot, voted_hash).
        let vote_ix = tower_sync(
            &vote_pubkey,
            &identity,
            TowerSync::new_from_slot(voted_slot, voted_hash),
        );
        let r2 = run(&mut bank, &[vote_ix]);
        assert!(
            matches!(&r2, Ok(ProcessedTransaction::Executed(e)) if e.was_successful()),
            "tower-sync vote should succeed against a matching SlotHashes (proves SlotHashes is read): {r2:?}"
        );
    }

    #[test]
    fn replays_a_range_rolling_the_bank_forward() {
        use solana_account::ReadableAccount;
        use solana_instruction::{AccountMeta, Instruction};
        use solana_message::{Message, VersionedMessage};
        use solana_signature::Signature;
        use solana_transaction::versioned::VersionedTransaction;

        let system = solana_sdk_ids::system_program::id();
        let src = Pubkey::new_unique();
        let mid = Pubkey::new_unique();
        let dst = Pubkey::new_unique();
        let fee = 5_000u64;
        let src_pre = 10_000_000u64;
        let a1 = 3_000_000u64;
        let a2 = 1_000_000u64;
        let base_slot = 300u64;

        // A one-transfer block plus the meta the oracle reconciles against (account order [from, to, system]).
        let transfer_block = |from: Pubkey, to: Pubkey, amount: u64, from_pre: u64, slot: u64| {
            let mut data = vec![2u8, 0, 0, 0]; // System Transfer
            data.extend_from_slice(&amount.to_le_bytes());
            let ix = Instruction {
                program_id: system,
                accounts: vec![AccountMeta::new(from, true), AccountMeta::new(to, false)],
                data,
            };
            let message = Message::new(&[ix], Some(&from));
            let tx = VersionedTransaction {
                signatures: vec![
                    Signature::default();
                    message.header.num_required_signatures as usize
                ],
                message: VersionedMessage::Legacy(message),
            };
            Block {
                slot,
                parent_slot: slot - 1,
                blockhash: Hash::default(),
                block_height: 0,
                previous_blockhash: Hash::default(),
                block_time: 0,
                transactions: vec![crate::block::BlockTx {
                    transaction: tx,
                    meta: crate::block::TxMeta {
                        err: None,
                        fee,
                        compute_units_consumed: 0,
                        pre_balances: vec![from_pre, 0, 1],
                        post_balances: vec![from_pre - amount - fee, amount, 1],
                        loaded_addresses: crate::block::LoadedAddresses::default(),
                        post_token_balances: vec![],
                    },
                }],
                fee_reward: None,
            }
        };

        let mut bank = ReplayBank::default();
        bank.insert(
            src,
            AccountSharedData::from(Account {
                lamports: src_pre,
                data: vec![],
                owner: system,
                executable: false,
                rent_epoch: 0,
            }),
            base_slot,
        );
        let mut replayer = Replayer::new(base_slot, base_slot / 432_000);
        register_builtins(&mut bank, &replayer.processor, &FeatureSet::all_enabled());

        // Block N: src -> mid; block N+1: mid -> dst, which only reconciles if block N's write to mid rolled forward.
        let blocks = [
            transfer_block(src, mid, a1, src_pre, base_slot),
            transfer_block(mid, dst, a2, a1, base_slot + 1),
        ];
        let range = replayer.replay_range(&mut bank, &blocks);
        assert!(
            range.is_complete(),
            "both blocks should replay: {:?}",
            range.halt
        );
        assert_eq!(range.blocks_completed, 2);

        let balance = |pk: &Pubkey| {
            bank.get_account_shared_data(pk)
                .map(|(a, _)| a.lamports())
                .unwrap_or(0)
        };
        assert_eq!(balance(&src), src_pre - a1 - fee);
        assert_eq!(balance(&mid), a1 - a2 - fee);
        assert_eq!(balance(&dst), a2);
    }

    #[test]
    fn wires_precompile_verification() {
        use ed25519_dalek::{Signer, SigningKey};

        let bank = ReplayBank::default();
        let ed25519 = solana_sdk_ids::ed25519_program::id();

        // is_precompile recognizes the ed25519 program but not an arbitrary id.
        assert!(bank.is_precompile(&ed25519));
        assert!(!bank.is_precompile(&Pubkey::new_unique()));

        // A valid ed25519 signature in a self-contained precompile instruction verifies through process_precompile.
        let signing_key = SigningKey::from_bytes(&[7u8; 32]);
        let message = b"slate precompile test";
        let signature = signing_key.sign(message).to_bytes();
        let pubkey = signing_key.verifying_key().to_bytes();
        let ix = solana_ed25519_program::new_ed25519_instruction_with_signature(
            message, &signature, &pubkey,
        );

        assert!(
            bank.process_precompile(&ed25519, &ix.data, vec![ix.data.as_slice()])
                .is_ok(),
            "a valid ed25519 signature should verify"
        );

        // Corrupting the signed message makes verification fail.
        let mut corrupted = ix.data.clone();
        let last = corrupted.len() - 1;
        corrupted[last] ^= 0xff;
        assert!(
            bank.process_precompile(&ed25519, &corrupted, vec![corrupted.as_slice()])
                .is_err(),
            "a corrupted precompile instruction must fail"
        );
    }

    // secp256r1 is the only feature-gated precompile (enable_secp256r1_precompile, activated at slot
    // 345_600_000, the first slot of epoch 800). Before that the cluster had no secp256r1 precompile
    // at all, so a range below it must not resolve one.
    #[test]
    fn a_precompile_gated_by_an_inactive_feature_is_not_recognised() {
        let secp256r1 = solana_sdk_ids::secp256r1_program::id();

        // Baseline: with the feature active it IS a precompile, so the assertion below is about the
        // gate and not about secp256r1 being unsupported outright.
        assert!(ReplayBank::default().is_precompile(&secp256r1));

        let mut bank = ReplayBank::default();
        let mut feature_set = FeatureSet::all_enabled();
        feature_set.deactivate(&agave_feature_set::enable_secp256r1_precompile::id());
        bank.set_feature_set(feature_set);

        assert!(
            !bank.is_precompile(&secp256r1),
            "secp256r1 is gated off, so the runtime must not treat it as a precompile"
        );
        assert!(
            bank.process_precompile(&secp256r1, &[], vec![]).is_err(),
            "verifying through a precompile that did not exist yet must fail"
        );
    }

    // Weaker than its builtin counterpart, deliberately: Precompile::check_id short-circuits on
    // `feature.is_none_or(..)`, so the predicate is never consulted for an ungated precompile and no
    // predicate bug can drop these two. The over-gating guard is the baseline assert in the test
    // above. This pins the invariant against a future rewrite of the callback itself.
    #[test]
    fn ungated_precompiles_survive_a_partial_feature_set() {
        let mut bank = ReplayBank::default();
        let mut feature_set = FeatureSet::all_enabled();
        feature_set.deactivate(&agave_feature_set::enable_secp256r1_precompile::id());
        bank.set_feature_set(feature_set);

        for id in [
            solana_sdk_ids::ed25519_program::id(),
            solana_sdk_ids::secp256k1_program::id(),
        ] {
            assert!(
                bank.is_precompile(&id),
                "{id} has no feature gate, deactivating an unrelated feature must not disable it"
            );
        }
    }
}
