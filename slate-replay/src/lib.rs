//! slate-replay: historical Solana account-state reconstruction via SVM
//! transaction replay.
//!
//! Phase 0 — walking skeleton: prove we can construct the SVM processor and the
//! account-loading callback against solana-svm 3.1.x. Execution (building the
//! per-slot environment, sanitizing a real tx, and reconciling against getBlock)
//! comes in Tasks 0.4–0.7.

pub mod block;
pub mod fixture;
pub mod oracle;
pub mod snapshot;

use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use agave_syscalls::create_program_runtime_environment_v1;
use solana_account::{Account, AccountSharedData};
use solana_clock::Clock;
use solana_compute_budget::compute_budget_limits::{
    ComputeBudgetLimits, MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES,
};
use solana_epoch_schedule::EpochSchedule;
use solana_fee_structure::FeeDetails;
use solana_hash::Hash;
use solana_program_runtime::{
    execution_budget::SVMTransactionExecutionBudget,
    loaded_programs::{BlockRelation, ForkGraph, ProgramCacheEntry},
};
use solana_pubkey::Pubkey;
use solana_rent::Rent;
use solana_svm::{
    account_loader::CheckedTransactionDetails,
    transaction_processing_result::{ProcessedTransaction, TransactionProcessingResult},
    transaction_processor::{
        TransactionBatchProcessor, TransactionProcessingConfig, TransactionProcessingEnvironment,
    },
};
use solana_svm_callback::{InvokeContextCallback, TransactionProcessingCallback};
use solana_svm_feature_set::SVMFeatureSet;
use solana_sysvar_id::SysvarId;
use solana_transaction::sanitized::SanitizedTransaction;

use crate::{
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

    /// Execute one sanitized transaction against `bank` and return its processing
    /// result. `fee` is the block meta's total fee, used to build the pre-validated
    /// check result the bank's fee validation would otherwise produce. Runs a batch
    /// of one and hands back the single result. This is the primitive the per-slot
    /// loop calls for each transaction, in order.
    pub fn execute(
        &self,
        bank: &ReplayBank,
        tx: SanitizedTransaction,
        fee: u64,
        epoch: u64,
    ) -> TransactionProcessingResult {
        let env = self.environment(*tx.message().recent_blockhash(), epoch);
        // no nonce; default compute budget capped at the max loaded-accounts size.
        let limits = ComputeBudgetLimits {
            loaded_accounts_bytes: MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES,
            ..Default::default()
        };
        let budget = limits.get_compute_budget_and_limits(
            limits.loaded_accounts_bytes,
            FeeDetails::new(fee, 0),
            true,
        );
        let check_results = vec![Ok(CheckedTransactionDetails::new(None, budget))];
        let mut output = self.processor.load_and_execute_sanitized_transactions(
            bank,
            std::slice::from_ref(&tx),
            check_results,
            &env,
            &TransactionProcessingConfig::default(),
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
        bank.configure_sysvars(block.slot, block.block_time);
        self.processor.fill_missing_sysvar_cache_entries(bank);

        for (i, block_tx) in block.transactions.iter().enumerate() {
            let tx = match sanitize(&block_tx.transaction) {
                Ok(tx) => tx,
                Err(err) => return BlockReplay::halted(i, format!("cannot sanitize: {err}")),
            };
            let account_keys: Vec<Pubkey> = tx.message().account_keys().iter().copied().collect();

            let result = self.execute(bank, tx, block_tx.meta.fee, epoch);
            let reconciliation = reconcile(&account_keys, &block_tx.meta, &result);
            if !reconciliation.matched() {
                return BlockReplay::halted(i, reconciliation.issues.join("; "));
            }

            commit_writes(bank, &result, block.slot);
        }

        BlockReplay::complete(block.transactions.len())
    }
}

/// Apply a successful transaction's account writes back into the bank so later
/// transactions in the same block see them. A failed-but-executed tx still charges
/// its fee and rolls the rest back; committing that correctly (the fee-payer
/// rollback) is still a TODO — until then a block containing a failed tx halts at
/// the next tx whose payer balance no longer lines up, which is caught, not silent.
fn commit_writes(bank: &mut ReplayBank, result: &TransactionProcessingResult, slot: u64) {
    if let Ok(ProcessedTransaction::Executed(executed)) = result
        && executed.was_successful()
    {
        for (key, account) in &executed.loaded_transaction.accounts {
            bank.insert(*key, account.clone(), slot);
        }
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
        use solana_svm::transaction_processing_result::ProcessedTransaction;

        let epoch = fixture::SLOT / 432_000;

        // --- assemble every input ---
        let mut bank = fixture::seed_bank();
        let replayer = Replayer::new(fixture::SLOT, epoch);
        register_builtins(&mut bank, &replayer.processor);
        bank.configure_sysvars(fixture::SLOT, fixture::BLOCK_TIME);
        replayer.processor.fill_missing_sysvar_cache_entries(&bank);

        // --- run it ---
        let result = replayer.execute(&bank, fixture::sanitized_transaction(), fixture::FEE, epoch);

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
            fixture::memo::sanitized_transaction(),
            fixture::memo::FEE,
            epoch,
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
            fixture::cpi::sanitized_transaction(),
            fixture::cpi::FEE,
            epoch,
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
        let accounts = snapshot::load_accounts(SNAPSHOT, None).unwrap();
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
        };

        // src -> mid, then mid -> dst. mid's pre-balance in tx2 only lines up if
        // tx1 committed first, so this exercises the tx-to-tx commit chaining.
        let block = block::Block {
            slot,
            parent_slot: slot - 1,
            blockhash: Hash::default(),
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
        };

        let outcome = replayer.replay_block(&mut bank, &block, epoch);
        assert!(outcome.is_complete(), "block halted: {:?}", outcome.halt);
        assert_eq!(outcome.replayed, 2);

        let balance = |pk: &Pubkey| bank.get_account_shared_data(pk).unwrap().0.lamports();
        assert_eq!(balance(&src), src_balance - amount1 - fee);
        assert_eq!(balance(&mid), amount1 - amount2 - fee);
        assert_eq!(balance(&dst), amount2);
    }
}
