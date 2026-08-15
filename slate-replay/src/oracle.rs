//! Reconciliation oracle: does a replayed transaction match what the chain
//! actually did? Compares the replay result against the getBlock meta on what it
//! reports: the success/failure status, the fee, every account's post-balance
//! (lamports), and every touched token account's post amount.
//!
//! Scope: status, fee, lamports, and token amounts. getBlock does not carry
//! arbitrary post-account DATA, so a divergence that changes a non-token account's
//! data but not its lamports (a PDA field) is NOT caught here; that's the
//! differential harness's job at serve time. The rule here is conservative:
//! anything we cannot positively confirm becomes an issue, never a silent pass.

use std::collections::HashMap;

use solana_account::{AccountSharedData, ReadableAccount};
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

            // Successful: the executed accounts are the committed post-state.
            // Failed: everything rolls back except the fee payer (and any nonce),
            // exactly like fees-only — the executed accounts here still hold the
            // pre-rollback state — so reconcile the rollback accounts instead.
            let replay_post: HashMap<&Pubkey, u64> = if replay_ok {
                executed
                    .loaded_transaction
                    .accounts
                    .iter()
                    .map(|(key, account)| (key, account.lamports()))
                    .collect()
            } else {
                executed
                    .loaded_transaction
                    .rollback_accounts
                    .iter()
                    .map(|(key, account)| (key, account.lamports()))
                    .collect()
            };
            check_balances(account_keys, meta, &replay_post, &mut issues);
            // Token amounts, successful txs only: a failed tx's token accounts roll
            // back, so its post token balances just equal its pre balances.
            if replay_ok {
                check_token_balances(&executed.loaded_transaction.accounts, meta, &mut issues);
            }
        }
        // Replay only charged fees: the transaction failed to load. This matches
        // the chain only when the chain also failed (its meta carries an error) —
        // otherwise we failed where the chain succeeded. When both failed, only the
        // fee payer (plus any advanced nonce) moved, so its rollback balance must
        // match and every other account must be unchanged, which is exactly what
        // `check_balances` asserts. The fee is checked implicitly: the fee payer's
        // post balance is its pre balance minus the fee.
        Ok(ProcessedTransaction::FeesOnly(fees_only)) => {
            if meta.succeeded() {
                issues.push(format!(
                    "status: replay failed to load ({:?}), chain succeeded",
                    fees_only.load_error
                ));
            }
            let replay_post: HashMap<&Pubkey, u64> = fees_only
                .rollback_accounts
                .iter()
                .map(|(key, account)| (key, account.lamports()))
                .collect();
            check_balances(account_keys, meta, &replay_post, &mut issues);
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

/// Check the replay's token accounts against the chain's `postTokenBalances`. Each
/// entry names an account (by index into the tx's account list) that held a token
/// amount; the amount lives at offset 64 of an SPL Token / Token-2022 account's
/// data, which share that base layout. A mismatch, or a replay account too short
/// to hold an amount, is a divergence.
fn check_token_balances(
    replay_accounts: &[(Pubkey, AccountSharedData)],
    meta: &TxMeta,
    issues: &mut Vec<String>,
) {
    for tb in &meta.post_token_balances {
        let i = tb.account_index as usize;
        match replay_accounts.get(i) {
            Some((key, account)) => match account.data().get(64..72) {
                Some(bytes) => {
                    let replay_amount = u64::from_le_bytes(bytes.try_into().unwrap());
                    if replay_amount != tb.amount {
                        issues.push(format!(
                            "token amount {key}: replay {replay_amount}, chain {}",
                            tb.amount
                        ));
                    }
                }
                None => issues.push(format!(
                    "token account {key}: replay data too short ({} bytes) for an amount",
                    account.data().len()
                )),
            },
            None => issues.push(format!(
                "token balance references account index {i}, out of range in the replay"
            )),
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
        let result = replayer.execute(&bank, &tx, fixture::cpi::FEE, epoch, solana_hash::Hash::default());
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
            post_token_balances: vec![],
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

    /// Replay a tx that invokes an unregistered program: the fee payer is charged
    /// but the tx can't execute, producing a fees-only result. Payer starts at
    /// 10_000_000; account order is [payer, program].
    fn replay_fees_only() -> (Vec<Pubkey>, TransactionProcessingResult) {
        use crate::{ReplayBank, block::sanitize};
        use solana_account::{Account, AccountSharedData};
        use solana_instruction::{AccountMeta, Instruction};
        use solana_message::{Message, VersionedMessage};
        use solana_signature::Signature;
        use solana_transaction::versioned::VersionedTransaction;

        let slot = 300u64;
        let epoch = slot / 432_000;
        let system = solana_sdk_ids::system_program::id();
        let payer = Pubkey::new_unique();
        let fake_program = Pubkey::new_unique();

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

        let ix = Instruction {
            program_id: fake_program,
            accounts: vec![AccountMeta::new(payer, true)],
            data: vec![],
        };
        let message = Message::new(&[ix], Some(&payer));
        let tx = VersionedTransaction {
            signatures: vec![Signature::default()],
            message: VersionedMessage::Legacy(message),
        };
        let sanitized = sanitize(&tx, &LoadedAddresses::default()).unwrap();
        let account_keys: Vec<Pubkey> =
            sanitized.message().account_keys().iter().copied().collect();
        let result = replayer.execute(&bank, &sanitized, 5_000, epoch, solana_hash::Hash::default());
        (account_keys, result)
    }

    #[test]
    fn fees_only_matches_a_chain_failure() {
        let (account_keys, result) = replay_fees_only();
        assert!(
            matches!(&result, Ok(ProcessedTransaction::FeesOnly(_))),
            "expected a fees-only result, got {result:?}"
        );

        // The chain also failed (err set): fee charged to the payer, program
        // unchanged. This must reconcile now instead of false-halting.
        let meta = TxMeta {
            err: Some("InstructionError".into()),
            fee: 5_000,
            compute_units_consumed: 0,
            pre_balances: vec![10_000_000, 0],
            post_balances: vec![10_000_000 - 5_000, 0],
            loaded_addresses: LoadedAddresses::default(),
            post_token_balances: vec![],
        };
        let rec = reconcile(&account_keys, &meta, &result);
        assert!(
            rec.matched(),
            "a fees-only replay matching a chain failure should reconcile, got {:?}",
            rec.issues
        );
    }

    #[test]
    fn fees_only_flags_a_chain_success() {
        let (account_keys, result) = replay_fees_only();
        // The chain SUCCEEDED — our fees-only is a genuine divergence.
        let meta = TxMeta {
            err: None,
            fee: 5_000,
            compute_units_consumed: 0,
            pre_balances: vec![10_000_000, 0],
            post_balances: vec![10_000_000 - 5_000, 0],
            loaded_addresses: LoadedAddresses::default(),
            post_token_balances: vec![],
        };
        let rec = reconcile(&account_keys, &meta, &result);
        assert!(
            !rec.matched(),
            "fees-only vs a chain success must be flagged"
        );
        assert!(
            rec.issues.iter().any(|i| i.contains("status")),
            "should flag the status mismatch, got {:?}",
            rec.issues
        );
    }

    #[test]
    fn token_amounts_reconcile_and_flag() {
        use solana_account::Account;

        let key = Pubkey::new_unique();
        // An SPL token account: the raw amount (u64 LE) lives at offset 64.
        let mut data = vec![0u8; 165];
        data[64..72].copy_from_slice(&500u64.to_le_bytes());
        let token = AccountSharedData::from(Account {
            lamports: 2_039_280,
            data,
            owner: Pubkey::new_unique(),
            executable: false,
            rent_epoch: 0,
        });
        let replay = vec![(key, token)];

        let meta = |amount| TxMeta {
            err: None,
            fee: 5_000,
            compute_units_consumed: 0,
            pre_balances: vec![],
            post_balances: vec![],
            loaded_addresses: LoadedAddresses::default(),
            post_token_balances: vec![crate::block::TokenBalance {
                account_index: 0,
                mint: Pubkey::new_unique(),
                amount,
            }],
        };

        let mut ok = Vec::new();
        check_token_balances(&replay, &meta(500), &mut ok);
        assert!(
            ok.is_empty(),
            "a matching token amount should reconcile: {ok:?}"
        );

        let mut bad = Vec::new();
        check_token_balances(&replay, &meta(999), &mut bad);
        assert!(
            bad.iter().any(|i| i.contains("token amount")),
            "a wrong token amount must be flagged, got {bad:?}"
        );
    }

    /// Replay a transfer larger than the payer holds: it executes then fails on
    /// insufficient funds. Payer starts at 10_000_000; order [payer, dst, system].
    fn replay_failed_transfer() -> (Vec<Pubkey>, TransactionProcessingResult) {
        use crate::{ReplayBank, block::sanitize};
        use solana_account::{Account, AccountSharedData};
        use solana_instruction::{AccountMeta, Instruction};
        use solana_message::{Message, VersionedMessage};
        use solana_signature::Signature;
        use solana_transaction::versioned::VersionedTransaction;

        let slot = 300u64;
        let epoch = slot / 432_000;
        let system = solana_sdk_ids::system_program::id();
        let payer = Pubkey::new_unique();
        let dst = Pubkey::new_unique();

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

        let mut data = vec![2u8, 0, 0, 0]; // System Transfer
        data.extend_from_slice(&100_000_000u64.to_le_bytes()); // more than the payer holds
        let ix = Instruction {
            program_id: system,
            accounts: vec![AccountMeta::new(payer, true), AccountMeta::new(dst, false)],
            data,
        };
        let message = Message::new(&[ix], Some(&payer));
        let tx = VersionedTransaction {
            signatures: vec![Signature::default()],
            message: VersionedMessage::Legacy(message),
        };
        let sanitized = sanitize(&tx, &LoadedAddresses::default()).unwrap();
        let account_keys: Vec<Pubkey> =
            sanitized.message().account_keys().iter().copied().collect();
        let result = replayer.execute(&bank, &sanitized, 5_000, epoch, solana_hash::Hash::default());
        (account_keys, result)
    }

    #[test]
    fn reconciles_a_failed_executed_tx_via_rollback() {
        let (account_keys, result) = replay_failed_transfer();
        assert!(
            matches!(&result, Ok(ProcessedTransaction::Executed(e)) if !e.was_successful()),
            "expected an executed-but-failed tx, got {result:?}"
        );

        // The chain also failed: fee charged to the payer, transfer rolled back.
        let meta = TxMeta {
            err: Some("InstructionError".into()),
            fee: 5_000,
            compute_units_consumed: 0,
            pre_balances: vec![10_000_000, 0, 1],
            post_balances: vec![10_000_000 - 5_000, 0, 1],
            loaded_addresses: LoadedAddresses::default(),
            post_token_balances: vec![],
        };
        let rec = reconcile(&account_keys, &meta, &result);
        assert!(
            rec.matched(),
            "a failed-executed tx should reconcile against a chain failure, got {:?}",
            rec.issues
        );
    }
}
