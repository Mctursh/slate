//! Reconciliation oracle: does a replayed transaction match what the chain
//! actually did? Compares the replay result against the getBlock meta on the
//! three things getBlock reports per account: the success/failure status, the
//! fee, and every account's post-balance (lamports).
//!
//! Scope: this checks LAMPORTS, status, and fee. getBlock does not carry
//! arbitrary post-account DATA, so a divergence that changes an account's data
//! but not its lamports (a token amount, a PDA field) is NOT caught here; that's
//! the differential harness's job at serve time. Token balances (`postTokenBalances`)
//! are a near-term addition. The rule here is conservative: anything we cannot
//! positively confirm becomes an issue, never a silent pass.

use std::collections::HashMap;

use solana_account::ReadableAccount;
use solana_pubkey::Pubkey;
use solana_svm::transaction_processing_result::{
    ProcessedTransaction, TransactionProcessingResult,
};

use crate::block::TxMeta;

/// The verdict for one transaction. `issues` is empty iff the replay reproduced
/// the chain's result. The loop halts the slot on any non-empty verdict.
pub struct Reconciliation {
    pub issues: Vec<String>,
}

impl Reconciliation {
    pub fn matched(&self) -> bool {
        self.issues.is_empty()
    }
}

/// Reconcile one replayed transaction against its on-chain meta. `account_keys`
/// is the transaction's full account list in canonical order (static keys plus
/// any lookup-table addresses), which is the order `pre_balances`/`post_balances`
/// are indexed by.
pub fn reconcile(
    account_keys: &[Pubkey],
    meta: &TxMeta,
    result: &TransactionProcessingResult,
) -> Reconciliation {
    let mut issues = Vec::new();

    match result {
        Ok(ProcessedTransaction::Executed(executed)) => {
            let replay_ok = executed.was_successful();
            if replay_ok != meta.succeeded() {
                issues.push(format!(
                    "status: replay {}, chain {}",
                    outcome(replay_ok),
                    outcome(meta.succeeded()),
                ));
            }

            let replay_fee = executed.loaded_transaction.fee_details.total_fee();
            if replay_fee != meta.fee {
                issues.push(format!("fee: replay {replay_fee}, chain {}", meta.fee));
            }

            let replay_post: HashMap<&Pubkey, u64> = executed
                .loaded_transaction
                .accounts
                .iter()
                .map(|(key, account)| (key, account.lamports()))
                .collect();
            check_balances(account_keys, meta, &replay_post, &mut issues);
        }
        // Replay only charged fees (the transaction failed to load). A block can
        // legitimately contain a fees-only tx, recorded with an error set, so this
        // is a real match ONLY when the chain also failed that way. v1 doesn't yet
        // distinguish it: it flags every FeesOnly, which is a false halt on a
        // genuine fees-only tx but never a wrong pass. TODO: when `meta.err` is
        // set, compare the fee-payer rollback balance instead of flagging.
        Ok(ProcessedTransaction::FeesOnly(fees_only)) => {
            issues.push(format!(
                "replay only charged fees ({:?}); chain err = {:?}",
                fees_only.load_error, meta.err
            ));
        }
        Err(err) => {
            issues.push(format!("replay could not process the transaction: {err:?}"));
        }
    }

    Reconciliation { issues }
}

fn outcome(ok: bool) -> &'static str {
    if ok { "succeeded" } else { "failed" }
}

fn check_balances(
    account_keys: &[Pubkey],
    meta: &TxMeta,
    replay_post: &HashMap<&Pubkey, u64>,
    issues: &mut Vec<String>,
) {
    if account_keys.len() != meta.post_balances.len() {
        issues.push(format!(
            "account count: {} keys vs {} post-balances",
            account_keys.len(),
            meta.post_balances.len()
        ));
        return;
    }

    for (i, key) in account_keys.iter().enumerate() {
        let chain_post = meta.post_balances[i];
        match replay_post.get(key) {
            Some(&replay) if replay == chain_post => {}
            Some(&replay) => {
                issues.push(format!(
                    "lamports {key}: replay {replay}, chain {chain_post}"
                ));
            }
            None => {
                // The replay result didn't return this account. That's fine only
                // if the chain didn't change its balance either (a read-only or
                // program account); otherwise it's a real omission.
                let chain_pre = meta.pre_balances.get(i).copied().unwrap_or(chain_post);
                if chain_pre != chain_post {
                    issues.push(format!(
                        "{key} changed on chain {chain_pre} -> {chain_post} but replay omitted it"
                    ));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Replayer, block::LoadedAddresses, fixture, register_builtins};

    /// Replay the CPI fixture and return its result plus canonical account keys.
    fn replay_cpi() -> (Vec<Pubkey>, TransactionProcessingResult) {
        let s = fixture::cpi::SLOT;
        let epoch = s / 432_000;
        let mut bank = fixture::cpi::seed_bank();
        let replayer = Replayer::new(s, epoch);
        register_builtins(&mut bank, &replayer.processor);
        bank.configure_sysvars(s, fixture::cpi::BLOCK_TIME);
        replayer.processor.fill_missing_sysvar_cache_entries(&bank);

        let tx = fixture::cpi::sanitized_transaction();
        let account_keys: Vec<Pubkey> = tx.message().account_keys().iter().copied().collect();
        let result = replayer.execute(&bank, tx, fixture::cpi::FEE, epoch);
        (account_keys, result)
    }

    /// Real getBlock meta for the CPI fixture tx (slot 437680849), in canonical
    /// account order: [payer, wallet1, wallet2, System, ComputeBudget, FdDd].
    fn cpi_meta() -> TxMeta {
        TxMeta {
            err: None,
            fee: 12_991,
            compute_units_consumed: 11_343,
            pre_balances: vec![161_425_479, 44_177_116_352, 0, 1, 1, 1_141_442],
            post_balances: vec![156_412_488, 44_175_557_312, 6_559_040, 1, 1, 1_141_442],
            loaded_addresses: LoadedAddresses::default(),
        }
    }

    #[test]
    fn reconciles_a_faithful_replay() {
        let (account_keys, result) = replay_cpi();
        let rec = reconcile(&account_keys, &cpi_meta(), &result);
        assert!(
            rec.matched(),
            "expected a clean match, got {:?}",
            rec.issues
        );
    }

    #[test]
    fn flags_a_balance_divergence() {
        let (account_keys, result) = replay_cpi();
        let mut meta = cpi_meta();
        meta.post_balances[2] += 1; // pretend the CPI recipient ended one lamport off
        let rec = reconcile(&account_keys, &meta, &result);
        assert!(!rec.matched(), "a wrong post-balance must be flagged");
        assert!(
            rec.issues.iter().any(|i| i.contains("lamports")),
            "issue should name the lamports divergence, got {:?}",
            rec.issues
        );
    }
}
