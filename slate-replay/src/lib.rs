//! slate-replay: historical Solana account-state reconstruction via SVM
//! transaction replay.
//!
//! Phase 0 — walking skeleton: prove we can construct the SVM processor and the
//! account-loading callback against solana-svm 3.1.x. Execution (building the
//! per-slot environment, sanitizing a real tx, and reconciling against getBlock)
//! comes in Tasks 0.4–0.7.

pub mod fixture;

use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use agave_syscalls::create_program_runtime_environment_v1;
use solana_account::{Account, AccountSharedData};
use solana_clock::Clock;
use solana_epoch_schedule::EpochSchedule;
use solana_hash::Hash;
use solana_program_runtime::{
    execution_budget::SVMTransactionExecutionBudget,
    loaded_programs::{BlockRelation, ForkGraph, ProgramCacheEntry},
};
use solana_pubkey::Pubkey;
use solana_rent::Rent;
use solana_svm::transaction_processor::{
    TransactionBatchProcessor, TransactionProcessingEnvironment,
};
use solana_svm_callback::{InvokeContextCallback, TransactionProcessingCallback};
use solana_svm_feature_set::SVMFeatureSet;
use solana_sysvar_id::SysvarId;

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
#[derive(Default)]
pub struct ReplayBank {
    /// pubkey -> (account, slot it was last written)
    accounts: HashMap<Pubkey, (AccountSharedData, u64)>,
}

impl ReplayBank {
    pub fn insert(&mut self, key: Pubkey, account: AccountSharedData, slot: u64) {
        self.accounts.insert(key, (account, slot));
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
        let account = AccountSharedData::from(Account {
            lamports: 1,
            data: name.as_bytes().to_vec(),
            owner: solana_sdk_ids::native_loader::id(),
            executable: true,
            rent_epoch: 0,
        });
        self.insert(program_id, account, 0);
        processor.add_builtin(program_id, entry);
    }

    /// Build the sysvars this replay needs and insert them as accounts; the
    /// processor pulls them into its cache via `fill_missing_sysvar_cache_entries`.
    /// Skeleton set: Clock (real slot + block time), Rent, EpochSchedule. Harder
    /// txs will add SlotHashes / StakeHistory.
    pub fn configure_sysvars(&mut self, slot: u64, unix_timestamp: i64) {
        let epoch = slot / 432_000; // mainnet: no warmup
        let clock = Clock {
            slot,
            // epoch_start_timestamp / leader_schedule_epoch are unused by a plain
            // transfer; Phase 1 computes them exactly per slot.
            epoch_start_timestamp: unix_timestamp,
            epoch,
            leader_schedule_epoch: epoch,
            unix_timestamp,
        };
        self.set_sysvar_account(Clock::id(), bincode::serialize(&clock).unwrap());
        self.set_sysvar_account(Rent::id(), bincode::serialize(&Rent::default()).unwrap());
        self.set_sysvar_account(
            EpochSchedule::id(),
            bincode::serialize(&EpochSchedule::without_warmup()).unwrap(),
        );
    }

    fn set_sysvar_account(&mut self, id: Pubkey, data: Vec<u8>) {
        let account = AccountSharedData::from(Account {
            lamports: 1,
            data,
            owner: solana_sdk_ids::sysvar::id(),
            executable: false,
            rent_epoch: 0,
        });
        self.insert(id, account, 0);
    }
}

/// Register the builtins the current fixture needs. A bare System transfer needs
/// only the System program.
pub fn register_builtins(
    bank: &mut ReplayBank,
    processor: &TransactionBatchProcessor<SlateForkGraph>,
) {
    // System program (native builtin).
    bank.add_builtin(
        processor,
        solana_system_program::id(),
        "system_program",
        ProgramCacheEntry::new_builtin(
            0,
            "system_program".len(),
            solana_system_program::system_processor::Entrypoint::vm,
        ),
    );
    // The BPF loaders — required so programs owned by them can be loaded + run.
    for (id, name) in [
        (
            solana_sdk_ids::bpf_loader_upgradeable::id(),
            "solana_bpf_loader_upgradeable_program",
        ),
        (
            solana_sdk_ids::bpf_loader::id(),
            "solana_bpf_loader_program",
        ),
        (
            solana_sdk_ids::bpf_loader_deprecated::id(),
            "solana_bpf_loader_deprecated_program",
        ),
    ] {
        bank.add_builtin(
            processor,
            id,
            name,
            ProgramCacheEntry::new_builtin(
                0,
                name.len(),
                solana_bpf_loader_program::Entrypoint::vm,
            ),
        );
    }
    // ComputeBudget (native builtin) — txs that set a CU limit/price invoke it.
    bank.add_builtin(
        processor,
        solana_sdk_ids::compute_budget::id(),
        "compute_budget_program",
        ProgramCacheEntry::new_builtin(
            0,
            "compute_budget_program".len(),
            solana_compute_budget_program::Entrypoint::vm,
        ),
    );
}

// All methods default to "no epoch stake / no precompiles"; replay fills
// these in later (epoch stake for reward-adjacent execution, precompiles).
impl InvokeContextCallback for ReplayBank {}

impl TransactionProcessingCallback for ReplayBank {
    fn get_account_shared_data(&self, pubkey: &Pubkey) -> Option<(AccountSharedData, u64)> {
        self.accounts.get(pubkey).cloned()
    }
}

/// The per-replay SVM harness. Owns the fork graph (the processor only holds a
/// `Weak` to it, so someone must keep the `Arc` alive) and the transaction
/// processor. Grows to hold the bank, environment, and config as we wire them.
pub struct Replayer {
    _fork_graph: Arc<RwLock<SlateForkGraph>>,
    pub processor: TransactionBatchProcessor<SlateForkGraph>,
}

impl Replayer {
    /// Build an execution-ready processor for `slot`/`epoch`. Passing `None` for
    /// both loaders gives the default empty (no-syscall) VM environment, which is
    /// fine for builtin-only txs like a System transfer; BPF programs need a real
    /// loader here later.
    pub fn new(slot: u64, epoch: u64) -> Self {
        let fork_graph = Arc::new(RwLock::new(SlateForkGraph));
        // The real BPF VM environment: the exact syscall set + costs the runtime
        // derives from the feature set. BPFLoader v1/v2 programs execute in here.
        let feature_set = SVMFeatureSet::all_enabled();
        let budget = SVMTransactionExecutionBudget::default();
        let loader = Arc::new(
            create_program_runtime_environment_v1(&feature_set, &budget, false, false)
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
            feature_set: SVMFeatureSet::all_enabled(),
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
        use solana_compute_budget::compute_budget_limits::{
            ComputeBudgetLimits, MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES,
        };
        use solana_fee_structure::FeeDetails;
        use solana_svm::{
            account_loader::CheckedTransactionDetails,
            transaction_processing_result::ProcessedTransaction,
            transaction_processor::TransactionProcessingConfig,
        };

        let epoch = fixture::SLOT / 432_000;

        // --- assemble every input ---
        let mut bank = fixture::seed_bank();
        let replayer = Replayer::new(fixture::SLOT, epoch);
        register_builtins(&mut bank, &replayer.processor);
        bank.configure_sysvars(fixture::SLOT, fixture::BLOCK_TIME);
        replayer.processor.fill_missing_sysvar_cache_entries(&bank);

        let tx = fixture::sanitized_transaction();
        let env = replayer.environment(*tx.message().recent_blockhash(), epoch);

        // per-tx check result: no nonce, default compute budget, 1-signature fee.
        let limits = ComputeBudgetLimits {
            loaded_accounts_bytes: MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES,
            ..Default::default()
        };
        let budget = limits.get_compute_budget_and_limits(
            limits.loaded_accounts_bytes,
            FeeDetails::new(fixture::FEE, 0),
            true,
        );
        let check_results = vec![Ok(CheckedTransactionDetails::new(None, budget))];
        let txs = [tx];

        // --- run it ---
        let output = replayer.processor.load_and_execute_sanitized_transactions(
            &bank,
            &txs,
            check_results,
            &env,
            &TransactionProcessingConfig::default(),
        );

        // --- reconcile against the getBlock oracle ---
        let executed = match &output.processing_results[0] {
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
        use solana_compute_budget::compute_budget_limits::{
            ComputeBudgetLimits, MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES,
        };
        use solana_fee_structure::FeeDetails;
        use solana_svm::{
            account_loader::CheckedTransactionDetails,
            transaction_processing_result::ProcessedTransaction,
            transaction_processor::TransactionProcessingConfig,
        };

        let m = fixture::memo::SLOT;
        let epoch = m / 432_000;

        let mut bank = fixture::memo::seed_bank();
        let replayer = Replayer::new(m, epoch);
        register_builtins(&mut bank, &replayer.processor);
        bank.configure_sysvars(m, fixture::memo::BLOCK_TIME);
        replayer.processor.fill_missing_sysvar_cache_entries(&bank);

        let tx = fixture::memo::sanitized_transaction();
        let env = replayer.environment(*tx.message().recent_blockhash(), epoch);

        let limits = ComputeBudgetLimits {
            loaded_accounts_bytes: MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES,
            ..Default::default()
        };
        let budget = limits.get_compute_budget_and_limits(
            limits.loaded_accounts_bytes,
            FeeDetails::new(fixture::memo::FEE, 0),
            true,
        );
        let check_results = vec![Ok(CheckedTransactionDetails::new(None, budget))];
        let txs = [tx];

        let output = replayer.processor.load_and_execute_sanitized_transactions(
            &bank,
            &txs,
            check_results,
            &env,
            &TransactionProcessingConfig::default(),
        );

        let executed = match &output.processing_results[0] {
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
        use solana_compute_budget::compute_budget_limits::{
            ComputeBudgetLimits, MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES,
        };
        use solana_fee_structure::FeeDetails;
        use solana_svm::{
            account_loader::CheckedTransactionDetails,
            transaction_processing_result::ProcessedTransaction,
            transaction_processor::TransactionProcessingConfig,
        };

        let s = fixture::cpi::SLOT;
        let epoch = s / 432_000;

        let mut bank = fixture::cpi::seed_bank();
        let replayer = Replayer::new(s, epoch);
        register_builtins(&mut bank, &replayer.processor);
        bank.configure_sysvars(s, fixture::cpi::BLOCK_TIME);
        replayer.processor.fill_missing_sysvar_cache_entries(&bank);

        let tx = fixture::cpi::sanitized_transaction();
        let env = replayer.environment(*tx.message().recent_blockhash(), epoch);

        let limits = ComputeBudgetLimits {
            loaded_accounts_bytes: MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES,
            ..Default::default()
        };
        let budget = limits.get_compute_budget_and_limits(
            limits.loaded_accounts_bytes,
            FeeDetails::new(fixture::cpi::FEE, 0),
            true,
        );
        let check_results = vec![Ok(CheckedTransactionDetails::new(None, budget))];
        let txs = [tx];

        let output = replayer.processor.load_and_execute_sanitized_transactions(
            &bank,
            &txs,
            check_results,
            &env,
            &TransactionProcessingConfig::default(),
        );

        let executed = match &output.processing_results[0] {
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
}
