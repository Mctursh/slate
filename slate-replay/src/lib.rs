//! slate-replay: historical Solana account-state reconstruction via SVM
//! transaction replay.
//!
//! Phase 0 — walking skeleton: prove we can construct the SVM processor and the
//! account-loading callback against solana-svm 3.1.x. Execution (building the
//! per-slot environment, sanitizing a real tx, and reconciling against getBlock)
//! comes in Tasks 0.4–0.7.

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
pub mod snapshot;

use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use agave_feature_set::FeatureSet;
use agave_syscalls::create_program_runtime_environment_v1;
use solana_account::{Account, AccountSharedData, ReadableAccount, WritableAccount};
use solana_clock::Clock;
use solana_compute_budget_instruction::instructions_processor::process_compute_budget_instructions;
use solana_epoch_schedule::EpochSchedule;
use solana_fee_structure::FeeDetails;
use solana_hash::Hash;
use solana_precompile_error::PrecompileError;
use solana_program_runtime::{
    execution_budget::SVMTransactionExecutionBudget,
    loaded_programs::{BlockRelation, ForkGraph, ProgramCacheEntry},
};
use solana_lattice_hash::lt_hash::LtHash;
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
use solana_instruction_error::InstructionError;
use solana_transaction_error::TransactionError;
use solana_sysvar_id::SysvarId;
use solana_transaction::sanitized::SanitizedTransaction;

use crate::{
    bankhash::BankHashRoller,
    block::{Block, sanitize},
    oracle::reconcile,
};

/// Replay walks a single linear chain of slots, so there are no forks to reason
/// about — the program cache only ever needs "unknown".
pub struct SlateForkGraph;

impl ForkGraph for SlateForkGraph {
    fn relationship(&self, a: u64, b: u64) -> BlockRelation {
        // Replay is a single linear chain: an earlier slot is always an ancestor
        // of a later one. The program cache needs this to see deployed programs.
        match a.cmp(&b) {
            std::cmp::Ordering::Less => BlockRelation::Ancestor,
            std::cmp::Ordering::Equal => BlockRelation::Equal,
            std::cmp::Ordering::Greater => BlockRelation::Descendant,
        }
    }
}

/// Slate's stand-in for the validator's `Bank`: the account source the SVM
/// reads and writes during replay (it implements the SVM callback). Seeded from
/// the snapshot footprint (Task 0.4), mutated as each slot's transactions commit,
/// and the eventual home for sysvar/builtin/epoch-stake setup. Phase 0 holds
/// accounts in a map; a real range uses a disk-backed store.
pub struct ReplayBank {
    /// The account universe: `pubkey -> (account, slot last written)`. In-memory by
    /// default; a disk-backed store for ranges too big for RAM. See [`store`].
    store: Box<dyn AccountStore>,
    /// Transaction-committed writes in commit order, for the persistence layer.
    /// Setup writes (seeds, builtins, sysvars) are deliberately not logged.
    writes: Vec<WriteRecord>,
    /// Monotonic counter so same-slot writes to one account order correctly.
    write_version: u64,
    /// While recording a slot (`Some`), the pre-slot value of each account the slot
    /// writes (`None` = it didn't exist), captured on the first write. This drives the
    /// lattice-hash roll: mix each changed account out at its old value, in at its new.
    slot_dirty: Option<HashMap<Pubkey, Option<AccountSharedData>>>,
    /// The bank-hash roll, when active (`Some` for a real backfill, bootstrapped from
    /// the snapshot manifest). Holds the running lattice + bank hash and advances one
    /// slot at a time. `None` for tests that don't need forward bank hashes.
    bankhash_roller: Option<BankHashRoller>,
}

impl Default for ReplayBank {
    fn default() -> Self {
        Self {
            store: Box::new(MemStore::default()),
            writes: Vec::new(),
            write_version: 0,
            slot_dirty: None,
            bankhash_roller: None,
        }
    }
}

/// One transaction-committed account write: the account's state at the slot it
/// was written, tagged with a monotonic write version. This is what the
/// persistence layer turns into rows, owner-filtered to the indexed program.
#[derive(Clone)]
pub struct WriteRecord {
    pub slot: u64,
    pub write_version: u64,
    pub pubkey: Pubkey,
    pub account: AccountSharedData,
}

impl ReplayBank {
    /// A bank backed by an explicit account store (a `DiskStore` for a range too big
    /// for RAM). `default()` uses the in-memory store.
    pub fn with_store(store: Box<dyn AccountStore>) -> Self {
        Self {
            store,
            writes: Vec::new(),
            write_version: 0,
            slot_dirty: None,
            bankhash_roller: None,
        }
    }

    /// Flush the account store's buffered writes to disk (a no-op for the in-memory
    /// store). Called at the end of a run so the disk file is complete.
    pub fn flush(&mut self) {
        self.store.flush();
    }

    /// The raw account store behind the bank, for reading the reconstructed end-state
    /// directly. Unlike [`ReplayBank::get_account_shared_data`] it returns the stored
    /// value as-is (no zero-lamport filter) — the boundary diff does its own dead-account
    /// handling when it compares this against the snapshot at the last replayed slot.
    pub fn store(&self) -> &dyn AccountStore {
        self.store.as_ref()
    }

    pub fn insert(&mut self, key: Pubkey, account: AccountSharedData, slot: u64) {
        // While recording a slot, remember the account's pre-slot value the first time
        // it's written this slot, so the lattice can mix it out before mixing the new in.
        if self.slot_dirty.as_ref().is_some_and(|d| !d.contains_key(&key)) {
            let old = self.store.get(&key).map(|(a, _)| a);
            self.slot_dirty.as_mut().unwrap().insert(key, old);
        }
        self.store.put(key, account, slot);
    }

    /// Commit a transaction's write: log it (for persistence) and update the
    /// account map. Distinct from [`ReplayBank::insert`], which is for setup
    /// (seeds, builtins, sysvars) that isn't a chain write and mustn't be persisted.
    fn commit_write(&mut self, key: Pubkey, account: AccountSharedData, slot: u64) {
        self.write_version += 1;
        self.writes.push(WriteRecord {
            slot,
            write_version: self.write_version,
            pubkey: key,
            account: account.clone(),
        });
        self.insert(key, account, slot);
    }

    /// The transaction-committed writes captured so far, in commit order. The
    /// persistence layer owner-filters these to the program being indexed.
    pub fn writes(&self) -> &[WriteRecord] {
        &self.writes
    }

    /// Drain the write log, clearing it. The caller persists what it drains; draining
    /// per chunk keeps the log from growing with the range length (it holds account
    /// data, so over tens of thousands of slots it would otherwise blow up RAM).
    pub fn take_writes(&mut self) -> Vec<WriteRecord> {
        std::mem::take(&mut self.writes)
    }

    /// Start recording which accounts a slot writes (with their pre-slot values), so
    /// the lattice hash can be rolled by the slot's changes. Call before configuring
    /// sysvars and replaying the slot's transactions.
    pub fn begin_slot(&mut self) {
        self.slot_dirty = Some(HashMap::new());
    }

    /// Take every account written since [`ReplayBank::begin_slot`], as `(pubkey,
    /// pre-slot value, post-slot value)`; a `None` pre-slot value means the slot
    /// created the account. Stops recording.
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

    /// Start the bank-hash roll from the snapshot manifest's lattice hash and bank hash
    /// at s_snap. Once set, each replayed slot advances the roll.
    pub fn bootstrap_bankhash(&mut self, lt_hash: LtHash, bank_hash: Hash) {
        self.bankhash_roller = Some(BankHashRoller::new(lt_hash, bank_hash));
    }

    /// The last finalized slot's bank hash — what gets prepended into SlotHashes for
    /// the next slot — or `None` if the roll isn't active.
    pub fn parent_bank_hash(&self) -> Option<Hash> {
        self.bankhash_roller.as_ref().map(|r| r.bank_hash())
    }

    /// Roll the lattice over this slot's changed accounts and compute the slot's bank
    /// hash, advancing the roll. `None` (and no-op) if the roll isn't active.
    pub fn finalize_slot_bankhash(
        &mut self,
        signature_count: u64,
        blockhash: &Hash,
    ) -> Option<Hash> {
        let changes = self.take_slot_changes();
        self.bankhash_roller
            .as_mut()
            .map(|r| r.roll_slot(&changes, signature_count, blockhash))
    }

    /// Prepend `(slot, bank_hash)` to the SlotHashes sysvar, exactly as the runtime's
    /// `update_slot_hashes` does at the start of a slot. `SlotHashes::add` keeps the
    /// entries newest-first and truncates to the 512-entry maximum.
    pub fn roll_slot_hashes(&mut self, slot: u64, bank_hash: Hash) {
        let mut slot_hashes = self
            .get_account_shared_data(&SlotHashes::id())
            .and_then(|(account, _)| bincode::deserialize::<SlotHashes>(account.data()).ok())
            .unwrap_or_else(|| SlotHashes::new(&[]));
        slot_hashes.add(slot, bank_hash);
        self.set_sysvar_account(SlotHashes::id(), bincode::serialize(&slot_hashes).unwrap());
    }

    /// Register a builtin (native) program: put its loader-owned account in the
    /// bank and hand its entrypoint to the processor's program cache. Builtins
    /// (System, the BPF loaders, ...) aren't loaded like normal accounts; the
    /// processor runs them natively, so they must be registered up front.
    pub fn add_builtin(
        &mut self,
        processor: &TransactionBatchProcessor<SlateForkGraph>,
        program_id: Pubkey,
        name: &str,
        entry: ProgramCacheEntry,
    ) {
        // Only synthesize a stub account when the real on-chain one isn't already
        // present (loaded from the snapshot). The real builtin account's data is
        // its runtime name — e.g. system program is "solana_system_program" (21
        // bytes) — which differs from the `solana_builtins` short name
        // ("system_program", 14 bytes). Overwriting the loaded account with a
        // name-stub changes its serialized length, and since builtins are passed
        // as instruction accounts, that shifts every following account in the VM
        // input region by the size delta (8 bytes here, after BPF u128 alignment).
        // Programs that persist raw input-region pointers (Neon EVM's holder)
        // then store shifted pointers, corrupting their state and the bank hash.
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

    /// Build the sysvars this replay needs and insert them as accounts; the
    /// processor pulls them into its cache via `fill_missing_sysvar_cache_entries`.
    /// Skeleton set: Clock (real slot + block time), Rent, EpochSchedule. Harder
    /// txs will add SlotHashes / StakeHistory.
    pub fn configure_sysvars(&mut self, slot: u64, unix_timestamp: i64) {
        let epoch = slot / 432_000; // mainnet: no warmup
        // Clock: derive from the snapshot's real Clock — the epoch fields
        // (epoch, epoch_start_timestamp, leader_schedule_epoch) are constant within an
        // epoch, so only slot and unix_timestamp advance. Reproducing them exactly is
        // what the bank-hash roll needs. Fall back to a synthesized Clock when none is
        // loaded (fixtures/tests that don't seed one).
        let clock = match self
            .get_account_shared_data(&Clock::id())
            .and_then(|(account, _)| bincode::deserialize::<Clock>(account.data()).ok())
        {
            Some(mut clock) => {
                clock.slot = slot;
                clock.unix_timestamp = unix_timestamp;
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
        // Rent / EpochSchedule are constant and seeded from the snapshot; only
        // synthesize them when absent (tests). Overwriting a loaded value would be a
        // spurious change in the lattice.
        if self.get_account_shared_data(&Rent::id()).is_none() {
            self.set_sysvar_account(Rent::id(), bincode::serialize(&Rent::default()).unwrap());
        }
        if self.get_account_shared_data(&EpochSchedule::id()).is_none() {
            self.set_sysvar_account(
                EpochSchedule::id(),
                bincode::serialize(&EpochSchedule::without_warmup()).unwrap(),
            );
        }
        // SlotHashes is seeded from the snapshot (real bank hashes) and prepended per
        // slot by roll_slot_hashes; only default it to empty when absent.
        if self.get_account_shared_data(&SlotHashes::id()).is_none() {
            self.set_slot_hashes(&[]);
        }
        // RecentBlockhashes is seeded from the snapshot and rolled at freeze
        // (freeze_slot). When absent (tests) fill a 150-entry placeholder: it's
        // deprecated, but AdvanceNonceAccount errors if it's empty, and its full size
        // fixes the rent-exempt lamports the oracle checks.
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

    /// Apply the account writes the runtime does at slot freeze that the bank-hash
    /// lattice must include: extend SlotHistory with this slot, and prepend this
    /// slot's blockhash to RecentBlockhashes. Both roll forward from the snapshot's
    /// real value. No-op for the sysvars the snapshot didn't supply (tests).
    pub fn freeze_slot(&mut self, slot: u64, blockhash: Hash, fee_reward: Option<(Pubkey, u64)>) {
        // Leader fee credit: the runtime pays the slot's leader 50% of its fees at
        // freeze (burning the rest). getBlock's "Fee" reward gives the exact amount.
        if let Some((leader, lamports)) = fee_reward {
            if let Some((mut account, _)) = self.get_account_shared_data(&leader) {
                account.set_lamports(account.lamports() + lamports);
                self.insert(leader, account, slot);
            }
        }
        if let Some(mut history) = self
            .get_account_shared_data(&solana_sdk_ids::sysvar::slot_history::id())
            .and_then(|(account, _)| {
                bincode::deserialize::<solana_sysvar::slot_history::SlotHistory>(account.data()).ok()
            })
        {
            history.add(slot);
            self.set_sysvar_account(
                solana_sdk_ids::sysvar::slot_history::id(),
                bincode::serialize(&history).unwrap(),
            );
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
            // Newest first, this slot's blockhash prepended, capped at 150 like the
            // runtime. Mainnet's fee is fixed, so every entry carries 5000.
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

    /// The blockhashes currently valid for transaction age: the RecentBlockhashes
    /// sysvar this bank carries (seeded from the snapshot, rolled at each freeze,
    /// capped at 150 like the runtime's queue). A transaction whose recent_blockhash
    /// is in here is a normal one; a transaction whose isn't is either durable-nonce
    /// (its "blockhash" is really the stored nonce) or too old to land. Mirrors the
    /// blockhash-queue lookup agave's age check does before falling back to nonces.
    /// Empty when the sysvar wasn't seeded (test banks), which makes an AdvanceNonce-
    /// first tx take the nonce path — the behavior callers had before this check.
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

    /// The blockhash an initialized System nonce account currently stores, or `None`
    /// if `address` isn't one. A durable-nonce transaction's recent_blockhash equals
    /// this exactly — that's what makes it durable — so comparing against it tells a
    /// real durable-nonce tx from a normal tx that merely advances its own nonce as an
    /// ordinary instruction. Exact, unlike the [`Self::recent_blockhashes`] window
    /// whose 150 entries stop one short of agave's age-150 validity (a normal tx built
    /// on the oldest-still-valid blockhash looked "not recent" and got mis-routed).
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

    /// Populate the SlotHashes sysvar from recent `(slot, hash)` pairs (newest
    /// first, as the runtime keeps them). Vote transactions read it to check the
    /// slot they vote on is real.
    pub fn set_slot_hashes(&mut self, entries: &[(u64, Hash)]) {
        self.set_sysvar_account(
            SlotHashes::id(),
            bincode::serialize(&SlotHashes::new(entries)).unwrap(),
        );
    }

    fn set_sysvar_account(&mut self, id: Pubkey, data: Vec<u8>) {
        // On-chain sysvar accounts are rent-exempt for their exact size, so use the
        // rent-exempt minimum for `data`, not a placeholder. A tx that passes a
        // sysvar as an account (e.g. a durable-nonce tx passing RecentBlockhashes)
        // has its balance checked against the chain by the oracle; a wrong balance
        // would falsely halt the replay.
        let lamports = Rent::default().minimum_balance(data.len());
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

/// Register the native builtin programs the SVM runs directly (System, Vote, the
/// BPF loaders, ComputeBudget, loader-v4, the ZK proof programs), straight from
/// agave's canonical `solana_builtins::BUILTINS` list so the set stays exactly in
/// sync with the runtime.
///
/// Stake, Config, and AddressLookupTable are deliberately absent: they've been
/// migrated to Core BPF, so they run as ordinary BPF programs loaded from their
/// on-chain bytecode through the loaders above, not as native builtins. That's
/// also why no 3.x crate for them exists to register.
///
/// Every entry is registered because we run with all features enabled. Once we
/// compute the exact per-slot feature set, gate on `enable_feature_id` so a
/// program only counts as a builtin from the slot its feature activated.
pub fn register_builtins(
    bank: &mut ReplayBank,
    processor: &TransactionBatchProcessor<SlateForkGraph>,
) {
    for builtin in solana_builtins::BUILTINS {
        bank.add_builtin(
            processor,
            builtin.program_id,
            builtin.name,
            ProgramCacheEntry::new_builtin(0, builtin.name.len(), builtin.entrypoint),
        );
    }
}

/// A feature account's activation slot: `Some(slot)` when the account is owned by
/// the feature program and its `Feature { activated_at }` is set, `None` otherwise
/// (wrong owner, too small, unparsable, or not yet activated). `Feature` is a lone
/// `Option<u64>` field, so it decodes straight as one; an inactive account's
/// zero-padded body simply fails to decode as `Some`, which is the answer we want.
fn feature_activation(account: &AccountSharedData) -> Option<u64> {
    use solana_account::ReadableAccount;
    // 9 == Feature::size_of() (1-byte Option tag + u64).
    if *account.owner() != solana_sdk_ids::feature::id() || account.data().len() < 9 {
        return None;
    }
    bincode::deserialize::<Option<u64>>(account.data())
        .ok()
        .flatten()
}

/// Build the feature set active at `slot` from the feature accounts already in
/// `bank` (seeded from the snapshot), instead of `FeatureSet::all_enabled()`. A
/// feature counts as active only if its account carries an activation slot at or
/// before `slot`. This is the exact set the runtime executed against; feature-
/// gated program behavior (and the derived syscall set) depends on getting it
/// right, which `all_enabled` doesn't for a historical slot.
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

// Precompile verification (ed25519 / secp256k1 / secp256r1) is wired to
// agave-precompiles, so a tx carrying a precompile instruction is verified rather
// than failing. Every precompile-enabling feature is active at the epoch-808
// floor, so all of them count as enabled. Epoch stake stays at the default 0,
// fine for anything that doesn't touch rewards.
impl InvokeContextCallback for ReplayBank {
    fn is_precompile(&self, program_id: &Pubkey) -> bool {
        agave_precompiles::is_precompile(program_id, |_| true)
    }

    fn process_precompile(
        &self,
        program_id: &Pubkey,
        data: &[u8],
        instruction_datas: Vec<&[u8]>,
    ) -> Result<(), PrecompileError> {
        match agave_precompiles::get_precompile(program_id, |_| true) {
            Some(precompile) => {
                precompile.verify(data, &instruction_datas, &FeatureSet::all_enabled())
            }
            None => Err(PrecompileError::InvalidPublicKey),
        }
    }
}

impl TransactionProcessingCallback for ReplayBank {
    fn get_account_shared_data(&self, pubkey: &Pubkey) -> Option<(AccountSharedData, u64)> {
        // A zero-lamport account is dead: the runtime purges it, so a read returns the
        // default (empty, system-owned). Keep it in the map so the lattice roll can
        // still mix it out, but hand reads nothing — otherwise stale bytes on a drained
        // account make a later System Allocate at that address fail "already in use"
        // where the chain re-creates it fresh. Mirrors the snapshot loader's
        // retain(lamports > 0), which drops dead accounts at seed time.
        self.store
            .get(pubkey)
            .filter(|(account, _)| account.lamports() > 0)
    }
}

/// The per-replay SVM harness. Owns the fork graph (the processor only holds a
/// `Weak` to it, so someone must keep the `Arc` alive) and the transaction
/// processor. Grows to hold the bank, environment, and config as we wire them.
pub struct Replayer {
    _fork_graph: Arc<RwLock<SlateForkGraph>>,
    pub processor: TransactionBatchProcessor<SlateForkGraph>,
    /// Runtime feature set, used to parse each tx's compute-budget limits.
    feature_set: FeatureSet,
    /// The SVM view of `feature_set`, held once for the per-batch environment.
    svm_feature_set: SVMFeatureSet,
}

impl Replayer {
    /// The runtime feature set this replayer was built against — the exact per-slot
    /// set, used to gate compat shims (e.g. re-supplied removed builtins).
    pub fn feature_set(&self) -> &FeatureSet {
        &self.feature_set
    }

    /// Build an execution-ready processor for `slot`/`epoch` with every feature
    /// enabled. Fine for the fixtures and any builtin-only path; a faithful replay
    /// of a real slot uses [`Replayer::new_with_feature_set`] with the exact set
    /// from [`build_feature_set`].
    pub fn new(slot: u64, epoch: u64) -> Self {
        Self::new_with_feature_set(slot, epoch, FeatureSet::all_enabled())
    }

    /// Build the processor against an explicit `feature_set` — the exact per-slot
    /// set derived from the on-chain feature accounts. The VM environment's syscall
    /// set and costs, and the compute-budget parser, both key off it, so feature-
    /// gated execution matches what ran on chain.
    pub fn new_with_feature_set(slot: u64, epoch: u64, feature_set: FeatureSet) -> Self {
        let fork_graph = Arc::new(RwLock::new(SlateForkGraph));
        // The SVM environment takes the derived SVMFeatureSet; the compute-budget
        // parser takes the runtime FeatureSet. Both are held for reuse per batch.
        let svm_feature_set = feature_set.runtime_features();
        let budget = SVMTransactionExecutionBudget::default();
        let loader = Arc::new(
            create_program_runtime_environment_v1(&svm_feature_set, &budget, false, false)
                .expect("build v1 program runtime environment"),
        );
        let processor = TransactionBatchProcessor::new(
            slot,
            epoch,
            Arc::downgrade(&fork_graph),
            Some(loader),
            None,
        );
        Self {
            _fork_graph: fork_graph,
            processor,
            feature_set,
            svm_feature_set,
        }
    }

    /// The per-batch runtime settings the SVM executes against. `blockhash` comes
    /// from the transaction; `feature_set` is `all_enabled` for the skeleton (the
    /// real slot-derived set comes later). `epoch_total_stake` defaults to 0,
    /// fine for anything that doesn't touch stake.
    pub fn environment(&self, blockhash: Hash, epoch: u64) -> TransactionProcessingEnvironment {
        TransactionProcessingEnvironment {
            blockhash,
            // non-zero so fees aren't disabled; the exact fee comes from the
            // per-tx check results, not from this field.
            blockhash_lamports_per_signature: 5_000,
            feature_set: self.svm_feature_set,
            program_runtime_environments_for_execution: self
                .processor
                .get_environments_for_epoch(epoch),
            program_runtime_environments_for_deployment: self
                .processor
                .get_environments_for_epoch(epoch),
            rent: Rent::default(),
            ..Default::default()
        }
    }

    /// Execute one sanitized transaction against `bank` and return its processing
    /// result. `fee` is the block meta's total fee, used to build the pre-validated
    /// check result the bank's fee validation would otherwise produce. Runs a batch
    /// of one and hands back the single result. This is the primitive the per-slot
    /// loop calls for each transaction, in order.
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

    /// Like [`Replayer::execute`] but against an explicit `processor`. The per-slot
    /// loop passes a `new_from` processor so each slot executes against a fresh
    /// sysvar cache while sharing the program cache and builtins.
    fn execute_with(
        &self,
        processor: &TransactionBatchProcessor<SlateForkGraph>,
        bank: &ReplayBank,
        tx: &SanitizedTransaction,
        fee: u64,
        epoch: u64,
        blockhash: Hash,
    ) -> TransactionProcessingResult {
        // The environment blockhash is the block's, NOT the tx's recent_blockhash.
        // A durable-nonce tx carries the nonce itself as its recent_blockhash, but
        // the nonce advances from the blockhash the bank is on at this slot (the
        // block's previousBlockhash). For non-nonce txs the environment blockhash is
        // otherwise unobserved, so sourcing it from the block is correct and safe.
        let env = self.environment(blockhash, epoch);
        // Parse the tx's OWN compute-budget instructions for its real CU limit,
        // price, and loaded-data-size limit — not a default. A tx that exhausts its
        // requested limit on chain must exhaust it here too. A malformed budget is
        // rejected on chain, so we fail it here the same way.
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
                // Decide durable-nonce vs normal. A tx whose first instruction is System
                // AdvanceNonceAccount looks like a durable nonce, but if its
                // recent_blockhash is a real recent blockhash it's just a normal tx
                // topping up its own nonce — agave validates that via the blockhash queue
                // (nonce = None) and the advance runs as an ordinary instruction. It's a
                // real durable-nonce tx only when the recent_blockhash IS the account's
                // stored nonce, so we compare against that directly. (Earlier this asked
                // whether the blockhash was in RecentBlockhashes, but that sysvar holds
                // 150 entries — ages 0-149 — while agave's age check accepts age 150 too,
                // so a normal tx built on the oldest-still-valid blockhash got mis-routed
                // to the nonce path and failed BlockhashNotFound.) On the nonce path the
                // SVM validates and advances the nonce, and on failure still rolls it back
                // advanced — which a normal tx's fee-only rollback would not.
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
        // Record program logs so a divergence can be diagnosed by comparing the
        // replay's log stream against the chain's getBlock logMessages. CPI and
        // return-data recording stay off; only the log stream is needed.
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

    /// Replay every transaction in `block` in order against `bank`, reconciling
    /// each against the block meta and committing a successful tx's writes back so
    /// the next tx sees them. Stops at the first transaction it can't replay (a
    /// v0/lookup-table tx, unsupported for now) or whose result diverges from the
    /// chain — a divergence leaves the bank unreliable, so continuing is pointless.
    /// `bank` must already be seeded and have its builtins registered.
    pub fn replay_block(&self, bank: &mut ReplayBank, block: &Block, epoch: u64) -> BlockReplay {
        self.replay_block_with(&self.processor, bank, block, epoch)
    }

    /// Replay `block` against an explicit `processor` (see [`Replayer::execute_with`]).
    fn replay_block_with(
        &self,
        processor: &TransactionBatchProcessor<SlateForkGraph>,
        bank: &mut ReplayBank,
        block: &Block,
        epoch: u64,
    ) -> BlockReplay {
        // With the bank-hash roll active, record the slot's writes and prepend the
        // parent's bank hash into SlotHashes (as the runtime's update_slot_hashes does
        // at slot start), so this slot's txs — including votes, which validate against
        // SlotHashes — read the real recent history.
        let rolling = bank.parent_bank_hash().is_some();
        if rolling {
            bank.begin_slot();
        }
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
            // SIMD-0162 compat. The solana-svm 3.1.x Slate runs on removed the runtime's
            // "an instruction may not modify an executable account" checks for good (the
            // feature is code-complete, not gated). That's right for current mainnet but
            // wrong for a slot before the feature activated, where the chain still enforced
            // them — so we re-supply the check here. A tx that changed a pre-existing
            // executable account failed on chain (ExecutableLamportChange); mark ours failed
            // too, and reconcile + commit roll it back to fees-only, matching the chain.
            if !self
                .feature_set
                .is_active(&agave_feature_set::remove_accounts_executable_flag_checks::id())
            {
                if let Ok(ProcessedTransaction::Executed(executed)) = &mut result {
                    if executed.was_successful() {
                        if let Some(idx) =
                            executable_modification(bank, &tx, &executed.loaded_transaction.accounts)
                        {
                            executed.execution_details.status =
                                Err(TransactionError::InstructionError(
                                    idx as u8,
                                    InstructionError::ExecutableLamportChange,
                                ));
                        }
                    }
                }
            }
            let reconciliation = reconcile(&account_keys, &block_tx.meta, &result);
            if !reconciliation.matched() {
                return BlockReplay::halted(i, reconciliation.issues.join("; "));
            }

            commit_writes(bank, &tx, &result, block.slot);
        }

        // Apply the runtime's freeze-time sysvar writes (SlotHistory, RecentBlockhashes),
        // then roll the lattice over everything this slot wrote and compute the slot's
        // bank hash; it becomes the parent for the next slot's SlotHashes prepend.
        if rolling {
            bank.freeze_slot(block.slot, block.blockhash, block.fee_reward);
            let signature_count = block
                .transactions
                .iter()
                .map(|tx| tx.transaction.signatures.len() as u64)
                .sum();
            if let Some(bank_hash) = bank.finalize_slot_bankhash(signature_count, &block.blockhash) {
                eprintln!("slot {} computed bank_hash {bank_hash}", block.slot);
            }
        }

        BlockReplay::complete(block.transactions.len())
    }

    /// Replay a contiguous range of blocks in slot order against one `bank`, which
    /// rolls forward from block to block (each block's committed writes are visible
    /// to the next). Each block gets a fresh per-slot processor via `new_from`, so
    /// the sysvar cache (Clock, etc.) advances with the slot while the program cache
    /// and builtins carry over — programs aren't reloaded every slot. Stops at the
    /// first block that doesn't fully replay and reports where.
    ///
    /// Intra-epoch only: crossing an epoch boundary needs feature-activation and
    /// reward machinery that isn't built yet, and the feature set is fixed at
    /// construction. `bank` must already be seeded with builtins registered.
    pub fn replay_range(&self, bank: &mut ReplayBank, blocks: &[Block]) -> RangeReplay {
        // Self-verify against consensus as we go. A validator vote carries the bank
        // hash of the slot it votes on, so votes in later blocks confirm earlier
        // slots' computed hashes. `computed` holds our hashes awaiting a vote (with
        // the block index, so a mismatch can stop coverage at the last good slot);
        // `confirmed` holds votes awaiting their slot. Each slot reconciles once, so
        // both stay bounded to the ~30-slot vote lag. A mismatch means our state
        // diverged from what a supermajority of stake agreed on: halt.
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
                            halt: Some((slot, BlockReplay::halted(
                                0,
                                format!("bank-hash mismatch vs consensus vote: computed {got}, vote {vote_hash}"),
                            ))),
                        };
                    }
                    Some(_) => verified += 1,
                    None => {
                        confirmed.entry(slot).or_insert(vote_hash);
                    }
                }
            }

            // Replay the slot.
            let epoch = block.slot / 432_000;
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
                            halt: Some((block.slot, BlockReplay::halted(
                                0,
                                format!("bank-hash mismatch vs consensus vote: computed {got}, vote {vote_hash}"),
                            ))),
                        };
                    }
                    Some(_) => verified += 1,
                    None => {
                        computed.insert(block.slot, (got, completed));
                    }
                }
            }
        }

        // The tail (~30 slots) can't be confirmed here: their votes fall past the
        // range end, so they're reported unverified, not wrong.
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

        // Commit any buffered writes so the disk store's file is complete (no-op for
        // the in-memory store).
        bank.flush();

        RangeReplay {
            blocks_completed: blocks.len(),
            halt: None,
        }
    }
}

/// Apply a processed transaction's account changes back into the bank so later
/// transactions in the same block see them, mirroring how agave commits a batch
/// (`update_accounts_for_executed_tx`):
///
/// - **Executed + successful:** write back only the accounts the tx could have
///   modified — the writable ones. A read-only account can't change, so
///   re-storing it would fabricate a write at this slot.
/// - **Executed but failed:** every state change rolls back except the fee charge
///   and any advanced nonce, so commit just those rollback accounts. Without this
///   the payer's fee deduction is lost and the next tx's payer balance no longer
///   lines up.
/// - **Fees-only (loaded but not executed):** the fee payer was still charged, so
///   commit its rollback too.
/// - **Not processed:** nothing hit the chain, so nothing to commit.
/// Re-supply SIMD-0162's removed check: an instruction may not change an executable
/// account's lamports, data, or owner. Scans a successful tx's post-state for a writable
/// account that was *already* executable and came out changed — exactly what the chain
/// rejected before the feature activated. Returns its index in the tx's account list, or
/// `None` if the tx touched no executable account (the common case, so the scan is cheap:
/// executable accounts are rarely writable). A freshly created account (no pre-state) is
/// skipped — a program deploy sets executable legitimately and isn't this violation.
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

/// Write a failed / fees-only tx's rollback accounts — the charged fee payer, plus
/// an advanced nonce if the tx used one — back into the bank.
fn commit_rollback(bank: &mut ReplayBank, rollback: &RollbackAccounts, slot: u64) {
    for (address, account) in rollback {
        bank.commit_write(*address, account.clone(), slot);
    }
}

/// The outcome of replaying a block: how many transactions committed cleanly, and
/// where it stopped if it didn't finish.
#[derive(Debug)]
pub struct BlockReplay {
    pub replayed: usize,
    pub halt: Option<Halt>,
}

/// Where and why a block replay stopped early.
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

    /// Whether the whole block replayed and reconciled.
    pub fn is_complete(&self) -> bool {
        self.halt.is_none()
    }
}

/// The outcome of replaying a range of blocks: how many completed, and where it
/// stopped if a block diverged — the slot plus that block's [`BlockReplay`].
#[derive(Debug)]
pub struct RangeReplay {
    pub blocks_completed: usize,
    pub halt: Option<(u64, BlockReplay)>,
}

impl RangeReplay {
    /// Whether every block in the range replayed and reconciled.
    pub fn is_complete(&self) -> bool {
        self.halt.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skeleton_constructs() {
        // The harness + bank construct and link.
        let _bank = ReplayBank::default();
        let _replayer = Replayer::new(fixture::SLOT, fixture::SLOT / 432_000);
    }

    #[test]
    fn registers_system_builtin() {
        use solana_account::ReadableAccount;
        let mut bank = fixture::seed_bank();
        let replayer = Replayer::new(fixture::SLOT, fixture::SLOT / 432_000);
        register_builtins(&mut bank, &replayer.processor);

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
        register_builtins(&mut bank, &replayer.processor);

        // Every builtin agave lists is registered, executable, and native-owned.
        for builtin in solana_builtins::BUILTINS {
            let (acct, _) = bank
                .get_account_shared_data(&builtin.program_id)
                .unwrap_or_else(|| panic!("{} not registered", builtin.name));
            assert!(acct.executable(), "{} should be executable", builtin.name);
            assert_eq!(*acct.owner(), solana_sdk_ids::native_loader::id());
        }
        // Vote and loader-v4 specifically — loader-v4 was missing from the old
        // hand-written set, so this pins the expansion.
        assert!(
            bank.get_account_shared_data(&solana_sdk_ids::vote::id())
                .is_some()
        );
        assert!(
            bank.get_account_shared_data(&solana_sdk_ids::loader_v4::id())
                .is_some()
        );
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

        // The real on-chain system_program account carries its runtime name
        // "solana_system_program" (21 bytes) as data — that's what a snapshot
        // footprint loads. `solana_builtins` knows it by the short name
        // "system_program" (14 bytes). If register_builtins stubbed over the
        // loaded account, its serialized length would drop by 8 (after BPF u128
        // alignment) and every following instruction account would shift in the
        // VM input region — the slot-030 +8 pointer bug.
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
        register_builtins(&mut bank, &replayer.processor);

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

    #[test]
    fn builds_environment() {
        let replayer = Replayer::new(fixture::SLOT, fixture::SLOT / 432_000);
        let env = replayer.environment(Hash::default(), fixture::SLOT / 432_000);
        assert_eq!(env.blockhash_lamports_per_signature, 5_000);
        assert_eq!(env.epoch_total_stake, 0);
    }

    #[test]
    fn executes_and_reconciles() {
        use solana_account::ReadableAccount;
        use solana_svm::transaction_processing_result::ProcessedTransaction;

        let epoch = fixture::SLOT / 432_000;

        // --- assemble every input ---
        let mut bank = fixture::seed_bank();
        let replayer = Replayer::new(fixture::SLOT, epoch);
        register_builtins(&mut bank, &replayer.processor);
        bank.configure_sysvars(fixture::SLOT, fixture::BLOCK_TIME);
        replayer.processor.fill_missing_sysvar_cache_entries(&bank);

        // --- run it ---
        let result = replayer.execute(
            &bank,
            &fixture::sanitized_transaction(),
            fixture::FEE,
            epoch,
            Hash::default(),
        );

        // --- reconcile against the getBlock oracle ---
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
        register_builtins(&mut bank, &replayer.processor);
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
        register_builtins(&mut bank, &replayer.processor);
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
        // The proof that matters: account state reconstructs bit-exact *through*
        // the CPI. The program did an inner System::transfer and every balance
        // lands on the on-chain value. State is the only thing Slate serves.
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
        // Compute units do NOT reconcile here: replay charges 11_451 vs chain's
        // 11_343 (~1%). Unlike the transfer/memo fixtures (bit-exact CU), this CPI
        // path touches a cost that `all_enabled()` accounts differently than the
        // exact feature set live at this epoch. It's a CU-accounting gap, not a
        // state error (balances above are exact). Guarding our own number so any
        // drift surfaces; exact-CU is a Phase-1 feature-set task.
        assert_eq!(
            executed.execution_details.executed_units,
            11_451,
            "replay CU (chain was {})",
            fixture::cpi::COMPUTE_UNITS
        );
    }

    #[test]
    fn replays_a_block_and_commits() {
        use solana_account::ReadableAccount;

        let s = fixture::cpi::SLOT;
        let epoch = s / 432_000;
        let mut bank = fixture::cpi::seed_bank();
        let replayer = Replayer::new(s, epoch);
        register_builtins(&mut bank, &replayer.processor);

        let block = fixture::cpi::block();
        let outcome = replayer.replay_block(&mut bank, &block, epoch);

        assert!(
            outcome.is_complete(),
            "block should replay clean, halted: {:?}",
            outcome.halt
        );
        assert_eq!(outcome.replayed, 1);

        // The commit landed in the bank: the CPI recipient now holds its post
        // balance, so a following tx in the same block would read it.
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
        let replayer = Replayer::new(slot, epoch);
        register_builtins(&mut bank, &replayer.processor);

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

        // src -> mid, then mid -> dst. mid's pre-balance in tx2 only lines up if
        // tx1 committed first, so this exercises the tx-to-tx commit chaining.
        let block = block::Block {
            slot,
            parent_slot: slot - 1,
            blockhash: Hash::default(),
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
        // SIMD-0162 compat. The 3.1.x SVM lets a transfer to an executable account
        // succeed — the runtime check was removed. Before the feature activated the chain
        // rejected it (ExecutableLamportChange). `executable_modification` re-supplies the
        // check: a writable account that was already executable and came out of the tx
        // changed is the violation.
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

        // transfer(payer -> program): compiled account order is [payer (writable signer),
        // program (writable), system_program (readonly)].
        let mut data = vec![2u8, 0, 0, 0]; // SystemInstruction::Transfer discriminant
        data.extend_from_slice(&500_000u64.to_le_bytes());
        let ix = Instruction {
            program_id: system,
            accounts: vec![AccountMeta::new(payer, true), AccountMeta::new(program, false)],
            data,
        };
        let message = Message::new_with_blockhash(&[ix], Some(&payer), &Hash::default());
        let vtx = VersionedTransaction {
            signatures: vec![Signature::default()],
            message: VersionedMessage::Legacy(message),
        };
        let tx = sanitize(&vtx, &block::LoadedAddresses::default()).unwrap();

        let system_acct = AccountSharedData::new(1, 0, &loader);
        // Post-state where the transfer landed: `program` gained lamports — the violation,
        // at its account index (1).
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

        // Control: the executable account is untouched — nothing to flag.
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

        // Control: same change, but the account was never executable — allowed.
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
            register_builtins(&mut bank, &replayer.processor);
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
        register_builtins(&mut bank, &replayer.processor);

        // Transfer ten times what the payer holds: it loads and executes, then
        // fails on insufficient funds. The fee is charged; the transfer rolls back.
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

        // Executed but not successful — the branch that used to commit nothing.
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
            "recipient must not exist — the transfer rolled back"
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
            bincode::serialize(&Versions::new(State::new_initialized(&payer, stored, fee))).unwrap();

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
        register_builtins(&mut bank, &replayer.processor);
        // AdvanceNonceAccount reads RecentBlockhashes from the sysvar cache, so it
        // has to be configured and pulled in before the tx runs.
        bank.configure_sysvars(slot, 1_700_000_000);
        replayer.processor.fill_missing_sysvar_cache_entries(&bank);

        // Durable-nonce tx: advance the nonce, then over-transfer so the tx fails.
        // Its recent_blockhash IS the stored nonce, which is how a durable-nonce tx
        // is formed and how the runtime rolls the advanced nonce forward on failure.
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

        // The block's blockhash — what a durable nonce advances FROM. Deliberately
        // different from the tx's recent_blockhash (the nonce) so the test proves
        // the advance uses the block's blockhash, not the tx's.
        let block_blockhash = Hash::new_from_array([7u8; 32]);
        let result = replayer.execute(&bank, &tx, fee, epoch, block_blockhash);
        assert!(
            matches!(&result, Ok(ProcessedTransaction::Executed(e)) if !e.was_successful()),
            "durable-nonce tx should execute then fail on the transfer, got {result:?}"
        );

        commit_writes(&mut bank, &tx, &result, slot);

        // The payoff: the transfer failed, but the nonce still advanced (a regular
        // failed tx leaves it untouched), and the fee payer was charged.
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
        assert_eq!(payer_acct.lamports(), start - fee, "fee payer charged the fee");
    }

    #[test]
    fn a_normal_tx_topping_up_its_own_nonce_is_not_durable() {
        // Gap-#2 regression. A tx whose first instruction is AdvanceNonceAccount but
        // whose recent_blockhash is a REAL blockhash (not the stored nonce) is a normal
        // tx, so on failure the nonce rolls back with everything else — unlike a durable
        // one, which keeps the advance. The old check asked "is recent_blockhash in the
        // 150-entry RecentBlockhashes set?" and mis-routed this to the nonce path when
        // the blockhash sat one slot past that window (agave accepts age 150; the sysvar
        // holds only ages 0-149). We now compare against the account's stored nonce.
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
            bincode::serialize(&Versions::new(State::new_initialized(&payer, stored, fee))).unwrap();

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
        register_builtins(&mut bank, &replayer.processor);
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

        // Normal-path payoff: the nonce did NOT advance (a durable tx would have kept
        // it), and only the fee was charged.
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
        assert_eq!(payer_acct.lamports(), start - fee, "fee payer charged the fee");
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
        register_builtins(&mut bank, &replayer.processor);
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

        // Tx1: create the account and let the Vote program initialize its state,
        // so we never hand-build a VoteState.
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

        // Tx2: vote for `voted_slot` via TowerSync (what mainnet uses; the legacy
        // Vote instruction is deprecated under the current feature set). The program
        // reads SlotHashes from the sysvar cache to check the slot is real, so this
        // only passes because we seeded SlotHashes with (voted_slot, voted_hash).
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

        // A one-transfer block plus the meta the oracle reconciles against
        // (account order is [from, to, system]).
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
        let replayer = Replayer::new(base_slot, base_slot / 432_000);
        register_builtins(&mut bank, &replayer.processor);

        // Block N: src -> mid. Block N+1: mid -> dst, which only reconciles if
        // block N's write to `mid` rolled forward into the next slot.
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

        // A valid ed25519 signature over a message, built into a self-contained
        // precompile instruction, verifies through process_precompile.
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
}
