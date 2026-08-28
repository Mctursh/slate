//! Stake state
//! * delegate stakes to vote accounts
//! * keep track of rewards
//! * own mining pools

#[deprecated(
    since = "1.8.0",
    note = "Please use `solana_stake_interface::state` instead"
)]
pub use solana_stake_interface::state::*;
use {
    solana_account::{state_traits::StateMut, AccountSharedData, ReadableAccount},
    solana_clock::{Clock, Epoch},
    solana_instruction::error::InstructionError,
    solana_program_runtime::invoke_context::InvokeContext,
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_sdk_ids::stake::id,
    solana_stake_interface::{
        error::StakeError,
        instruction::LockupArgs,
        stake_flags::StakeFlags,
        stake_history::{StakeHistory, StakeHistoryEntry},
        tools::{acceptable_reference_epoch_credits, eligible_for_deactivate_delinquent},
    },
    solana_svm_log_collector::ic_msg,
    solana_transaction_context::{
        BorrowedInstructionAccount as BorrowedAccount, IndexOfAccount, InstructionContext,
    },
    solana_vote_interface::state::{VoteStateV3, VoteStateVersions},
    std::{collections::HashSet, convert::TryFrom},
};

// feature_set::reduce_stake_warmup_cooldown changed the warmup/cooldown from
// 25% to 9%. this number is necessary to calculate historical effective stake from
// stake history, but we only care that stake we are dealing with in the present
// epoch has been fully (de)activated. this means, as long as one epoch has
// passed since activation where all prior stake had escaped warmup/cooldown,
// we can pretend the rate has always beein 9% without issue. so we do that
const PERPETUAL_NEW_WARMUP_COOLDOWN_RATE_EPOCH: Option<u64> = Some(0);

// utility function, used by Stakes, tests
pub fn from<T: ReadableAccount + StateMut<StakeStateV2>>(account: &T) -> Option<StakeStateV2> {
    account.state().ok()
}

pub fn stake_from<T: ReadableAccount + StateMut<StakeStateV2>>(account: &T) -> Option<Stake> {
    from(account).and_then(|state: StakeStateV2| state.stake())
}

pub fn delegation_from(account: &AccountSharedData) -> Option<Delegation> {
    from(account).and_then(|state: StakeStateV2| state.delegation())
}

pub fn authorized_from(account: &AccountSharedData) -> Option<Authorized> {
    from(account).and_then(|state: StakeStateV2| state.authorized())
}

pub fn lockup_from<T: ReadableAccount + StateMut<StakeStateV2>>(account: &T) -> Option<Lockup> {
    from(account).and_then(|state: StakeStateV2| state.lockup())
}

pub fn meta_from(account: &AccountSharedData) -> Option<Meta> {
    from(account).and_then(|state: StakeStateV2| state.meta())
}

pub(crate) fn new_warmup_cooldown_rate_epoch() -> Option<Epoch> {
    PERPETUAL_NEW_WARMUP_COOLDOWN_RATE_EPOCH
}

fn checked_add(a: u64, b: u64) -> Result<u64, InstructionError> {
    a.checked_add(b).ok_or(InstructionError::InsufficientFunds)
}

fn get_stake_status(
    invoke_context: &InvokeContext,
    stake: &Stake,
    clock: &Clock,
) -> Result<StakeActivationStatus, InstructionError> {
    let stake_history = invoke_context.get_sysvar_cache().get_stake_history()?;
    Ok(stake.delegation.stake_activating_and_deactivating(
        clock.epoch,
        stake_history.as_ref(),
        new_warmup_cooldown_rate_epoch(),
    ))
}

fn redelegate_stake(
    stake: &mut Stake,
    stake_lamports: u64,
    voter_pubkey: &Pubkey,
    vote_state: &VoteStateV3,
    clock: &Clock,
    stake_history: &StakeHistory,
) -> Result<(), StakeError> {
    let new_rate_activation_epoch = new_warmup_cooldown_rate_epoch();
    // If stake is currently active:
    if stake.stake(clock.epoch, stake_history, new_rate_activation_epoch) != 0 {
        // If pubkey of new voter is the same as current,
        // and we are scheduled to start deactivating this epoch,
        // we rescind deactivation
        if stake.delegation.voter_pubkey == *voter_pubkey
            && clock.epoch == stake.delegation.deactivation_epoch
        {
            stake.delegation.deactivation_epoch = u64::MAX;
            return Ok(());
        } else {
            // can't redelegate to another pubkey if stake is active.
            return Err(StakeError::TooSoonToRedelegate);
        }
    }
    // Either the stake is freshly activated, is active but has been
    // deactivated this epoch, or has fully de-activated.
    // Redelegation implies either re-activation or un-deactivation

    stake.delegation.stake = stake_lamports;
    stake.delegation.activation_epoch = clock.epoch;
    stake.delegation.deactivation_epoch = u64::MAX;
    stake.delegation.voter_pubkey = *voter_pubkey;
    stake.credits_observed = vote_state.credits();
    Ok(())
}

fn move_stake_or_lamports_shared_checks(
    invoke_context: &InvokeContext,
    instruction_context: &InstructionContext,
    source_account: &BorrowedAccount,
    lamports: u64,
    destination_account: &BorrowedAccount,
    stake_authority_index: IndexOfAccount,
) -> Result<(MergeKind, MergeKind), InstructionError> {
    // authority must sign
    let stake_authority_pubkey =
        instruction_context.get_key_of_instruction_account(stake_authority_index)?;
    if !instruction_context.is_instruction_account_signer(stake_authority_index)? {
        return Err(InstructionError::MissingRequiredSignature);
    }

    let mut signers = HashSet::new();
    signers.insert(*stake_authority_pubkey);

    // check owners
    if *source_account.get_owner() != id() || *destination_account.get_owner() != id() {
        return Err(InstructionError::IncorrectProgramId);
    }

    // confirm not the same account
    if *source_account.get_key() == *destination_account.get_key() {
        return Err(InstructionError::InvalidInstructionData);
    }

    // source and destination must be writable
    if !source_account.is_writable() || !destination_account.is_writable() {
        return Err(InstructionError::InvalidInstructionData);
    }

    // must move something
    if lamports == 0 {
        return Err(InstructionError::InvalidArgument);
    }

    let clock = invoke_context.get_sysvar_cache().get_clock()?;
    let stake_history = invoke_context.get_sysvar_cache().get_stake_history()?;

    // get_if_mergeable ensures accounts are not partly activated or in any form of deactivating
    // we still need to exclude activating state ourselves
    let source_merge_kind = MergeKind::get_if_mergeable(
        invoke_context,
        &source_account.get_state()?,
        source_account.get_lamports(),
        &clock,
        &stake_history,
    )?;

    // Authorized staker is allowed to move stake
    source_merge_kind
        .meta()
        .authorized
        .check(&signers, StakeAuthorize::Staker)?;

    // same transient assurance as with source
    let destination_merge_kind = MergeKind::get_if_mergeable(
        invoke_context,
        &destination_account.get_state()?,
        destination_account.get_lamports(),
        &clock,
        &stake_history,
    )?;

    // ensure all authorities match and lockups match if lockup is in force
    MergeKind::metas_can_merge(
        invoke_context,
        source_merge_kind.meta(),
        destination_merge_kind.meta(),
        &clock,
    )?;

    Ok((source_merge_kind, destination_merge_kind))
}

pub(crate) fn new_stake(
    stake: u64,
    voter_pubkey: &Pubkey,
    vote_state: &VoteStateV3,
    activation_epoch: Epoch,
) -> Stake {
    Stake {
        delegation: Delegation::new(voter_pubkey, stake, activation_epoch),
        credits_observed: vote_state.credits(),
    }
}

pub fn initialize(
    stake_account: &mut BorrowedAccount,
    authorized: &Authorized,
    lockup: &Lockup,
    rent: &Rent,
) -> Result<(), InstructionError> {
    if stake_account.get_data().len() != StakeStateV2::size_of() {
        return Err(InstructionError::InvalidAccountData);
    }

    if let StakeStateV2::Uninitialized = stake_account.get_state()? {
        let rent_exempt_reserve = rent.minimum_balance(stake_account.get_data().len());
        if stake_account.get_lamports() >= rent_exempt_reserve {
            stake_account.set_state(&StakeStateV2::Initialized(Meta {
                rent_exempt_reserve,
                authorized: *authorized,
                lockup: *lockup,
            }))
        } else {
            Err(InstructionError::InsufficientFunds)
        }
    } else {
        Err(InstructionError::InvalidAccountData)
    }
}

/// Authorize the given pubkey to manage stake (deactivate, withdraw). This may be called
/// multiple times, but will implicitly withdraw authorization from the previously authorized
/// staker. The default staker is the owner of the stake account's pubkey.
pub fn authorize(
    stake_account: &mut BorrowedAccount,
    signers: &HashSet<Pubkey>,
    new_authority: &Pubkey,
    stake_authorize: StakeAuthorize,
    clock: &Clock,
    custodian: Option<&Pubkey>,
) -> Result<(), InstructionError> {
    match stake_account.get_state()? {
        StakeStateV2::Stake(mut meta, stake, stake_flags) => {
            meta.authorized.authorize(
                signers,
                new_authority,
                stake_authorize,
                Some((&meta.lockup, clock, custodian)),
            )?;
            stake_account.set_state(&StakeStateV2::Stake(meta, stake, stake_flags))
        }
        StakeStateV2::Initialized(mut meta) => {
            meta.authorized.authorize(
                signers,
                new_authority,
                stake_authorize,
                Some((&meta.lockup, clock, custodian)),
            )?;
            stake_account.set_state(&StakeStateV2::Initialized(meta))
        }
        _ => Err(InstructionError::InvalidAccountData),
    }
}

#[allow(clippy::too_many_arguments)]
pub fn authorize_with_seed(
    instruction_context: &InstructionContext,
    stake_account: &mut BorrowedAccount,
    authority_base_index: IndexOfAccount,
    authority_seed: &str,
    authority_owner: &Pubkey,
    new_authority: &Pubkey,
    stake_authorize: StakeAuthorize,
    clock: &Clock,
    custodian: Option<&Pubkey>,
) -> Result<(), InstructionError> {
    let mut signers = HashSet::default();
    if instruction_context.is_instruction_account_signer(authority_base_index)? {
        let base_pubkey =
            instruction_context.get_key_of_instruction_account(authority_base_index)?;
        // The conversion from `PubkeyError` to `InstructionError` through
        // num-traits is incorrect, but it's the existing behavior.
        signers.insert(
            Pubkey::create_with_seed(base_pubkey, authority_seed, authority_owner)
                .map_err(|e| e as u64)?,
        );
    }
    authorize(
        stake_account,
        &signers,
        new_authority,
        stake_authorize,
        clock,
        custodian,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn delegate(
    instruction_context: &InstructionContext,
    stake_account_index: IndexOfAccount,
    vote_account_index: IndexOfAccount,
    clock: &Clock,
    stake_history: &StakeHistory,
    signers: &HashSet<Pubkey>,
    invoke_context: &InvokeContext,
) -> Result<(), InstructionError> {
    let vote_account = instruction_context.try_borrow_instruction_account(vote_account_index)?;
    if *vote_account.get_owner() != solana_sdk_ids::vote::id() {
        return Err(InstructionError::IncorrectProgramId);
    }
    let vote_pubkey = *vote_account.get_key();
    let vote_state = vote_account.get_state::<VoteStateVersions>();
    drop(vote_account);

    let mut stake_account =
        instruction_context.try_borrow_instruction_account(stake_account_index)?;
    match stake_account.get_state()? {
        StakeStateV2::Initialized(meta) => {
            meta.authorized.check(signers, StakeAuthorize::Staker)?;
            let ValidatedDelegatedInfo { stake_amount } =
                validate_delegated_amount(&stake_account, &meta, invoke_context)?;
            let stake = new_stake(
                stake_amount,
                &vote_pubkey,
                &vote_state?.convert_to_v3(),
                clock.epoch,
            );
            stake_account.set_state(&StakeStateV2::Stake(meta, stake, StakeFlags::empty()))
        }
        StakeStateV2::Stake(meta, mut stake, stake_flags) => {
            meta.authorized.check(signers, StakeAuthorize::Staker)?;
            let ValidatedDelegatedInfo { stake_amount } =
                validate_delegated_amount(&stake_account, &meta, invoke_context)?;
            redelegate_stake(
                &mut stake,
                stake_amount,
                &vote_pubkey,
                &vote_state?.convert_to_v3(),
                clock,
                stake_history,
            )?;
            stake_account.set_state(&StakeStateV2::Stake(meta, stake, stake_flags))
        }
        _ => Err(InstructionError::InvalidAccountData),
    }
}

pub fn deactivate(
    stake_account: &mut BorrowedAccount,
    clock: &Clock,
    signers: &HashSet<Pubkey>,
) -> Result<(), InstructionError> {
    if let StakeStateV2::Stake(meta, mut stake, stake_flags) = stake_account.get_state()? {
        meta.authorized.check(signers, StakeAuthorize::Staker)?;
        stake.deactivate(clock.epoch)?;
        stake_account.set_state(&StakeStateV2::Stake(meta, stake, stake_flags))
    } else {
        Err(InstructionError::InvalidAccountData)
    }
}

pub fn set_lockup(
    stake_account: &mut BorrowedAccount,
    lockup: &LockupArgs,
    signers: &HashSet<Pubkey>,
    clock: &Clock,
) -> Result<(), InstructionError> {
    match stake_account.get_state()? {
        StakeStateV2::Initialized(mut meta) => {
            meta.set_lockup(lockup, signers, clock)?;
            stake_account.set_state(&StakeStateV2::Initialized(meta))
        }
        StakeStateV2::Stake(mut meta, stake, stake_flags) => {
            meta.set_lockup(lockup, signers, clock)?;
            stake_account.set_state(&StakeStateV2::Stake(meta, stake, stake_flags))
        }
        _ => Err(InstructionError::InvalidAccountData),
    }
}

pub fn split(
    invoke_context: &InvokeContext,
    instruction_context: &InstructionContext,
    stake_account_index: IndexOfAccount,
    lamports: u64,
    split_index: IndexOfAccount,
    signers: &HashSet<Pubkey>,
) -> Result<(), InstructionError> {
    let split = instruction_context.try_borrow_instruction_account(split_index)?;
    if *split.get_owner() != id() {
        return Err(InstructionError::IncorrectProgramId);
    }
    if split.get_data().len() != StakeStateV2::size_of() {
        return Err(InstructionError::InvalidAccountData);
    }
    if !matches!(split.get_state()?, StakeStateV2::Uninitialized) {
        return Err(InstructionError::InvalidAccountData);
    }
    let split_lamport_balance = split.get_lamports();
    drop(split);
    let stake_account = instruction_context.try_borrow_instruction_account(stake_account_index)?;
    if lamports > stake_account.get_lamports() {
        return Err(InstructionError::InsufficientFunds);
    }
    let stake_state = stake_account.get_state()?;
    drop(stake_account);

    match stake_state {
        StakeStateV2::Stake(meta, mut stake, stake_flags) => {
            meta.authorized.check(signers, StakeAuthorize::Staker)?;
            let minimum_delegation = crate::get_minimum_delegation(
                invoke_context.is_stake_raise_minimum_delegation_to_1_sol_active(),
            );
            let is_active = {
                let clock = invoke_context.get_sysvar_cache().get_clock()?;
                let status = get_stake_status(invoke_context, &stake, &clock)?;
                status.effective > 0
            };
            let validated_split_info = validate_split_amount(
                invoke_context,
                instruction_context,
                stake_account_index,
                split_index,
                lamports,
                &meta,
                minimum_delegation,
                is_active,
            )?;

            // split the stake, subtract rent_exempt_balance unless
            // the destination account already has those lamports
            // in place.
            // this means that the new stake account will have a stake equivalent to
            // lamports minus rent_exempt_reserve if it starts out with a zero balance
            let (remaining_stake_delta, split_stake_amount) =
                if validated_split_info.source_remaining_balance == 0 {
                    // If split amount equals the full source stake (as implied by 0
                    // source_remaining_balance), the new split stake must equal the same
                    // amount, regardless of any current lamport balance in the split account.
                    // Since split accounts retain the state of their source account, this
                    // prevents any magic activation of stake by prefunding the split account.
                    //
                    // The new split stake also needs to ignore any positive delta between the
                    // original rent_exempt_reserve and the split_rent_exempt_reserve, in order
                    // to prevent magic activation of stake by splitting between accounts of
                    // different sizes.
                    let remaining_stake_delta = lamports.saturating_sub(meta.rent_exempt_reserve);
                    (remaining_stake_delta, remaining_stake_delta)
                } else {
                    // Otherwise, the new split stake should reflect the entire split
                    // requested, less any lamports needed to cover the split_rent_exempt_reserve.

                    if stake.delegation.stake.saturating_sub(lamports) < minimum_delegation {
                        return Err(StakeError::InsufficientDelegation.into());
                    }

                    (
                        lamports,
                        lamports.saturating_sub(
                            validated_split_info
                                .destination_rent_exempt_reserve
                                .saturating_sub(split_lamport_balance),
                        ),
                    )
                };

            if split_stake_amount < minimum_delegation {
                return Err(StakeError::InsufficientDelegation.into());
            }

            let split_stake = stake.split(remaining_stake_delta, split_stake_amount)?;
            let mut split_meta = meta;
            split_meta.rent_exempt_reserve = validated_split_info.destination_rent_exempt_reserve;

            let mut stake_account =
                instruction_context.try_borrow_instruction_account(stake_account_index)?;
            stake_account.set_state(&StakeStateV2::Stake(meta, stake, stake_flags))?;
            drop(stake_account);
            let mut split = instruction_context.try_borrow_instruction_account(split_index)?;
            split.set_state(&StakeStateV2::Stake(split_meta, split_stake, stake_flags))?;
        }
        StakeStateV2::Initialized(meta) => {
            meta.authorized.check(signers, StakeAuthorize::Staker)?;
            let validated_split_info = validate_split_amount(
                invoke_context,
                instruction_context,
                stake_account_index,
                split_index,
                lamports,
                &meta,
                0, // additional_required_lamports
                false,
            )?;
            let mut split_meta = meta;
            split_meta.rent_exempt_reserve = validated_split_info.destination_rent_exempt_reserve;
            let mut split = instruction_context.try_borrow_instruction_account(split_index)?;
            split.set_state(&StakeStateV2::Initialized(split_meta))?;
        }
        StakeStateV2::Uninitialized => {
            let stake_pubkey =
                instruction_context.get_key_of_instruction_account(stake_account_index)?;
            if !signers.contains(stake_pubkey) {
                return Err(InstructionError::MissingRequiredSignature);
            }
        }
        _ => return Err(InstructionError::InvalidAccountData),
    }

    // Deinitialize state upon zero balance
    let mut stake_account =
        instruction_context.try_borrow_instruction_account(stake_account_index)?;
    if lamports == stake_account.get_lamports() {
        stake_account.set_state(&StakeStateV2::Uninitialized)?;
    }
    drop(stake_account);

    let mut split = instruction_context.try_borrow_instruction_account(split_index)?;
    split.checked_add_lamports(lamports)?;
    drop(split);
    let mut stake_account =
        instruction_context.try_borrow_instruction_account(stake_account_index)?;
    stake_account.checked_sub_lamports(lamports)?;
    Ok(())
}

pub fn merge(
    invoke_context: &InvokeContext,
    instruction_context: &InstructionContext,
    stake_account_index: IndexOfAccount,
    source_account_index: IndexOfAccount,
    clock: &Clock,
    stake_history: &StakeHistory,
    signers: &HashSet<Pubkey>,
) -> Result<(), InstructionError> {
    let mut source_account =
        instruction_context.try_borrow_instruction_account(source_account_index)?;
    // Ensure source isn't spoofed
    if *source_account.get_owner() != id() {
        return Err(InstructionError::IncorrectProgramId);
    }
    // Close the stake_account-reference loophole
    if instruction_context.get_index_of_instruction_account_in_transaction(stake_account_index)?
        == instruction_context
            .get_index_of_instruction_account_in_transaction(source_account_index)?
    {
        return Err(InstructionError::InvalidArgument);
    }
    let mut stake_account =
        instruction_context.try_borrow_instruction_account(stake_account_index)?;

    ic_msg!(invoke_context, "Checking if destination stake is mergeable");
    let stake_merge_kind = MergeKind::get_if_mergeable(
        invoke_context,
        &stake_account.get_state()?,
        stake_account.get_lamports(),
        clock,
        stake_history,
    )?;

    // Authorized staker is allowed to split/merge accounts
    stake_merge_kind
        .meta()
        .authorized
        .check(signers, StakeAuthorize::Staker)?;

    ic_msg!(invoke_context, "Checking if source stake is mergeable");
    let source_merge_kind = MergeKind::get_if_mergeable(
        invoke_context,
        &source_account.get_state()?,
        source_account.get_lamports(),
        clock,
        stake_history,
    )?;

    ic_msg!(invoke_context, "Merging stake accounts");
    if let Some(merged_state) = stake_merge_kind.merge(invoke_context, source_merge_kind, clock)? {
        stake_account.set_state(&merged_state)?;
    }

    // Source is about to be drained, deinitialize its state
    source_account.set_state(&StakeStateV2::Uninitialized)?;

    // Drain the source stake account
    let lamports = source_account.get_lamports();
    source_account.checked_sub_lamports(lamports)?;
    stake_account.checked_add_lamports(lamports)?;
    Ok(())
}

pub fn move_stake(
    invoke_context: &InvokeContext,
    instruction_context: &InstructionContext,
    source_account_index: IndexOfAccount,
    lamports: u64,
    destination_account_index: IndexOfAccount,
    stake_authority_index: IndexOfAccount,
) -> Result<(), InstructionError> {
    let mut source_account =
        instruction_context.try_borrow_instruction_account(source_account_index)?;

    let mut destination_account =
        instruction_context.try_borrow_instruction_account(destination_account_index)?;

    let (source_merge_kind, destination_merge_kind) = move_stake_or_lamports_shared_checks(
        invoke_context,
        instruction_context,
        &source_account,
        lamports,
        &destination_account,
        stake_authority_index,
    )?;

    // ensure source and destination are the right size for the current version of StakeState
    // this a safeguard in case there is a new version of the struct that cannot fit into an old account
    if source_account.get_data().len() != StakeStateV2::size_of()
        || destination_account.get_data().len() != StakeStateV2::size_of()
    {
        return Err(InstructionError::InvalidAccountData);
    }

    // source must be fully active
    let MergeKind::FullyActive(source_meta, mut source_stake) = source_merge_kind else {
        return Err(InstructionError::InvalidAccountData);
    };

    let minimum_delegation = crate::get_minimum_delegation(
        invoke_context.is_stake_raise_minimum_delegation_to_1_sol_active(),
    );
    let source_effective_stake = source_stake.delegation.stake;

    // source cannot move more stake than it has, regardless of how many lamports it has
    let source_final_stake = source_effective_stake
        .checked_sub(lamports)
        .ok_or(InstructionError::InvalidArgument)?;

    // unless all stake is being moved, source must retain at least the minimum delegation
    if source_final_stake != 0 && source_final_stake < minimum_delegation {
        return Err(InstructionError::InvalidArgument);
    }

    // destination must be fully active or fully inactive
    let destination_meta = match destination_merge_kind {
        MergeKind::FullyActive(destination_meta, mut destination_stake) => {
            // if active, destination must be delegated to the same vote account as source
            if source_stake.delegation.voter_pubkey != destination_stake.delegation.voter_pubkey {
                return Err(StakeError::VoteAddressMismatch.into());
            }

            let destination_effective_stake = destination_stake.delegation.stake;
            let destination_final_stake = destination_effective_stake
                .checked_add(lamports)
                .ok_or(InstructionError::ArithmeticOverflow)?;

            // ensure destination meets miniumum delegation
            // since it is already active, this only really applies if the minimum is raised
            if destination_final_stake < minimum_delegation {
                return Err(InstructionError::InvalidArgument);
            }

            merge_delegation_stake_and_credits_observed(
                &mut destination_stake,
                lamports,
                source_stake.credits_observed,
            )?;

            destination_account.set_state(&StakeStateV2::Stake(
                destination_meta,
                destination_stake,
                StakeFlags::empty(),
            ))?;

            destination_meta
        }
        MergeKind::Inactive(destination_meta, _, _) => {
            // if destination is inactive, it must be given at least the minimum delegation
            if lamports < minimum_delegation {
                return Err(InstructionError::InvalidArgument);
            }

            let mut destination_stake = source_stake;
            destination_stake.delegation.stake = lamports;

            destination_account.set_state(&StakeStateV2::Stake(
                destination_meta,
                destination_stake,
                StakeFlags::empty(),
            ))?;

            destination_meta
        }
        _ => return Err(InstructionError::InvalidAccountData),
    };

    if source_final_stake == 0 {
        source_account.set_state(&StakeStateV2::Initialized(source_meta))?;
    } else {
        source_stake.delegation.stake = source_final_stake;

        source_account.set_state(&StakeStateV2::Stake(
            source_meta,
            source_stake,
            StakeFlags::empty(),
        ))?;
    }

    source_account.checked_sub_lamports(lamports)?;
    destination_account.checked_add_lamports(lamports)?;

    // this should be impossible, but because we do all our math with delegations, best to guard it
    if source_account.get_lamports() < source_meta.rent_exempt_reserve
        || destination_account.get_lamports() < destination_meta.rent_exempt_reserve
    {
        ic_msg!(
            invoke_context,
            "Delegation calculations violated lamport balance assumptions"
        );
        return Err(InstructionError::InvalidArgument);
    }

    Ok(())
}

pub fn move_lamports(
    invoke_context: &InvokeContext,
    instruction_context: &InstructionContext,
    source_account_index: IndexOfAccount,
    lamports: u64,
    destination_account_index: IndexOfAccount,
    stake_authority_index: IndexOfAccount,
) -> Result<(), InstructionError> {
    let mut source_account =
        instruction_context.try_borrow_instruction_account(source_account_index)?;

    let mut destination_account =
        instruction_context.try_borrow_instruction_account(destination_account_index)?;

    let (source_merge_kind, _) = move_stake_or_lamports_shared_checks(
        invoke_context,
        instruction_context,
        &source_account,
        lamports,
        &destination_account,
        stake_authority_index,
    )?;

    let source_free_lamports = match source_merge_kind {
        MergeKind::FullyActive(source_meta, source_stake) => source_account
            .get_lamports()
            .saturating_sub(source_stake.delegation.stake)
            .saturating_sub(source_meta.rent_exempt_reserve),
        MergeKind::Inactive(source_meta, source_lamports, _) => {
            source_lamports.saturating_sub(source_meta.rent_exempt_reserve)
        }
        _ => return Err(InstructionError::InvalidAccountData),
    };

    if lamports > source_free_lamports {
        return Err(InstructionError::InvalidArgument);
    }

    source_account.checked_sub_lamports(lamports)?;
    destination_account.checked_add_lamports(lamports)?;

    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn withdraw(
    instruction_context: &InstructionContext,
    stake_account_index: IndexOfAccount,
    lamports: u64,
    to_index: IndexOfAccount,
    clock: &Clock,
    stake_history: &StakeHistory,
    withdraw_authority_index: IndexOfAccount,
    custodian_index: Option<IndexOfAccount>,
    new_rate_activation_epoch: Option<Epoch>,
) -> Result<(), InstructionError> {
    let withdraw_authority_pubkey =
        instruction_context.get_key_of_instruction_account(withdraw_authority_index)?;
    if !instruction_context.is_instruction_account_signer(withdraw_authority_index)? {
        return Err(InstructionError::MissingRequiredSignature);
    }
    let mut signers = HashSet::new();
    signers.insert(*withdraw_authority_pubkey);

    let mut stake_account =
        instruction_context.try_borrow_instruction_account(stake_account_index)?;
    let (lockup, reserve, is_staked) = match stake_account.get_state()? {
        StakeStateV2::Stake(meta, stake, _stake_flag) => {
            meta.authorized
                .check(&signers, StakeAuthorize::Withdrawer)?;
            // if we have a deactivation epoch and we're in cooldown
            let staked = if clock.epoch >= stake.delegation.deactivation_epoch {
                stake
                    .delegation
                    .stake(clock.epoch, stake_history, new_rate_activation_epoch)
            } else {
                // Assume full stake if the stake account hasn't been
                //  de-activated, because in the future the exposed stake
                //  might be higher than stake.stake() due to warmup
                stake.delegation.stake
            };

            let staked_and_reserve = checked_add(staked, meta.rent_exempt_reserve)?;
            (meta.lockup, staked_and_reserve, staked != 0)
        }
        StakeStateV2::Initialized(meta) => {
            meta.authorized
                .check(&signers, StakeAuthorize::Withdrawer)?;
            // stake accounts must have a balance >= rent_exempt_reserve
            (meta.lockup, meta.rent_exempt_reserve, false)
        }
        StakeStateV2::Uninitialized => {
            if !signers.contains(stake_account.get_key()) {
                return Err(InstructionError::MissingRequiredSignature);
            }
            (Lockup::default(), 0, false) // no lockup, no restrictions
        }
        _ => return Err(InstructionError::InvalidAccountData),
    };

    // verify that lockup has expired or that the withdrawal is signed by
    //   the custodian, both epoch and unix_timestamp must have passed
    let custodian_pubkey = if let Some(custodian_index) = custodian_index {
        if instruction_context.is_instruction_account_signer(custodian_index)? {
            Some(instruction_context.get_key_of_instruction_account(custodian_index)?)
        } else {
            None
        }
    } else {
        None
    };
    if lockup.is_in_force(clock, custodian_pubkey) {
        return Err(StakeError::LockupInForce.into());
    }

    if lamports == stake_account.get_lamports() {
        // if the stake is active, we mustn't allow the account to go away
        if is_staked {
            return Err(InstructionError::InsufficientFunds);
        }

        // Deinitialize state upon zero balance
        stake_account.set_state(&StakeStateV2::Uninitialized)?;
    } else {
        // Don't allow withdrawing the reserved rent balance or active stake
        let lamports_and_reserve = checked_add(lamports, reserve)?;
        if lamports_and_reserve > stake_account.get_lamports() {
            return Err(InstructionError::InsufficientFunds);
        }
    }

    stake_account.checked_sub_lamports(lamports)?;
    drop(stake_account);
    let mut to = instruction_context.try_borrow_instruction_account(to_index)?;
    to.checked_add_lamports(lamports)?;
    Ok(())
}

pub(crate) fn deactivate_delinquent(
    instruction_context: &InstructionContext,
    stake_account: &mut BorrowedAccount,
    delinquent_vote_account_index: IndexOfAccount,
    reference_vote_account_index: IndexOfAccount,
    current_epoch: Epoch,
) -> Result<(), InstructionError> {
    let delinquent_vote_account_pubkey =
        instruction_context.get_key_of_instruction_account(delinquent_vote_account_index)?;
    let delinquent_vote_account =
        instruction_context.try_borrow_instruction_account(delinquent_vote_account_index)?;
    if *delinquent_vote_account.get_owner() != solana_sdk_ids::vote::id() {
        return Err(InstructionError::IncorrectProgramId);
    }
    let delinquent_vote_state = delinquent_vote_account
        .get_state::<VoteStateVersions>()?
        .convert_to_v3();

    let reference_vote_account =
        instruction_context.try_borrow_instruction_account(reference_vote_account_index)?;
    if *reference_vote_account.get_owner() != solana_sdk_ids::vote::id() {
        return Err(InstructionError::IncorrectProgramId);
    }
    let reference_vote_state = reference_vote_account
        .get_state::<VoteStateVersions>()?
        .convert_to_v3();

    if !acceptable_reference_epoch_credits(&reference_vote_state.epoch_credits, current_epoch) {
        return Err(StakeError::InsufficientReferenceVotes.into());
    }

    if let StakeStateV2::Stake(meta, mut stake, stake_flags) = stake_account.get_state()? {
        if stake.delegation.voter_pubkey != *delinquent_vote_account_pubkey {
            return Err(StakeError::VoteAddressMismatch.into());
        }

        // Deactivate the stake account if its delegated vote account has never voted or has not
        // voted in the last `MINIMUM_DELINQUENT_EPOCHS_FOR_DEACTIVATION`
        if eligible_for_deactivate_delinquent(&delinquent_vote_state.epoch_credits, current_epoch) {
            stake.deactivate(current_epoch)?;
            stake_account.set_state(&StakeStateV2::Stake(meta, stake, stake_flags))
        } else {
            Err(StakeError::MinimumDelinquentEpochsForDeactivationNotMet.into())
        }
    } else {
        Err(InstructionError::InvalidAccountData)
    }
}

/// After calling `validate_delegated_amount()`, this struct contains calculated values that are used
/// by the caller.
struct ValidatedDelegatedInfo {
    stake_amount: u64,
}

/// Ensure the stake delegation amount is valid.  This checks that the account meets the minimum
/// balance requirements of delegated stake.  If not, return an error.
fn validate_delegated_amount(
    account: &BorrowedAccount,
    meta: &Meta,
    invoke_context: &InvokeContext,
) -> Result<ValidatedDelegatedInfo, InstructionError> {
    let stake_amount = account
        .get_lamports()
        .saturating_sub(meta.rent_exempt_reserve); // can't stake the rent

    // Stake accounts may be initialized with a stake amount below the minimum delegation so check
    // that the minimum is met before delegation.
    if stake_amount
        < crate::get_minimum_delegation(
            invoke_context.is_stake_raise_minimum_delegation_to_1_sol_active(),
        )
    {
        return Err(StakeError::InsufficientDelegation.into());
    }
    Ok(ValidatedDelegatedInfo { stake_amount })
}

/// After calling `validate_split_amount()`, this struct contains calculated values that are used
/// by the caller.
#[derive(Copy, Clone, Debug, Default)]
struct ValidatedSplitInfo {
    source_remaining_balance: u64,
    destination_rent_exempt_reserve: u64,
}

/// Ensure the split amount is valid.  This checks the source and destination accounts meet the
/// minimum balance requirements, which is the rent exempt reserve plus the minimum stake
/// delegation, and that the source account has enough lamports for the request split amount.  If
/// not, return an error.
fn validate_split_amount(
    invoke_context: &InvokeContext,
    instruction_context: &InstructionContext,
    source_account_index: IndexOfAccount,
    destination_account_index: IndexOfAccount,
    lamports: u64,
    source_meta: &Meta,
    additional_required_lamports: u64,
    source_is_active: bool,
) -> Result<ValidatedSplitInfo, InstructionError> {
    let source_account =
        instruction_context.try_borrow_instruction_account(source_account_index)?;
    let source_lamports = source_account.get_lamports();
    drop(source_account);
    let destination_account =
        instruction_context.try_borrow_instruction_account(destination_account_index)?;
    let destination_lamports = destination_account.get_lamports();
    let destination_data_len = destination_account.get_data().len();
    drop(destination_account);

    // Split amount has to be something
    if lamports == 0 {
        return Err(InstructionError::InsufficientFunds);
    }

    // Obviously cannot split more than what the source account has
    if lamports > source_lamports {
        return Err(InstructionError::InsufficientFunds);
    }

    // Verify that the source account still has enough lamports left after splitting:
    // EITHER at least the minimum balance, OR zero (in this case the source
    // account is transferring all lamports to new destination account, and the source
    // account will be closed)
    let source_minimum_balance = source_meta
        .rent_exempt_reserve
        .saturating_add(additional_required_lamports);
    let source_remaining_balance = source_lamports.saturating_sub(lamports);
    if source_remaining_balance == 0 {
        // full amount is a withdrawal
        // nothing to do here
    } else if source_remaining_balance < source_minimum_balance {
        // the remaining balance is too low to do the split
        return Err(InstructionError::InsufficientFunds);
    } else {
        // all clear!
        // nothing to do here
    }

    let rent = invoke_context.get_sysvar_cache().get_rent()?;
    let destination_rent_exempt_reserve = rent.minimum_balance(destination_data_len);

    // If the source is active stake, one of these criteria must be met:
    // 1. the destination account must be prefunded with at least the rent-exempt reserve, or
    // 2. the split must consume 100% of the source
    if source_is_active
        && source_remaining_balance != 0
        && destination_lamports < destination_rent_exempt_reserve
    {
        return Err(InstructionError::InsufficientFunds);
    }

    // Verify the destination account meets the minimum balance requirements
    // This must handle:
    // 1. The destination account having a different rent exempt reserve due to data size changes
    // 2. The destination account being prefunded, which would lower the minimum split amount
    let destination_minimum_balance =
        destination_rent_exempt_reserve.saturating_add(additional_required_lamports);
    let destination_balance_deficit =
        destination_minimum_balance.saturating_sub(destination_lamports);
    if lamports < destination_balance_deficit {
        return Err(InstructionError::InsufficientFunds);
    }

    Ok(ValidatedSplitInfo {
        source_remaining_balance,
        destination_rent_exempt_reserve,
    })
}

#[derive(Clone, Debug, PartialEq)]
enum MergeKind {
    Inactive(Meta, u64, StakeFlags),
    ActivationEpoch(Meta, Stake, StakeFlags),
    FullyActive(Meta, Stake),
}

impl MergeKind {
    fn meta(&self) -> &Meta {
        match self {
            Self::Inactive(meta, _, _) => meta,
            Self::ActivationEpoch(meta, _, _) => meta,
            Self::FullyActive(meta, _) => meta,
        }
    }

    fn active_stake(&self) -> Option<&Stake> {
        match self {
            Self::Inactive(_, _, _) => None,
            Self::ActivationEpoch(_, stake, _) => Some(stake),
            Self::FullyActive(_, stake) => Some(stake),
        }
    }

    fn get_if_mergeable(
        invoke_context: &InvokeContext,
        stake_state: &StakeStateV2,
        stake_lamports: u64,
        clock: &Clock,
        stake_history: &StakeHistory,
    ) -> Result<Self, InstructionError> {
        match stake_state {
            StakeStateV2::Stake(meta, stake, stake_flags) => {
                // stake must not be in a transient state. Transient here meaning
                // activating or deactivating with non-zero effective stake.
                let status = stake.delegation.stake_activating_and_deactivating(
                    clock.epoch,
                    stake_history,
                    new_warmup_cooldown_rate_epoch(),
                );

                match (status.effective, status.activating, status.deactivating) {
                    (0, 0, 0) => Ok(Self::Inactive(*meta, stake_lamports, *stake_flags)),
                    (0, _, _) => Ok(Self::ActivationEpoch(*meta, *stake, *stake_flags)),
                    (_, 0, 0) => Ok(Self::FullyActive(*meta, *stake)),
                    _ => {
                        let err = StakeError::MergeTransientStake;
                        ic_msg!(invoke_context, "{}", err);
                        Err(err.into())
                    }
                }
            }
            StakeStateV2::Initialized(meta) => {
                Ok(Self::Inactive(*meta, stake_lamports, StakeFlags::empty()))
            }
            _ => Err(InstructionError::InvalidAccountData),
        }
    }

    fn metas_can_merge(
        invoke_context: &InvokeContext,
        stake: &Meta,
        source: &Meta,
        clock: &Clock,
    ) -> Result<(), InstructionError> {
        // lockups may mismatch so long as both have expired
        let can_merge_lockups = stake.lockup == source.lockup
            || (!stake.lockup.is_in_force(clock, None) && !source.lockup.is_in_force(clock, None));
        // `rent_exempt_reserve` has no bearing on the mergeability of accounts,
        // as the source account will be culled by runtime once the operation
        // succeeds. Considering it here would needlessly prevent merging stake
        // accounts with differing data lengths, which already exist in the wild
        // due to an SDK bug
        if stake.authorized == source.authorized && can_merge_lockups {
            Ok(())
        } else {
            ic_msg!(invoke_context, "Unable to merge due to metadata mismatch");
            Err(StakeError::MergeMismatch.into())
        }
    }

    fn active_delegations_can_merge(
        invoke_context: &InvokeContext,
        stake: &Delegation,
        source: &Delegation,
    ) -> Result<(), InstructionError> {
        if stake.voter_pubkey != source.voter_pubkey {
            ic_msg!(invoke_context, "Unable to merge due to voter mismatch");
            Err(StakeError::MergeMismatch.into())
        } else if stake.deactivation_epoch == Epoch::MAX && source.deactivation_epoch == Epoch::MAX
        {
            Ok(())
        } else {
            ic_msg!(invoke_context, "Unable to merge due to stake deactivation");
            Err(StakeError::MergeMismatch.into())
        }
    }

    fn merge(
        self,
        invoke_context: &InvokeContext,
        source: Self,
        clock: &Clock,
    ) -> Result<Option<StakeStateV2>, InstructionError> {
        Self::metas_can_merge(invoke_context, self.meta(), source.meta(), clock)?;
        self.active_stake()
            .zip(source.active_stake())
            .map(|(stake, source)| {
                Self::active_delegations_can_merge(
                    invoke_context,
                    &stake.delegation,
                    &source.delegation,
                )
            })
            .unwrap_or(Ok(()))?;
        let merged_state = match (self, source) {
            (Self::Inactive(_, _, _), Self::Inactive(_, _, _)) => None,
            (Self::Inactive(_, _, _), Self::ActivationEpoch(_, _, _)) => None,
            (
                Self::ActivationEpoch(meta, mut stake, stake_flags),
                Self::Inactive(_, source_lamports, source_stake_flags),
            ) => {
                stake.delegation.stake = checked_add(stake.delegation.stake, source_lamports)?;
                Some(StakeStateV2::Stake(
                    meta,
                    stake,
                    stake_flags.union(source_stake_flags),
                ))
            }
            (
                Self::ActivationEpoch(meta, mut stake, stake_flags),
                Self::ActivationEpoch(source_meta, source_stake, source_stake_flags),
            ) => {
                let source_lamports = checked_add(
                    source_meta.rent_exempt_reserve,
                    source_stake.delegation.stake,
                )?;
                merge_delegation_stake_and_credits_observed(
                    &mut stake,
                    source_lamports,
                    source_stake.credits_observed,
                )?;
                Some(StakeStateV2::Stake(
                    meta,
                    stake,
                    stake_flags.union(source_stake_flags),
                ))
            }
            (Self::FullyActive(meta, mut stake), Self::FullyActive(_, source_stake)) => {
                // Don't stake the source account's `rent_exempt_reserve` to
                // protect against the magic activation loophole. It will
                // instead be moved into the destination account as extra,
                // withdrawable `lamports`
                merge_delegation_stake_and_credits_observed(
                    &mut stake,
                    source_stake.delegation.stake,
                    source_stake.credits_observed,
                )?;
                Some(StakeStateV2::Stake(meta, stake, StakeFlags::empty()))
            }
            _ => return Err(StakeError::MergeMismatch.into()),
        };
        Ok(merged_state)
    }
}

fn merge_delegation_stake_and_credits_observed(
    stake: &mut Stake,
    absorbed_lamports: u64,
    absorbed_credits_observed: u64,
) -> Result<(), InstructionError> {
    stake.credits_observed =
        stake_weighted_credits_observed(stake, absorbed_lamports, absorbed_credits_observed)
            .ok_or(InstructionError::ArithmeticOverflow)?;
    stake.delegation.stake = checked_add(stake.delegation.stake, absorbed_lamports)?;
    Ok(())
}

/// Calculate the effective credits observed for two stakes when merging
///
/// When merging two `ActivationEpoch` or `FullyActive` stakes, the credits
/// observed of the merged stake is the weighted average of the two stakes'
/// credits observed.
///
/// This is because we can derive the effective credits_observed by reversing the staking
/// rewards equation, _while keeping the rewards unchanged after merge (i.e. strong
/// requirement)_, like below:
///
/// a(N) => account, r => rewards, s => stake, c => credits:
/// assume:
///   a3 = merge(a1, a2)
/// then:
///   a3.s = a1.s + a2.s
///
/// Next, given:
///   aN.r = aN.c * aN.s (for every N)
/// finally:
///        a3.r = a1.r + a2.r
/// a3.c * a3.s = a1.c * a1.s + a2.c * a2.s
///        a3.c = (a1.c * a1.s + a2.c * a2.s) / (a1.s + a2.s)     // QED
///
/// (For this discussion, we omitted irrelevant variables, including distance
///  calculation against vote_account and point indirection.)
fn stake_weighted_credits_observed(
    stake: &Stake,
    absorbed_lamports: u64,
    absorbed_credits_observed: u64,
) -> Option<u64> {
    if stake.credits_observed == absorbed_credits_observed {
        Some(stake.credits_observed)
    } else {
        let total_stake = u128::from(stake.delegation.stake.checked_add(absorbed_lamports)?);
        let stake_weighted_credits =
            u128::from(stake.credits_observed).checked_mul(u128::from(stake.delegation.stake))?;
        let absorbed_weighted_credits =
            u128::from(absorbed_credits_observed).checked_mul(u128::from(absorbed_lamports))?;
        // Discard fractional credits as a merge side-effect friction by taking
        // the ceiling, done by adding `denominator - 1` to the numerator.
        let total_weighted_credits = stake_weighted_credits
            .checked_add(absorbed_weighted_credits)?
            .checked_add(total_stake)?
            .checked_sub(1)?;
        u64::try_from(total_weighted_credits.checked_div(total_stake)?).ok()
    }
}

pub type RewriteStakeStatus = (&'static str, (u64, u64), (u64, u64));

// utility function, used by runtime::Stakes, tests
pub fn new_stake_history_entry<'a, I>(
    epoch: Epoch,
    stakes: I,
    history: &StakeHistory,
    new_rate_activation_epoch: Option<Epoch>,
) -> StakeHistoryEntry
where
    I: Iterator<Item = &'a Delegation>,
{
    stakes.fold(StakeHistoryEntry::default(), |sum, stake| {
        sum + stake.stake_activating_and_deactivating(epoch, history, new_rate_activation_epoch)
    })
}

// utility function, used by tests
pub fn create_stake_history_from_delegations(
    bootstrap: Option<u64>,
    epochs: std::ops::Range<Epoch>,
    delegations: &[Delegation],
    new_rate_activation_epoch: Option<Epoch>,
) -> StakeHistory {
    let mut stake_history = StakeHistory::default();

    let bootstrap_delegation = if let Some(bootstrap) = bootstrap {
        vec![Delegation {
            activation_epoch: u64::MAX,
            stake: bootstrap,
            ..Delegation::default()
        }]
    } else {
        vec![]
    };

    for epoch in epochs {
        let entry = new_stake_history_entry(
            epoch,
            delegations.iter().chain(bootstrap_delegation.iter()),
            &stake_history,
            new_rate_activation_epoch,
        );
        stake_history.add(epoch, entry);
    }

    stake_history
}

// genesis investor accounts
pub fn create_lockup_stake_account(
    authorized: &Authorized,
    lockup: &Lockup,
    rent: &Rent,
    lamports: u64,
) -> AccountSharedData {
    let mut stake_account = AccountSharedData::new(lamports, StakeStateV2::size_of(), &id());

    let rent_exempt_reserve = rent.minimum_balance(stake_account.data().len());
    assert!(
        lamports >= rent_exempt_reserve,
        "lamports: {lamports} is less than rent_exempt_reserve {rent_exempt_reserve}"
    );

    stake_account
        .set_state(&StakeStateV2::Initialized(Meta {
            authorized: *authorized,
            lockup: *lockup,
            rent_exempt_reserve,
        }))
        .expect("set_state");

    stake_account
}

// utility function, used by Bank, tests, genesis for bootstrap
pub fn create_account(
    authorized: &Pubkey,
    voter_pubkey: &Pubkey,
    vote_account: &AccountSharedData,
    rent: &Rent,
    lamports: u64,
) -> AccountSharedData {
    do_create_account(
        authorized,
        voter_pubkey,
        vote_account,
        rent,
        lamports,
        Epoch::MAX,
    )
}

// utility function, used by tests
pub fn create_account_with_activation_epoch(
    authorized: &Pubkey,
    voter_pubkey: &Pubkey,
    vote_account: &AccountSharedData,
    rent: &Rent,
    lamports: u64,
    activation_epoch: Epoch,
) -> AccountSharedData {
    do_create_account(
        authorized,
        voter_pubkey,
        vote_account,
        rent,
        lamports,
        activation_epoch,
    )
}

fn do_create_account(
    authorized: &Pubkey,
    voter_pubkey: &Pubkey,
    vote_account: &AccountSharedData,
    rent: &Rent,
    lamports: u64,
    activation_epoch: Epoch,
) -> AccountSharedData {
    let mut stake_account = AccountSharedData::new(lamports, StakeStateV2::size_of(), &id());

    let vote_state = VoteStateV3::deserialize(vote_account.data()).expect("vote_state");

    let rent_exempt_reserve = rent.minimum_balance(stake_account.data().len());

    stake_account
        .set_state(&StakeStateV2::Stake(
            Meta {
                authorized: Authorized::auto(authorized),
                rent_exempt_reserve,
                ..Meta::default()
            },
            new_stake(
                lamports - rent_exempt_reserve, // underflow is an error, is basically: assert!(lamports > rent_exempt_reserve);
                voter_pubkey,
                &vote_state,
                activation_epoch,
            ),
            StakeFlags::empty(),
        ))
        .expect("set_state");

    stake_account
}
