// Core-BPF migrations that agave 3.1.14 can no longer perform. The migration machinery still
// exists in solana-runtime, but `solana-builtins` dropped each config once its migration landed,
// so the inputs are re-supplied here from solana-builtins 2.3.13, the last version carrying them.
//
// Mirrors Bank::migrate_builtin_to_core_bpf. Capitalization is deliberately NOT adjusted: every
// write goes through ReplayBank::insert, so the burn/fund falls out of the per-slot lamport
// deltas the lattice already tracks, and adjusting here as agave does would double-count.

use solana_account::{AccountSharedData, ReadableAccount, WritableAccount};
use solana_loader_v3_interface::{get_program_data_address, state::UpgradeableLoaderState};
use solana_program_runtime::loaded_programs::ProgramCacheForTxBatch;
use solana_pubkey::Pubkey;
use solana_svm::transaction_processor::TransactionBatchProcessor;
use solana_svm_callback::TransactionProcessingCallback;

use crate::{ReplayBank, SlateForkGraph};

// solana-builtins 2.3.13: BUILTINS["stake_program"].core_bpf_migration_config.
// upgrade_authority_address: None, verified_build_hash: None, target: Builtin.
pub const STAKE_SOURCE_BUFFER: Pubkey =
    Pubkey::from_str_const("8t3vv6v99tQA6Gp7fVdsBH66hQMaswH5qsJVqJqo8xvG");

/// Every account a core-BPF migration reads that no transaction in a range would touch.
/// The snapshot seeder filters on the range's transaction keys, so without this the buffer
/// is absent and the migration refuses.
// agave_feature_set::vote_state_v4::stake_program_buffer, the SIMD-0185 upgrade source.
pub const STAKE_V4_SOURCE_BUFFER: Pubkey =
    Pubkey::from_str_const("BM11F4hqrpinQs28sEZfzQ2fYddivYs4NEAHF6QMjkJF");

pub const MIGRATION_SOURCE_BUFFERS: &[Pubkey] = &[STAKE_SOURCE_BUFFER, STAKE_V4_SOURCE_BUFFER];

/// solana-builtins 2.3.13 sets `upgrade_authority_address: None` for this migration.
const STAKE_UPGRADE_AUTHORITY: Option<Pubkey> = None;

#[derive(Debug, PartialEq, Eq)]
pub enum MigrationError {
    ProgramMissing,
    BufferMissing,
    /// The programdata account already exists. agave only tolerates a prefunded system account
    /// here once `create_account_allow_prefund` is active (epoch 979), which is past this era.
    ProgramDataExists,
    /// The buffer account doesn't deserialize as `UpgradeableLoaderState::Buffer`.
    InvalidBuffer,
    /// The buffer's authority doesn't match the config's.
    AuthorityMismatch,
    /// The ELF failed to load or verify against the current runtime environment.
    Deploy,
    /// An upgrade target has no programdata account, so it is not a deployed loader-v3 program.
    ProgramDataMissing,
}

/// What the migration wrote, for the caller to log and for tests to assert on.
pub struct Migrated {
    pub program_address: Pubkey,
    pub program_data_address: Pubkey,
    pub burned: u64,
    pub funded: u64,
}

/// Stake program, native builtin -> core BPF, at the first block of epoch 823.
pub fn migrate_stake_to_core_bpf(
    bank: &mut ReplayBank,
    processor: &TransactionBatchProcessor<SlateForkGraph>,
    parent: &TransactionBatchProcessor<SlateForkGraph>,
    epoch: u64,
    slot: u64,
) -> Result<Migrated, MigrationError> {
    let program_address = solana_sdk_ids::stake::id();
    let program_data_address = get_program_data_address(&program_address);

    let (program_account, _) = bank
        .get_account_shared_data(&program_address)
        .ok_or(MigrationError::ProgramMissing)?;
    let (buffer_account, _) = bank
        .get_account_shared_data(&STAKE_SOURCE_BUFFER)
        .ok_or(MigrationError::BufferMissing)?;
    if bank
        .get_account_shared_data(&program_data_address)
        .is_some()
    {
        return Err(MigrationError::ProgramDataExists);
    }

    // agave only compares authorities when the CONFIG supplies one. Stake's config carries
    // upgrade_authority_address: None, so the buffer's own authority is simply not checked and
    // the new programdata gets None. Rejecting a buffer that has an authority is a constraint
    // agave does not impose, and the real epoch-823 buffer has one.
    let metadata_size = UpgradeableLoaderState::size_of_buffer_metadata();
    let buffer_authority = match bincode::deserialize(
        buffer_account
            .data()
            .get(..metadata_size)
            .ok_or(MigrationError::InvalidBuffer)?,
    ) {
        Ok(UpgradeableLoaderState::Buffer { authority_address }) => authority_address,
        _ => return Err(MigrationError::InvalidBuffer),
    };
    if let Some(configured) = STAKE_UPGRADE_AUTHORITY
        && Some(configured) != buffer_authority
    {
        return Err(MigrationError::AuthorityMismatch);
    }
    let elf = &buffer_account.data()[metadata_size..];

    let loader = solana_sdk_ids::bpf_loader_upgradeable::id();

    let mut new_program = AccountSharedData::new_data(
        bank.minimum_balance(UpgradeableLoaderState::size_of_program()),
        &UpgradeableLoaderState::Program {
            programdata_address: program_data_address,
        },
        &loader,
    )
    .map_err(|_| MigrationError::InvalidBuffer)?;
    new_program.set_executable(true);

    let programdata_metadata_size = UpgradeableLoaderState::size_of_programdata_metadata();
    let space = programdata_metadata_size + elf.len();
    let mut new_program_data = AccountSharedData::new_data_with_space(
        bank.minimum_balance(space),
        &UpgradeableLoaderState::ProgramData {
            slot,
            upgrade_authority_address: None,
        },
        space,
        &loader,
    )
    .map_err(|_| MigrationError::InvalidBuffer)?;
    new_program_data.data_as_mut_slice()[programdata_metadata_size..].copy_from_slice(elf);

    let burned = program_account.lamports() + buffer_account.lamports();
    let funded = new_program.lamports() + new_program_data.lamports();

    // Deploy before the writes borrow ends, so the cache entry exists for the same slot the
    // accounts do. deployment_slot = slot gives effective_slot = slot + 1, which is how agave
    // makes the program un-invokable for the rest of the migration slot.
    let account_size = UpgradeableLoaderState::size_of_program() + new_program_data.data().len();
    let mut batch_cache = ProgramCacheForTxBatch::new(slot);
    let environments = processor.get_environments_for_epoch(epoch);
    solana_bpf_loader_program::deploy_program(
        None,
        &mut batch_cache,
        environments.program_runtime_v1.clone(),
        &program_address,
        &loader,
        account_size,
        elf,
        slot,
    )
    .map_err(|_| MigrationError::Deploy)?;
    processor
        .global_program_cache
        .write()
        .unwrap()
        .merge(&environments, &batch_cache.drain_modified_entries());

    // Stop dispatching it as a native builtin; from here it executes as BPF. Both processors:
    // new_from CLONES builtin_program_ids rather than sharing it, so the per-slot one covers this
    // slot and the parent covers every slot cloned from it afterwards.
    for p in [processor, parent] {
        p.builtin_program_ids
            .write()
            .unwrap()
            .remove(&program_address);
    }

    bank.insert(program_address, new_program, slot);
    bank.insert(program_data_address, new_program_data, slot);
    // Cleared, not removed: a zero-lamport account is dead and hashes to the lattice identity.
    bank.insert(STAKE_SOURCE_BUFFER, AccountSharedData::default(), slot);

    Ok(Migrated {
        program_address,
        program_data_address,
        burned,
        funded,
    })
}

/// Stake program, core BPF -> core BPF, at the first block of epoch 949 (SIMD-0185).
pub fn upgrade_stake_for_vote_state_v4(
    bank: &mut ReplayBank,
    processor: &TransactionBatchProcessor<SlateForkGraph>,
    epoch: u64,
    slot: u64,
) -> Result<Migrated, MigrationError> {
    let program_address = solana_sdk_ids::stake::id();
    let program_data_address = get_program_data_address(&program_address);
    let loader = solana_sdk_ids::bpf_loader_upgradeable::id();

    let (program_account, _) = bank
        .get_account_shared_data(&program_address)
        .ok_or(MigrationError::ProgramMissing)?;
    if program_account.owner() != &loader || !program_account.executable() {
        return Err(MigrationError::ProgramMissing);
    }
    let (program_data_account, _) = bank
        .get_account_shared_data(&program_data_address)
        .ok_or(MigrationError::ProgramDataMissing)?;
    let (buffer_account, _) = bank
        .get_account_shared_data(&STAKE_V4_SOURCE_BUFFER)
        .ok_or(MigrationError::BufferMissing)?;

    let programdata_metadata_size = UpgradeableLoaderState::size_of_programdata_metadata();
    let upgrade_authority_address = match bincode::deserialize(
        program_data_account
            .data()
            .get(..programdata_metadata_size)
            .ok_or(MigrationError::ProgramDataMissing)?,
    ) {
        Ok(UpgradeableLoaderState::ProgramData {
            upgrade_authority_address,
            ..
        }) => upgrade_authority_address,
        _ => return Err(MigrationError::ProgramDataMissing),
    };

    let metadata_size = UpgradeableLoaderState::size_of_buffer_metadata();
    let buffer_authority = match bincode::deserialize(
        buffer_account
            .data()
            .get(..metadata_size)
            .ok_or(MigrationError::InvalidBuffer)?,
    ) {
        Ok(UpgradeableLoaderState::Buffer { authority_address }) => authority_address,
        _ => return Err(MigrationError::InvalidBuffer),
    };
    // agave only compares when the TARGET carries an authority; None skips the check.
    if upgrade_authority_address.is_some() && upgrade_authority_address != buffer_authority {
        return Err(MigrationError::AuthorityMismatch);
    }
    let elf = &buffer_account.data()[metadata_size..];

    let space = programdata_metadata_size + elf.len();
    let mut new_program_data = AccountSharedData::new_data_with_space(
        bank.minimum_balance(space),
        &UpgradeableLoaderState::ProgramData {
            slot,
            upgrade_authority_address,
        },
        space,
        &loader,
    )
    .map_err(|_| MigrationError::InvalidBuffer)?;
    new_program_data.data_as_mut_slice()[programdata_metadata_size..].copy_from_slice(elf);

    // The program account survives, so only the old programdata and the buffer are burned.
    let burned = program_data_account.lamports() + buffer_account.lamports();
    let funded = new_program_data.lamports();

    let account_size = UpgradeableLoaderState::size_of_program() + new_program_data.data().len();
    let mut batch_cache = ProgramCacheForTxBatch::new(slot);
    let environments = processor.get_environments_for_epoch(epoch);
    solana_bpf_loader_program::deploy_program(
        None,
        &mut batch_cache,
        environments.program_runtime_v1.clone(),
        &program_address,
        &loader,
        account_size,
        elf,
        slot,
    )
    .map_err(|_| MigrationError::Deploy)?;
    processor
        .global_program_cache
        .write()
        .unwrap()
        .merge(&environments, &batch_cache.drain_modified_entries());

    bank.insert(program_data_address, new_program_data, slot);
    bank.insert(STAKE_V4_SOURCE_BUFFER, AccountSharedData::default(), slot);

    Ok(Migrated {
        program_address,
        program_data_address,
        burned,
        funded,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Replayer;
    use solana_account::Account;

    fn buffer(authority: Option<Pubkey>, elf: &[u8], lamports: u64) -> AccountSharedData {
        let mut data = bincode::serialize(&UpgradeableLoaderState::Buffer {
            authority_address: authority,
        })
        .unwrap();
        data.resize(UpgradeableLoaderState::size_of_buffer_metadata(), 0);
        data.extend_from_slice(elf);
        AccountSharedData::from(Account {
            lamports,
            data,
            owner: solana_sdk_ids::bpf_loader_upgradeable::id(),
            executable: false,
            rent_epoch: 0,
        })
    }

    fn native_stake(lamports: u64) -> AccountSharedData {
        AccountSharedData::from(Account {
            lamports,
            data: b"stake".to_vec(),
            owner: solana_sdk_ids::native_loader::id(),
            executable: true,
            rent_epoch: 0,
        })
    }

    // A real ELF: deploy_program loads and verifies it against the runtime environment, so
    // arbitrary bytes are rejected. SPL Memo is the fixture already embedded for offline tests.
    fn elf() -> &'static [u8] {
        crate::fixture::memo::program_bytecode()
    }

    const EPOCH: u64 = 823;

    fn bank_at_823(elf: &[u8]) -> (ReplayBank, Replayer, u64) {
        let slot = EPOCH * 432_000;
        let mut bank = ReplayBank::default();
        bank.insert(solana_sdk_ids::stake::id(), native_stake(1), slot);
        bank.insert(STAKE_SOURCE_BUFFER, buffer(None, elf, 5_000_000), slot);
        (bank, Replayer::new(slot, EPOCH), slot)
    }

    #[test]
    fn writes_the_program_the_programdata_and_clears_the_buffer() {
        let elf = elf();
        let (mut bank, replayer, slot) = bank_at_823(elf);

        let out = migrate_stake_to_core_bpf(
            &mut bank,
            &replayer.processor,
            &replayer.processor,
            EPOCH,
            slot,
        )
        .unwrap();
        let loader = solana_sdk_ids::bpf_loader_upgradeable::id();

        let (program, _) = bank.get_account_shared_data(&out.program_address).unwrap();
        assert!(
            program.executable(),
            "the program account must be executable"
        );
        assert_eq!(*program.owner(), loader);
        assert_eq!(
            bincode::deserialize::<UpgradeableLoaderState>(program.data()).unwrap(),
            UpgradeableLoaderState::Program {
                programdata_address: out.program_data_address
            }
        );

        let (data, _) = bank
            .get_account_shared_data(&out.program_data_address)
            .unwrap();
        assert_eq!(*data.owner(), loader);
        assert!(!data.executable(), "programdata is not itself executable");
        let meta = UpgradeableLoaderState::size_of_programdata_metadata();
        assert_eq!(
            bincode::deserialize::<UpgradeableLoaderState>(&data.data()[..meta]).unwrap(),
            UpgradeableLoaderState::ProgramData {
                slot,
                upgrade_authority_address: None
            }
        );
        assert_eq!(&data.data()[meta..], elf, "the ELF is copied verbatim");

        // Drained to zero lamports, which is dead on chain: it reads back as absent and
        // contributes the lattice identity, so mixing it out cancels what was mixed in.
        assert!(
            bank.get_account_shared_data(&STAKE_SOURCE_BUFFER).is_none(),
            "the drained buffer must read as absent"
        );
    }

    // The lamport movement must equal agave's explicit burn/fund, since Slate gets it from the
    // per-slot deltas instead. Both new accounts are rent-exempt for their exact size.
    #[test]
    fn the_lamport_movement_matches_agaves_burn_and_fund() {
        let elf = elf();
        let (mut bank, replayer, slot) = bank_at_823(elf);
        let out = migrate_stake_to_core_bpf(
            &mut bank,
            &replayer.processor,
            &replayer.processor,
            EPOCH,
            slot,
        )
        .unwrap();

        assert_eq!(out.burned, 1 + 5_000_000, "stake account + buffer");
        let expected = bank.minimum_balance(UpgradeableLoaderState::size_of_program())
            + bank.minimum_balance(
                UpgradeableLoaderState::size_of_programdata_metadata() + elf.len(),
            );
        assert_eq!(out.funded, expected);
    }

    // The point of deploying through the loader rather than letting the SVM find the account
    // later: agave's entry is effective at slot + 1, so a stake transaction in the migration
    // slot itself must fail as not-deployed. Loading lazily would make it invokable immediately.
    #[test]
    fn the_migrated_program_is_not_invokable_until_the_next_slot() {
        let (mut bank, replayer, slot) = bank_at_823(elf());
        migrate_stake_to_core_bpf(
            &mut bank,
            &replayer.processor,
            &replayer.processor,
            EPOCH,
            slot,
        )
        .unwrap();

        let cache = replayer.processor.global_program_cache.read().unwrap();
        let versions = cache.get_slot_versions_for_tests(&solana_sdk_ids::stake::id());
        let deployed = versions
            .iter()
            .find(|e| e.deployment_slot == slot)
            .expect("the migrated program must be in the cache");
        assert_eq!(
            deployed.effective_slot,
            slot + 1,
            "delay visibility: not usable in the migration slot"
        );
    }

    // Until this, the native processor keeps being dispatched and the BPF program never runs.
    #[test]
    fn the_native_builtin_stops_being_dispatched() {
        let (mut bank, replayer, slot) = bank_at_823(elf());
        let stake = solana_sdk_ids::stake::id();
        replayer.processor.add_builtin(
            stake,
            solana_program_runtime::loaded_programs::ProgramCacheEntry::new_builtin(
                0,
                "stake".len(),
                crate::compat::stake::Entrypoint::vm,
            ),
        );
        assert!(
            replayer
                .processor
                .builtin_program_ids
                .read()
                .unwrap()
                .contains(&stake)
        );

        migrate_stake_to_core_bpf(
            &mut bank,
            &replayer.processor,
            &replayer.processor,
            EPOCH,
            slot,
        )
        .unwrap();

        assert!(
            !replayer
                .processor
                .builtin_program_ids
                .read()
                .unwrap()
                .contains(&stake),
            "the stake builtin must be un-registered by the migration"
        );
    }

    // new_from CLONES builtin_program_ids (unlike global_program_cache and
    // epoch_boundary_preparation, which are shared Arcs). So un-registering on the migration
    // slot's processor alone is lost: every later slot gets a fresh copy from the parent, which
    // would still dispatch the native processor and never run the BPF program.
    #[test]
    fn the_un_registration_survives_into_later_slots() {
        let (mut bank, replayer, slot) = bank_at_823(elf());
        let stake = solana_sdk_ids::stake::id();
        replayer.processor.add_builtin(
            stake,
            solana_program_runtime::loaded_programs::ProgramCacheEntry::new_builtin(
                0,
                "stake".len(),
                crate::compat::stake::Entrypoint::vm,
            ),
        );

        // The migration runs against the per-slot processor, as replay_range builds one per block.
        let per_slot = replayer.processor.new_from(slot, EPOCH);
        crate::compat::apply_core_bpf_migrations(
            &mut bank,
            &per_slot,
            &replayer.processor,
            &[agave_feature_set::migrate_stake_program_to_core_bpf::id()],
            EPOCH,
            slot,
        );

        assert!(
            !per_slot
                .builtin_program_ids
                .read()
                .unwrap()
                .contains(&stake),
            "gone for the migration slot itself"
        );
        let next_slot = replayer.processor.new_from(slot + 1, EPOCH);
        assert!(
            !next_slot
                .builtin_program_ids
                .read()
                .unwrap()
                .contains(&stake),
            "and for every slot after it"
        );
    }

    // The gate is the NEWLY activated set, not the feature set: the migration must fire in the
    // block the feature turns on, and never again in later epochs where it's merely active.
    #[test]
    fn the_gate_fires_only_on_the_activating_crossing() {
        let stake = solana_sdk_ids::stake::id();
        let migrate = agave_feature_set::migrate_stake_program_to_core_bpf::id();

        let (mut bank, replayer, slot) = bank_at_823(elf());
        crate::compat::apply_core_bpf_migrations(
            &mut bank,
            &replayer.processor,
            &replayer.processor,
            &[Pubkey::new_unique()],
            EPOCH,
            slot,
        );
        assert_eq!(
            bank.get_account_shared_data(&stake).unwrap().0.owner(),
            &solana_sdk_ids::native_loader::id(),
            "an unrelated activation must not migrate anything"
        );

        crate::compat::apply_core_bpf_migrations(
            &mut bank,
            &replayer.processor,
            &replayer.processor,
            &[migrate],
            EPOCH,
            slot,
        );
        assert_eq!(
            bank.get_account_shared_data(&stake).unwrap().0.owner(),
            &solana_sdk_ids::bpf_loader_upgradeable::id(),
            "the activating crossing must migrate"
        );
    }

    #[test]
    fn refuses_when_the_programdata_account_already_exists() {
        let (mut bank, replayer, slot) = bank_at_823(elf());
        let pda = get_program_data_address(&solana_sdk_ids::stake::id());
        bank.insert(pda, native_stake(42), slot);
        assert!(matches!(
            migrate_stake_to_core_bpf(
                &mut bank,
                &replayer.processor,
                &replayer.processor,
                EPOCH,
                slot
            ),
            Err(MigrationError::ProgramDataExists)
        ));
    }

    // The real epoch-823 buffer carries an authority. agave ignores it because the stake
    // config sets upgrade_authority_address: None, and writes None into the programdata.
    // Requiring the buffer's authority to be None halted a 12-hour run at the boundary.
    #[test]
    fn a_buffer_authority_is_ignored_when_the_config_has_none() {
        let slot = EPOCH * 432_000;
        let mut bank = ReplayBank::default();
        let replayer = Replayer::new(slot, EPOCH);
        bank.insert(solana_sdk_ids::stake::id(), native_stake(1), slot);
        bank.insert(
            STAKE_SOURCE_BUFFER,
            buffer(Some(Pubkey::new_unique()), elf(), 5_000_000),
            slot,
        );

        let out = migrate_stake_to_core_bpf(
            &mut bank,
            &replayer.processor,
            &replayer.processor,
            EPOCH,
            slot,
        )
        .expect("a buffer authority must not block the migration");

        let (data, _) = bank
            .get_account_shared_data(&out.program_data_address)
            .unwrap();
        let meta = UpgradeableLoaderState::size_of_programdata_metadata();
        assert_eq!(
            bincode::deserialize::<UpgradeableLoaderState>(&data.data()[..meta]).unwrap(),
            UpgradeableLoaderState::ProgramData {
                slot,
                upgrade_authority_address: None
            },
            "the config's None is what lands in the programdata"
        );
    }
}
