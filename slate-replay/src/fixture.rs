//! Phase 0 test fixture: one real mainnet SOL transfer, used as both the input
//! to the SVM and the reconciliation oracle for the walking skeleton.
//!
//! tx `2B5TKSri…goDJc` at slot 437,390,844 (epoch 1012): a bare, single-
//! instruction System transfer. No snapshot needed — the two wallet accounts
//! are fully described by their pre-balances (system-owned, no data).

use std::collections::HashSet;

use base64::Engine;
use solana_account::{Account, AccountSharedData};
use solana_transaction::{Transaction, sanitized::SanitizedTransaction};

use crate::ReplayBank;

/// Slot the transaction executed in.
pub const SLOT: u64 = 437_390_844;
/// Block time for SLOT — becomes `Clock.unix_timestamp` when we build sysvars.
pub const BLOCK_TIME: i64 = 1_785_937_873;

/// The serialized (legacy) `VersionedTransaction`, base64 as `getBlock` returns
/// it. Deserialized in a later step (0.6.1).
pub const TX_BASE64: &str = "ATq082EQa8wH8RtRiKiNnbTtnmKeltzlaGqCRtdk21Hg1hNU2kHxp+78foBpncMbQI5YC+glQYNv/SX15A0HpQ0BAAEDw4B6xeDlAaFibjEgbRMiMXS00HF5RzZfwTxZfKDTSdvZ36530GxvAWJRjG5qeKEIGQAV7QfgiUiVaKdl8H87NwAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAwiB0jNDlB6T4/1tzggIwNX7E0msTiBHqXHegtJdI4kMBAgIAAQwCAAAASIaYLwAAAAA=";

// Account keys, in the transaction's order (the meta balance arrays are indexed
// the same way).
pub const SENDER: &str = "EAA8V9ZkF7gA5pfavotinL9jRPFUKNTgdF4AS6rLaJpS";
pub const RECIPIENT: &str = "FfVJ8zpFFo12TerGXAm8yhDPxjyAZcprTrww6AV2TmH8";

// The oracle: `getBlock` meta for this tx.
pub const SENDER_PRE: u64 = 73_133_641_000;
pub const RECIPIENT_PRE: u64 = 0;
pub const SENDER_POST: u64 = 72_335_111_000;
pub const RECIPIENT_POST: u64 = 798_525_000;
pub const FEE: u64 = 5_000;
pub const COMPUTE_UNITS: u64 = 150;

/// A plain system-owned wallet: lamports only, no data, not executable.
fn system_wallet(lamports: u64) -> AccountSharedData {
    AccountSharedData::from(Account {
        lamports,
        data: Vec::new(),
        owner: solana_sdk_ids::system_program::id(),
        executable: false,
        rent_epoch: 0,
    })
}

/// Seed a fresh `ReplayBank` with the fixture's pre-state: the two wallets at
/// their pre-balances. The System program is deliberately NOT seeded here — it's
/// a builtin, provided by `register_builtins` in Task 0.5.
pub fn seed_bank() -> ReplayBank {
    let mut bank = ReplayBank::default();
    bank.insert(SENDER.parse().unwrap(), system_wallet(SENDER_PRE), SLOT);
    bank.insert(
        RECIPIENT.parse().unwrap(),
        system_wallet(RECIPIENT_PRE),
        SLOT,
    );
    bank
}

/// Decode the base64 transaction and sanitize it into the form the SVM accepts.
/// Legacy tx (no address tables), so an empty reserved-key set is fine — that's
/// exactly what Solana's own test helper uses. v0 txs will need the fuller
/// `try_new` path with ALT resolution later.
pub fn sanitized_transaction() -> SanitizedTransaction {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(TX_BASE64)
        .expect("valid base64");
    let tx: Transaction = bincode::deserialize(&bytes).expect("legacy transaction");
    SanitizedTransaction::try_from_legacy_transaction(tx, &HashSet::default())
        .expect("transaction sanitizes")
}

/// Second fixture: a legacy `transfer + memo` tx. The memo invokes the SPL Memo
/// BPF program, which forces the real program-loading + VM path. It reads no
/// account data, so all accounts are still reconstructable from pre-balances
/// (no snapshot). sig `2emmK4…PD6T`, slot 437,572,006.
pub mod memo {
    use std::collections::HashSet;

    use base64::Engine;
    use solana_account::{Account, AccountSharedData};
    use solana_transaction::{Transaction, sanitized::SanitizedTransaction};

    use crate::ReplayBank;

    pub const SLOT: u64 = 437_572_006;
    pub const BLOCK_TIME: i64 = 1_786_014_489;
    pub const TX_BASE64: &str = "AVKXB4r/Wts4YfeG+kbhz6yMR8h9TdH4N7bslz/ae9ZjxU9tZfWFLrt0riJb1VJWzMgjNP77fzdEwAxOcseAkQwBAAIEV9P/AiUTCbWOL7dsUx8qrRgqD0O0wGr1p24RvVPEZQgYgErrj78SzoJG4ctP7cnaCuetBHjxDs9RXD1jD0EsoQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABUpTWpkpIQZNJOhxYNo4fHw1td28kruB5B+oQEEFRI0o2Yt1+yPb3jGv7cYg3/KaPYhdZ/kaf8IaZm9wbDOC2QICAgABDAIAAAABAAAAAAAAAAMAG2lzc3VlOjExMDAwMjAyNjA4MDYwMDAwMjgwOA==";

    pub const SENDER: &str = "6uqxgxbsVJWLWJfKipEJ5n21Jq51nYba9aVSjnEdXSPy";
    pub const RECIPIENT: &str = "2eeFML5PsX5g2xidqvEx9mTRgEdN5bXv7JeFfZD2PC5n";
    pub const MEMO_PROGRAM: &str = "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr";

    pub const SENDER_PRE: u64 = 20_483_136_423;
    pub const RECIPIENT_PRE: u64 = 2_101_137;
    pub const SENDER_POST: u64 = 20_483_131_422;
    pub const RECIPIENT_POST: u64 = 2_101_138;
    pub const FEE: u64 = 5_000;
    pub const COMPUTE_UNITS: u64 = 11_295;
    pub const MEMO_PROGRAM_LAMPORTS: u64 = 523_015_135;

    /// The SPL Memo program's ELF, embedded so tests stay offline. It's a
    /// BPFLoader2 (non-upgradeable, immutable) program, so the current bytecode
    /// is exactly what ran at the historical slot.
    pub fn program_bytecode() -> &'static [u8] {
        include_bytes!("memo_program.so")
    }

    /// Seed the two wallets (from pre-balances) and the Memo program account,
    /// which is BPFLoader2-owned, executable, and holds the ELF bytecode.
    pub fn seed_bank() -> ReplayBank {
        let mut bank = ReplayBank::default();
        bank.insert(
            SENDER.parse().unwrap(),
            super::system_wallet(SENDER_PRE),
            SLOT,
        );
        bank.insert(
            RECIPIENT.parse().unwrap(),
            super::system_wallet(RECIPIENT_PRE),
            SLOT,
        );
        let program = AccountSharedData::from(Account {
            lamports: MEMO_PROGRAM_LAMPORTS,
            data: program_bytecode().to_vec(),
            owner: solana_sdk_ids::bpf_loader::id(),
            executable: true,
            rent_epoch: 0,
        });
        bank.insert(MEMO_PROGRAM.parse().unwrap(), program, SLOT);
        bank
    }

    pub fn sanitized_transaction() -> SanitizedTransaction {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(TX_BASE64)
            .expect("valid base64");
        let tx: Transaction = bincode::deserialize(&bytes).expect("legacy transaction");
        SanitizedTransaction::try_from_legacy_transaction(tx, &HashSet::default())
            .expect("transaction sanitizes")
    }
}

/// Third fixture: a legacy tx where an upgradeable BPF program CPIs
/// `System::transfer`. All non-program accounts are wallets (verified: two are
/// system-owned + empty, one was a fresh 0-lamport account being funded), so
/// still no snapshot. This exercises the CPI path + the *upgradeable* loader
/// (program stub + separate programdata) + ComputeBudget. sig `2SWPHXgR…r3N3g`.
pub mod cpi {
    use std::collections::HashSet;

    use base64::Engine;
    use solana_account::{Account, AccountSharedData};
    use solana_transaction::{Transaction, sanitized::SanitizedTransaction};

    use crate::ReplayBank;

    pub const SLOT: u64 = 437_680_849;
    pub const BLOCK_TIME: i64 = 1_786_060_479;
    pub const TX_BASE64: &str = "AkgDUD62ddyCoGtYzI6d3nyuxGPJu+XPBJ+9yr3RDaLXc1EXpiQnL5cDWIMFe3bnojhNjxf+QngY19yS0XfP4A8637FuCPyUul1OWTPCsklHeihZDxzE5o2DagaDSvGC0+bk9h5WZX64499AWX1xkJ4vYRbQE+Ymq+6RTYjRE8kJAgADBrug/m6DkGdjmZy24/oJzD/RW1lhEBodjaxttmT50z5Q0S4AaXAVmURXp3/lZ3lpaYbg/W6GslzgrDbaW0nKNhud3tKORlrx2YRBgmvkyN449iuef2HReRrz/X2EhJlpLAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAwZGb+UhFzL/7K26csOb57yM5bvF9xJrLEObOkAAAADZSsxaLpc7wSJMK/SYdP+YE9lXpUo0p5PK4A0ZfCvlhK+MjhfEAMGQZ4vLc+RLnsBF5VpzBrX65npuEIhqCMxCAwQACQMSJAEAAAAAAAQABQJAnAAABQQAAQIDGIdQRZVSAPDNUyxgAAAAAABAS0wAAAAAAA==";

    pub const PAYER: &str = "DdRd9iJqsuQWCb6Uoy8XbeWJHtCFzT96TkWFWGrokFq1";
    pub const WALLET1: &str = "F5YtngCQs6QCUdy2vqT6hMtFyNkLpkJSTQF2WZKV1y8e";
    pub const WALLET2: &str = "BdG5HYMNpd71FDY5pFe9DeEenvodBNpqSMC6SidPSh9u";
    pub const PROGRAM: &str = "FdDd6eCKiRTUDnQ9o466pDnpcks6kwPCVc1uqcMdScAf";
    pub const PROGRAMDATA: &str = "3VtDRgLtnSUFFaQDTwuP3r94LEi47xdVDvYnbzmHFbCU";

    pub const PAYER_PRE: u64 = 161_425_479;
    pub const WALLET1_PRE: u64 = 44_177_116_352;
    pub const WALLET2_PRE: u64 = 0;
    pub const PAYER_POST: u64 = 156_412_488;
    pub const WALLET1_POST: u64 = 44_175_557_312;
    pub const WALLET2_POST: u64 = 6_559_040;
    pub const FEE: u64 = 12_991;
    pub const COMPUTE_UNITS: u64 = 11_343;
    const PROGRAM_LAMPORTS: u64 = 1_141_442;
    const PROGRAMDATA_LAMPORTS: u64 = 3_487_607_280;
    /// 36-byte upgradeable stub: enum Program { programdata_address }.
    const PROGRAM_STUB_B64: &str = "AgAAACUdi2x9aB+XxzPVIajvRrPtyueG9y5LFEC/+Pm35IhR";

    pub fn sanitized_transaction() -> SanitizedTransaction {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(TX_BASE64)
            .expect("valid base64");
        let tx: Transaction = bincode::deserialize(&bytes).expect("legacy transaction");
        SanitizedTransaction::try_from_legacy_transaction(tx, &HashSet::default())
            .expect("transaction sanitizes")
    }

    pub fn versioned_transaction() -> solana_transaction::versioned::VersionedTransaction {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(TX_BASE64)
            .expect("valid base64");
        bincode::deserialize(&bytes).expect("versioned transaction")
    }

    /// The real getBlock meta for this tx, in canonical account order:
    /// [payer, wallet1, wallet2, System, ComputeBudget, FdDd].
    pub fn meta() -> crate::block::TxMeta {
        crate::block::TxMeta {
            err: None,
            fee: FEE,
            compute_units_consumed: COMPUTE_UNITS,
            pre_balances: vec![PAYER_PRE, WALLET1_PRE, WALLET2_PRE, 1, 1, PROGRAM_LAMPORTS],
            post_balances: vec![
                PAYER_POST,
                WALLET1_POST,
                WALLET2_POST,
                1,
                1,
                PROGRAM_LAMPORTS,
            ],
            loaded_addresses: crate::block::LoadedAddresses::default(),
        }
    }

    /// This fixture as a one-transaction block, for exercising the replay loop.
    pub fn block() -> crate::block::Block {
        crate::block::Block {
            slot: SLOT,
            parent_slot: SLOT - 1,
            blockhash: solana_hash::Hash::default(),
            block_time: BLOCK_TIME,
            transactions: vec![crate::block::BlockTx {
                transaction: versioned_transaction(),
                meta: meta(),
            }],
        }
    }

    pub fn seed_bank() -> ReplayBank {
        let mut bank = ReplayBank::default();
        bank.insert(
            PAYER.parse().unwrap(),
            super::system_wallet(PAYER_PRE),
            SLOT,
        );
        bank.insert(
            WALLET1.parse().unwrap(),
            super::system_wallet(WALLET1_PRE),
            SLOT,
        );
        bank.insert(
            WALLET2.parse().unwrap(),
            super::system_wallet(WALLET2_PRE),
            SLOT,
        );

        // Upgradeable program: the 36-byte stub points at the programdata account,
        // which holds the ELF and is loaded via that pointer even though it's not
        // in the transaction's account list.
        let stub = base64::engine::general_purpose::STANDARD
            .decode(PROGRAM_STUB_B64)
            .unwrap();
        let program = AccountSharedData::from(Account {
            lamports: PROGRAM_LAMPORTS,
            data: stub,
            owner: solana_sdk_ids::bpf_loader_upgradeable::id(),
            executable: true,
            rent_epoch: 0,
        });
        bank.insert(PROGRAM.parse().unwrap(), program, SLOT);
        let programdata = AccountSharedData::from(Account {
            lamports: PROGRAMDATA_LAMPORTS,
            data: include_bytes!("cpi_programdata.bin").to_vec(),
            owner: solana_sdk_ids::bpf_loader_upgradeable::id(),
            executable: false,
            rent_epoch: 0,
        });
        bank.insert(PROGRAMDATA.parse().unwrap(), programdata, SLOT);
        bank
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_account::ReadableAccount;
    use solana_pubkey::Pubkey;
    use solana_svm_callback::TransactionProcessingCallback;

    #[test]
    fn bank_serves_seeded_wallets() {
        let bank = seed_bank();

        // The SVM will ask for accounts through the callback; check it answers.
        let sender: Pubkey = SENDER.parse().unwrap();
        let (acct, slot) = bank
            .get_account_shared_data(&sender)
            .expect("sender should be seeded");
        assert_eq!(acct.lamports(), SENDER_PRE);
        assert_eq!(*acct.owner(), solana_sdk_ids::system_program::id());
        assert_eq!(slot, SLOT);

        // The recipient is present even at 0 lamports (the transfer funds it).
        let recipient: Pubkey = RECIPIENT.parse().unwrap();
        assert!(bank.get_account_shared_data(&recipient).is_some());
    }

    #[test]
    fn deserializes_and_sanitizes() {
        let tx = sanitized_transaction();
        let keys = tx.message().account_keys();
        assert_eq!(keys.len(), 3);
        assert_eq!(keys[0], SENDER.parse::<Pubkey>().unwrap());
        assert_eq!(keys[1], RECIPIENT.parse::<Pubkey>().unwrap());
    }

    #[test]
    fn memo_fixture_loads() {
        assert!(!memo::program_bytecode().is_empty());
        let tx = memo::sanitized_transaction();
        assert_eq!(tx.message().account_keys().len(), 4);
    }
}
