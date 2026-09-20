//! Epoch inflation rewards, mirroring agave's Bank. The f64 operation order is consensus-relevant
//! and matches agave exactly rather than being algebraically rearranged.
//!
//! The vendored points/rewards API is deprecated upstream because agave moved it into the runtime
//! crate at 3.x; this is the 2.2.17 form, which is what mainnet ran at epoch 808.
#![allow(deprecated)]

use std::collections::HashMap;

use agave_feature_set::{FeatureSet, pico_inflation, reduce_stake_warmup_cooldown};
use solana_account::AccountSharedData;
use solana_epoch_rewards::EpochRewards;
use solana_epoch_rewards_hasher::EpochRewardsHasher;
use solana_hash::Hash;
use solana_inflation::Inflation;
use solana_pubkey::Pubkey;
use solana_stake_interface::{
    stake_history::StakeHistory,
    state::{Stake, StakeActivationStatus, StakeStateV2},
};
use solana_stake_program::solana_vote_interface::state::{VoteStateV3, VoteStateVersions};
use solana_stake_program::{
    points::{InflationPointCalculationEvent, PointValue, calculate_points},
    rewards::redeem_rewards,
};

use crate::{ReplayBank, SLOTS_PER_EPOCH, stake_state_of};
use solana_svm::transaction_processing_callback::TransactionProcessingCallback;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PrevEpochInflationRewards {
    pub validator_rewards: u64,
    pub prev_epoch_duration_in_years: f64,
    pub validator_rate: f64,
    pub foundation_rate: f64,
}

// agave Bank::get_inflation_start_slot. Accrual starts the epoch BEFORE activation, so the year fed
// to the taper is not measured from genesis.
fn inflation_start_slot(feature_set: &FeatureSet) -> u64 {
    let mut slots: Vec<u64> = feature_set
        .full_inflation_features_enabled()
        .iter()
        .filter_map(|id| feature_set.activated_slot(id))
        .collect();
    slots.sort_unstable();
    slots.first().copied().unwrap_or_else(|| {
        feature_set
            .activated_slot(&pico_inflation::id())
            .unwrap_or(0)
    })
}

fn inflation_num_slots(feature_set: &FeatureSet, epoch: u64) -> u64 {
    let activation = inflation_start_slot(feature_set);
    let start_epoch = (activation / SLOTS_PER_EPOCH).saturating_sub(1);
    (epoch * SLOTS_PER_EPOCH).saturating_sub(start_epoch * SLOTS_PER_EPOCH)
}

pub fn slot_in_year_for_inflation(
    feature_set: &FeatureSet,
    epoch: u64,
    slots_per_year: f64,
) -> f64 {
    inflation_num_slots(feature_set, epoch) as f64 / slots_per_year
}

// agave Bank::new_warmup_cooldown_rate_epoch. None here silently selects the OLD 0.25 warmup rate
// instead of 0.09, which would skew every effective-stake calculation.
pub fn new_warmup_cooldown_rate_epoch(feature_set: &FeatureSet) -> Option<u64> {
    feature_set
        .activated_slot(&reduce_stake_warmup_cooldown::id())
        .map(crate::epoch_of)
}

pub fn epoch_duration_in_years(slots_per_year: f64) -> f64 {
    SLOTS_PER_EPOCH as f64 / slots_per_year
}

// agave Bank::calculate_previous_epoch_inflation_rewards.
pub fn previous_epoch_inflation_rewards(
    feature_set: &FeatureSet,
    inflation: &Inflation,
    prev_epoch_capitalization: u64,
    epoch: u64,
    slots_per_year: f64,
) -> PrevEpochInflationRewards {
    let slot_in_year = slot_in_year_for_inflation(feature_set, epoch, slots_per_year);
    let validator_rate = inflation.validator(slot_in_year);
    let foundation_rate = inflation.foundation(slot_in_year);
    let prev_epoch_duration_in_years = epoch_duration_in_years(slots_per_year);

    let validator_rewards =
        (validator_rate * prev_epoch_capitalization as f64 * prev_epoch_duration_in_years) as u64;

    PrevEpochInflationRewards {
        validator_rewards,
        prev_epoch_duration_in_years,
        validator_rate,
        foundation_rate,
    }
}

// Owner-only, like agave's VoteAccount::try_from. An is_correct_size_and_initialized check here
// would reject not-yet-resized 3731-byte V1_14_11 accounts and inflate every reward.
pub fn vote_state_of(account: &AccountSharedData) -> Option<VoteStateV3> {
    use solana_account::ReadableAccount;
    if account.lamports() == 0 || *account.owner() != solana_sdk_ids::vote::id() {
        return None;
    }
    bincode::deserialize::<VoteStateVersions>(account.data())
        .ok()
        .map(VoteStateVersions::convert_to_v3)
}

pub fn stake_history_of(bank: &ReplayBank) -> Option<StakeHistory> {
    use solana_account::ReadableAccount;
    let (account, _) = bank.get_account_shared_data(&solana_sdk_ids::sysvar::stake_history::id())?;
    bincode::deserialize::<StakeHistory>(account.data()).ok()
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StakeReward {
    pub stake_pubkey: Pubkey,
    pub lamports: u64,
    /// Stake as of calculation time; distribution re-applies it to the account's current balance.
    pub stake: Stake,
}

pub struct EpochRewardsCalculation {
    pub point_value: Option<PointValue>,
    pub stake_rewards: Vec<StakeReward>,
    pub vote_commission: HashMap<Pubkey, u64>,
}

// agave calculate_reward_points_partitioned + calculate_stake_vote_rewards. The surviving count sets
// num_partitions: paid only if the vote account exists, is vote-owned, and redeem_rewards succeeds.
pub fn calculate_epoch_rewards(
    bank: &ReplayBank,
    vote_cache: &std::collections::HashSet<Pubkey>,
    validator_rewards: u64,
    rewarded_epoch: u64,
    new_rate_activation_epoch: Option<u64>,
) -> EpochRewardsCalculation {
    let delegations = bank.stake_delegations();
    let stake_history = stake_history_of(bank).unwrap_or_default();

    // The manifest's vote set is NOT agave's epoch stakes; gating on it drops rewarded delegations.
    let _ = vote_cache;
    let vote_state_for = |voter: &Pubkey| -> Option<VoteStateV3> {
        let (account, _) = bank.get_account_shared_data(voter)?;
        vote_state_of(&account)
    };

    let mut points: u128 = 0;
    for (stake_pubkey, delegation) in &delegations {
        let Some((account, _)) = bank.get_account_shared_data(stake_pubkey) else {
            continue;
        };
        let Some(state) = stake_state_of(&account) else {
            continue;
        };
        let Some(vote_state) = vote_state_for(&delegation.voter_pubkey) else {
            continue;
        };
        if let Ok(p) = calculate_points(
            // match calculate_points(
            &state,
            &vote_state,
            &stake_history,
            new_rate_activation_epoch,
        ) {
            points += p;
        }
    }

    let Some(point_value) = (points > 0).then_some(PointValue {
        rewards: validator_rewards,
        points,
    }) else {
        return EpochRewardsCalculation {
            point_value: None,
            stake_rewards: Vec::new(),
            vote_commission: HashMap::new(),
        };
    };

    let mut stake_rewards = Vec::new();
    let mut vote_commission: HashMap<Pubkey, u64> = HashMap::new();
    for (stake_pubkey, delegation) in &delegations {
        let Some((mut account, _)) = bank.get_account_shared_data(stake_pubkey) else {
            continue;
        };
        let Some(state) = stake_state_of(&account) else {
            continue;
        };
        let Some(vote_state) = vote_state_for(&delegation.voter_pubkey) else {
            continue;
        };
        let redeemed = redeem_rewards(
            rewarded_epoch,
            state,
            &mut account,
            &vote_state,
            &point_value,
            &stake_history,
            None::<fn(&InflationPointCalculationEvent)>,
            new_rate_activation_epoch,
        );
        if let Ok((stakers_reward, voters_reward)) = redeemed {
            let Some(StakeStateV2::Stake(_, stake, _)) = stake_state_of(&account) else {
                continue;
            };
            *vote_commission.entry(delegation.voter_pubkey).or_default() += voters_reward;
            stake_rewards.push(StakeReward {
                stake_pubkey: *stake_pubkey,
                lamports: stakers_reward,
                stake,
            });
        }
    }

    EpochRewardsCalculation {
        point_value: Some(point_value),
        stake_rewards,
        vote_commission,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mainnet_curve() -> Inflation {
        let mut i = Inflation::default();
        i.foundation = 0.0;
        i.foundation_term = 0.0;
        i
    }

    #[test]
    fn inflation_starts_the_epoch_before_activation() {
        let mut fs = FeatureSet::default();
        fs.activate(&pico_inflation::id(), 100 * SLOTS_PER_EPOCH + 5);
        assert_eq!(
            inflation_num_slots(&fs, 110),
            (110 - 99) * SLOTS_PER_EPOCH,
            "11 epochs of accrual, not 10"
        );
    }

    // None here would select the 0.25 warmup rate instead of 0.09 and skew every effective stake.
    #[test]
    fn an_active_warmup_feature_yields_its_activation_epoch() {
        let mut fs = FeatureSet::default();
        assert_eq!(new_warmup_cooldown_rate_epoch(&fs), None);

        fs.activate(
            &reduce_stake_warmup_cooldown::id(),
            700 * SLOTS_PER_EPOCH + 9,
        );
        assert_eq!(new_warmup_cooldown_rate_epoch(&fs), Some(700));
    }

    #[test]
    fn no_inflation_feature_means_accrual_from_genesis() {
        let fs = FeatureSet::default();
        assert_eq!(inflation_num_slots(&fs, 10), 10 * SLOTS_PER_EPOCH);
    }

    #[test]
    fn a_zero_foundation_gives_validators_the_whole_curve() {
        let mut fs = FeatureSet::default();
        fs.activate(&pico_inflation::id(), 0);
        let spy = 78_892_314.0;
        let r = previous_epoch_inflation_rewards(&fs, &mainnet_curve(), 600_000_000, 5, spy);
        let year = slot_in_year_for_inflation(&fs, 5, spy);
        assert_eq!(r.foundation_rate, 0.0);
        assert_eq!(r.validator_rate, mainnet_curve().total(year));
    }

    #[test]
    fn the_reward_total_matches_agaves_operation_order() {
        let mut fs = FeatureSet::default();
        fs.activate(&pico_inflation::id(), 0);
        let inflation = mainnet_curve();
        let cap = 603_724_512_541_705_391u64;
        let slots_per_year = 78_892_314.984_f64;

        let r = previous_epoch_inflation_rewards(&fs, &inflation, cap, 808, slots_per_year);
        let expected = (r.validator_rate * cap as f64 * r.prev_epoch_duration_in_years) as u64;
        assert_eq!(r.validator_rewards, expected);
        assert!(r.validator_rewards > 0);
    }
}

/// agave `MAX_PARTITIONED_REWARDS_PER_BLOCK`: the 400ms-slot baseline for stake accounts per block.
pub const STAKE_ACCOUNT_STORES_PER_BLOCK: u64 = 4096;

// agave get_reward_distribution_num_blocks. Derived from the COUNT of paid delegations, so an
// off-by-one here reseeds the hasher and moves every account to a different block.
pub fn num_reward_partitions(paid_delegations: usize) -> u64 {
    const MAX_FACTOR_OF_REWARD_BLOCKS_IN_EPOCH: u64 = 10;
    let chunks = paid_delegations.div_ceil(STAKE_ACCOUNT_STORES_PER_BLOCK as usize) as u64;
    chunks.clamp(
        1,
        (SLOTS_PER_EPOCH / MAX_FACTOR_OF_REWARD_BLOCKS_IN_EPOCH).max(1),
    )
}

// agave hash_rewards_into_partitions, seeded by num_partitions and the boundary's parent blockhash.
pub fn partition_rewards<T: Clone>(
    rewards: &[(Pubkey, T)],
    parent_blockhash: &Hash,
    num_partitions: usize,
) -> Vec<Vec<(Pubkey, T)>> {
    let hasher = EpochRewardsHasher::new(num_partitions, parent_blockhash);
    let mut out = vec![Vec::new(); num_partitions];
    for entry in rewards {
        let index = hasher.clone().hash_address_to_partition(&entry.0);
        out[index].push(entry.clone());
    }
    out
}

// agave distribute_partitioned_epoch_rewards: block HEIGHT, not slot, because mainnet skips slots.
pub fn partition_for_block(
    block_height: u64,
    distribution_starting_block_height: u64,
    num_partitions: u64,
) -> Option<u64> {
    let end = distribution_starting_block_height.checked_add(num_partitions)?;
    (block_height >= distribution_starting_block_height && block_height < end)
        .then(|| block_height - distribution_starting_block_height)
}

pub fn epoch_rewards_of(bank: &ReplayBank) -> EpochRewards {
    use solana_account::ReadableAccount;
    bank.get_account_shared_data(&solana_sdk_ids::sysvar::epoch_rewards::id())
        .and_then(|(a, _)| bincode::deserialize::<EpochRewards>(a.data()).ok())
        .unwrap_or_default()
}

fn write_epoch_rewards(bank: &mut ReplayBank, rewards: &EpochRewards, slot: u64) {
    bank.set_sysvar_at(
        solana_sdk_ids::sysvar::epoch_rewards::id(),
        bincode::serialize(rewards).expect("EpochRewards serialises"),
        slot,
    );
}

// agave create_epoch_rewards_sysvar. parent_blockhash seeds the hasher, so it is the boundary's parent.
pub fn begin_epoch_rewards(
    bank: &mut ReplayBank,
    point_value: &PointValue,
    // Commission credited at the boundary; partitions add to it. total_rewards stays the pool.
    distributed_rewards: u64,
    distribution_starting_block_height: u64,
    num_partitions: u64,
    parent_blockhash: Hash,
    slot: u64,
) {
    write_epoch_rewards(
        bank,
        &EpochRewards {
            distribution_starting_block_height,
            num_partitions,
            parent_blockhash,
            total_points: point_value.points,
            total_rewards: point_value.rewards,
            distributed_rewards,
            active: true,
        },
        slot,
    );
}

pub fn deactivate_epoch_rewards(bank: &mut ReplayBank, slot: u64) {
    let mut rewards = epoch_rewards_of(bank);
    rewards.active = false;
    write_epoch_rewards(bank, &rewards, slot);
}

/// Lamports actually credited, and lamports burned because the account no longer qualifies.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Distributed {
    pub credited: u64,
    pub burned: u64,
}

// agave store_stake_accounts_in_partition: credit the CURRENT balance, write the calculation-time
// Stake over it, and burn rather than pay if the account stopped qualifying since calculation.
pub fn distribute_partition(
    bank: &mut ReplayBank,
    partition: &[StakeReward],
    slot: u64,
) -> Distributed {
    use solana_account::{ReadableAccount, WritableAccount};
    let mut out = Distributed::default();
    for reward in partition {
        let current = bank
            .get_account_shared_data(&reward.stake_pubkey)
            .and_then(|(a, _)| stake_state_of(&a).map(|state| (a, state)));
        let Some((mut account, StakeStateV2::Stake(meta, _, flags))) = current else {
            out.burned += reward.lamports;
            continue;
        };
        let Some(lamports) = account.lamports().checked_add(reward.lamports) else {
            out.burned += reward.lamports;
            continue;
        };
        account.set_lamports(lamports);
        if bincode::serialize_into(
            account.data_as_mut_slice(),
            &StakeStateV2::Stake(meta, reward.stake, flags),
        )
        .is_err()
        {
            out.burned += reward.lamports;
            continue;
        }
        bank.insert(reward.stake_pubkey, account, slot);
        out.credited += reward.lamports;
    }

    let mut rewards = epoch_rewards_of(bank);
    rewards.distributed_rewards = rewards
        .distributed_rewards
        .saturating_add(out.credited)
        .saturating_add(out.burned);
    write_epoch_rewards(bank, &rewards, slot);
    out
}

#[cfg(test)]
mod distribution_tests {
    use super::{reward_tests::*, *};
    use solana_stake_interface::state::Delegation;

    fn reward_for(pk: Pubkey, lamports: u64, stake_amount: u64) -> StakeReward {
        StakeReward {
            stake_pubkey: pk,
            lamports,
            stake: Stake {
                delegation: Delegation {
                    stake: stake_amount,
                    ..Delegation::default()
                },
                credits_observed: 42,
            },
        }
    }

    #[test]
    fn a_reward_credits_the_accounts_current_balance() {
        use solana_account::ReadableAccount;
        let pk = Pubkey::new_unique();
        let mut bank = ReplayBank::default();
        bank.insert(pk, stake_account(Pubkey::new_unique(), 5_000_000_000), 0);
        begin_epoch_rewards(
            &mut bank,
            &PointValue {
                rewards: 10_000,
                points: 1,
            },
            0,
            100,
            2,
            Hash::default(),
            9,
        );

        let out = distribute_partition(&mut bank, &[reward_for(pk, 777, 5_000_000_777)], 9);
        assert_eq!(
            out,
            Distributed {
                credited: 777,
                burned: 0
            }
        );

        let (account, _) = bank.get_account_shared_data(&pk).unwrap();
        assert_eq!(account.lamports(), 5_000_000_777);
        let Some(StakeStateV2::Stake(_, stake, _)) = stake_state_of(&account) else {
            panic!("still a delegated stake account")
        };
        assert_eq!(
            stake.credits_observed, 42,
            "calculation-time Stake is written over"
        );
        assert_eq!(epoch_rewards_of(&bank).distributed_rewards, 777);
    }

    #[test]
    fn a_vanished_account_has_its_reward_burned() {
        let mut bank = ReplayBank::default();
        begin_epoch_rewards(
            &mut bank,
            &PointValue {
                rewards: 10_000,
                points: 1,
            },
            0,
            100,
            2,
            Hash::default(),
            9,
        );

        let out = distribute_partition(&mut bank, &[reward_for(Pubkey::new_unique(), 500, 1)], 9);
        assert_eq!(
            out,
            Distributed {
                credited: 0,
                burned: 500
            }
        );
        assert_eq!(
            epoch_rewards_of(&bank).distributed_rewards,
            500,
            "burned lamports still count as distributed"
        );
    }

    #[test]
    fn deactivating_closes_the_window_without_losing_totals() {
        let mut bank = ReplayBank::default();
        begin_epoch_rewards(
            &mut bank,
            &PointValue {
                rewards: 10_000,
                points: 7,
            },
            0,
            100,
            2,
            Hash::default(),
            9,
        );
        assert!(epoch_rewards_of(&bank).active);

        deactivate_epoch_rewards(&mut bank, 9);
        let r = epoch_rewards_of(&bank);
        assert!(!r.active);
        assert_eq!(r.total_rewards, 10_000);
        assert_eq!(r.total_points, 7);
    }
}

// agave Stakes::activate_epoch. `ending_epoch` is the epoch just finished; its entry is what the
// next epoch's warmup and reward math read back.
pub fn roll_stake_history(
    bank: &mut ReplayBank,
    feature_set: &FeatureSet,
    ending_epoch: u64,
    slot: u64,
) -> StakeHistory {
    let mut history = stake_history_of(bank).unwrap_or_default();
    let new_rate_epoch = new_warmup_cooldown_rate_epoch(feature_set);
    let entry = bank.stake_delegations().iter().fold(
        StakeActivationStatus::default(),
        |acc, (_, delegation)| {
            acc + delegation.stake_activating_and_deactivating(
                ending_epoch,
                &history,
                new_rate_epoch,
            )
        },
    );
    history.add(ending_epoch, entry);
    bank.set_sysvar_at(
        solana_sdk_ids::sysvar::stake_history::id(),
        bincode::serialize(&history).expect("StakeHistory serialises"),
        slot,
    );
    history
}

// agave store_vote_accounts_partitioned: commission is paid at the boundary, not spread over blocks.
pub fn credit_vote_commission(
    bank: &mut ReplayBank,
    commission: &HashMap<Pubkey, u64>,
    slot: u64,
) -> u64 {
    use solana_account::{ReadableAccount, WritableAccount};
    let mut paid = 0;
    let mut entries: Vec<_> = commission.iter().collect();
    entries.sort_unstable_by_key(|(pubkey, _)| **pubkey);
    for (pubkey, lamports) in entries {
        if *lamports == 0 {
            continue;
        }
        let Some((mut account, _)) = bank.get_account_shared_data(pubkey) else {
            continue;
        };
        if vote_state_of(&account).is_none() {
            continue;
        }
        let Some(total) = account.lamports().checked_add(*lamports) else {
            continue;
        };
        account.set_lamports(total);
        bank.insert(*pubkey, account, slot);
        paid += lamports;
    }
    paid
}

/// The manifest half of the reward inputs; the feature set is built from the seeded bank.
#[derive(Debug, Clone)]
pub struct ManifestRewardInputs {
    pub inflation: Inflation,
    pub capitalization: u64,
    pub slots_per_year: f64,
    /// agave's StakesCache, read from the manifest. Scanning accounts instead yields a superset:
    /// at slot 349047024 the scan finds 1,100,650 delegated accounts against the cache's 1,097,015.
    pub stake_delegations: std::collections::HashSet<Pubkey>,
    /// agave resolves voters through this cache, not a bank lookup: 905 referenced voters are
    /// absent from it at slot 349047024, and the 4,387 delegations behind them earn nothing.
    pub vote_accounts: std::collections::HashSet<Pubkey>,
}

#[derive(Debug, Clone)]
pub struct RewardInputs {
    pub feature_set: FeatureSet,
    pub inflation: Inflation,
    pub capitalization: u64,
    pub slots_per_year: f64,
    pub vote_accounts: std::collections::HashSet<Pubkey>,
}

impl RewardInputs {
    pub fn new(feature_set: FeatureSet, manifest: &ManifestRewardInputs) -> Self {
        Self {
            feature_set,
            inflation: manifest.inflation,
            capitalization: manifest.capitalization,
            slots_per_year: manifest.slots_per_year,
            vote_accounts: manifest.vote_accounts.clone(),
        }
    }
}

/// State a crossing leaves behind for the blocks that follow it.
pub struct BoundaryOutcome {
    pub activated_features: Vec<Pubkey>,
    pub paid_delegations: usize,
    pub num_partitions: u64,
    pub commission_paid: u64,
    pub partitions: Vec<Vec<StakeReward>>,
}

// agave process_new_epoch. The order is consensus-critical and the caller owns the first two
// steps: feature activation, then any core-BPF migration the activation triggers (it moves
// capitalization, which validator_rewards below is computed from), then the stake-history entry,
// then rewards, which read that entry back.
pub fn process_epoch_boundary(
    bank: &mut ReplayBank,
    inputs: &RewardInputs,
    epoch: u64,
    slot: u64,
    parent_blockhash: Hash,
    block_height: u64,
    activated_features: Vec<Pubkey>,
) -> BoundaryOutcome {
    let RewardInputs {
        feature_set,
        inflation,
        capitalization: _,
        slots_per_year,
        vote_accounts: _,
    } = inputs;
    let prev_epoch = epoch.saturating_sub(1);
    roll_stake_history(bank, feature_set, prev_epoch, slot);

    // The bank's running value: the manifest's is stale by the fees burned since the seed slot.
    if std::env::var("SLATE_DUMP_REWARD_INPUTS").is_ok() {
        eprintln!(
            "REWARDINPUTS slots_per_year={} inflation={:?} capitalization_now={}",
            slots_per_year,
            inflation,
            bank.capitalization_now()
        );
    }
    let inflation_rewards = previous_epoch_inflation_rewards(
        feature_set,
        inflation,
        bank.capitalization_now(),
        epoch,
        *slots_per_year,
    );
    let delegation_count = bank.stake_delegations().len();
    let new_rate_epoch = new_warmup_cooldown_rate_epoch(feature_set);
    let calculated = calculate_epoch_rewards(
        bank,
        &inputs.vote_accounts,
        inflation_rewards.validator_rewards,
        prev_epoch,
        new_rate_epoch,
    );

    if let Ok(path) = std::env::var("SLATE_DUMP_REWARDS") {
        use std::io::Write;
        if let Ok(f) = std::fs::File::create(&path) {
            let mut f = std::io::BufWriter::new(f);
            for r in &calculated.stake_rewards {
                let _ = writeln!(
                    f,
                    "{} reward={} stake={} credits={}",
                    r.stake_pubkey, r.lamports, r.stake.delegation.stake, r.stake.credits_observed
                );
            }
            let _ = f.flush();
            eprintln!("wrote {} stake rewards to {path}", calculated.stake_rewards.len());
        }
    }

    let commission_paid = credit_vote_commission(bank, &calculated.vote_commission, slot);
    let paid_delegations = calculated.stake_rewards.len();
    let num_partitions = num_reward_partitions(paid_delegations);

    let indexed: Vec<(Pubkey, StakeReward)> = calculated
        .stake_rewards
        .iter()
        .map(|r| (r.stake_pubkey, *r))
        .collect();
    let partitions: Vec<Vec<StakeReward>> =
        partition_rewards(&indexed, &parent_blockhash, num_partitions as usize)
            .into_iter()
            .map(|part| part.into_iter().map(|(_, r)| r).collect())
            .collect();

    if let Some(point_value) = &calculated.point_value {
        // Distribution opens one block after the calculation block.
        begin_epoch_rewards(
            bank,
            point_value,
            commission_paid,
            block_height + REWARD_CALCULATION_NUM_BLOCKS,
            num_partitions,
            parent_blockhash,
            slot,
        );
    }

    eprintln!(
        "epoch boundary slot {slot} epoch {epoch}: features activated {}, delegations {}, paid {}, \
         partitions {}, inflation {} lamports ({:.2} SOL), points {}, commission {} accounts / {} \
         lamports ({:.2} SOL), distribution starts at block height {}",
        activated_features.len(),
        delegation_count,
        paid_delegations,
        num_partitions,
        inflation_rewards.validator_rewards,
        inflation_rewards.validator_rewards as f64 / 1e9,
        calculated
            .point_value
            .as_ref()
            .map(|p| p.points)
            .unwrap_or(0),
        calculated
            .vote_commission
            .values()
            .filter(|v| **v > 0)
            .count(),
        commission_paid,
        commission_paid as f64 / 1e9,
        block_height + REWARD_CALCULATION_NUM_BLOCKS,
    );

    BoundaryOutcome {
        activated_features,
        paid_delegations,
        num_partitions,
        commission_paid,
        partitions,
    }
}

// agave distribute_partitioned_epoch_rewards, run on every block: pay this block's partition if one
// is due, and close the window once the last one has gone out.
pub fn distribute_due_partition(bank: &mut ReplayBank, block_height: u64, slot: u64) {
    let sysvar = epoch_rewards_of(bank);
    if !sysvar.active {
        return;
    }
    let start = sysvar.distribution_starting_block_height;
    if let Some(index) = partition_for_block(block_height, start, sysvar.num_partitions) {
        let partition = bank
            .pending_partitions
            .get(index as usize)
            .cloned()
            .unwrap_or_default();
        distribute_partition(bank, &partition, slot);
    }
    if block_height + 1 >= start + sysvar.num_partitions {
        deactivate_epoch_rewards(bank, slot);
    }
}

// Checkpoint encoding for the pending partitions. Explicit bytes rather than bincode of a
// version-specific type: the checkpoint is read by whichever binary resumes, which may not be the
// one that wrote it. version(4) ++ partitions(4) ++ per partition [count(4) ++ count * 112 bytes].
// StakeReward is built from this era's solana-stake-interface types, so the checkpoint
// stores the fields flat.
pub fn to_reward_record(r: &StakeReward) -> slate_format::StakeRewardRecord {
    slate_format::StakeRewardRecord {
        stake_pubkey: r.stake_pubkey.to_bytes(),
        lamports: r.lamports,
        voter_pubkey: r.stake.delegation.voter_pubkey.to_bytes(),
        stake: r.stake.delegation.stake,
        activation_epoch: r.stake.delegation.activation_epoch,
        deactivation_epoch: r.stake.delegation.deactivation_epoch,
        warmup_cooldown_rate: r.stake.delegation.warmup_cooldown_rate,
        credits_observed: r.stake.credits_observed,
    }
}

pub fn from_reward_record(r: &slate_format::StakeRewardRecord) -> StakeReward {
    StakeReward {
        stake_pubkey: Pubkey::new_from_array(r.stake_pubkey),
        lamports: r.lamports,
        stake: Stake {
            delegation: solana_stake_interface::state::Delegation {
                voter_pubkey: Pubkey::new_from_array(r.voter_pubkey),
                stake: r.stake,
                activation_epoch: r.activation_epoch,
                deactivation_epoch: r.deactivation_epoch,
                warmup_cooldown_rate: r.warmup_cooldown_rate,
            },
            credits_observed: r.credits_observed,
        },
    }
}

/// agave REWARD_CALCULATION_NUM_BLOCKS: one block between calculation and the first payout.
pub const REWARD_CALCULATION_NUM_BLOCKS: u64 = 1;

#[cfg(test)]
mod boundary_tests {
    use super::{reward_tests::*, *};

    fn mainnet_curve() -> Inflation {
        let mut i = Inflation::default();
        i.foundation = 0.0;
        i.foundation_term = 0.0;
        i
    }

    fn inputs(feature_set: FeatureSet, capitalization: u64, bank: &mut ReplayBank) -> RewardInputs {
        bank.set_capitalization(capitalization);
        RewardInputs {
            feature_set,
            inflation: mainnet_curve(),
            capitalization,
            slots_per_year: 78_892_314.984,
            vote_accounts: reward_tests::all_voters(bank),
        }
    }

    fn bank_with_one_delegation() -> (ReplayBank, Pubkey, Pubkey) {
        let (stake_pk, voter) = (Pubkey::new_unique(), Pubkey::new_unique());
        let mut bank = ReplayBank::default();
        bank.insert(stake_pk, stake_account(voter, 5_000_000_000), 0);
        bank.insert(voter, vote_account(10, 1_000), 0);
        bank.set_stake_keys([stake_pk].into_iter().collect());
        (bank, stake_pk, voter)
    }

    #[test]
    fn pending_partitions_survive_a_checkpoint_round_trip() {
        let reward = |seed: u8, lamports: u64| StakeReward {
            stake_pubkey: Pubkey::new_from_array([seed; 32]),
            lamports,
            stake: Stake {
                delegation: solana_stake_interface::state::Delegation {
                    voter_pubkey: Pubkey::new_from_array([seed.wrapping_add(1); 32]),
                    stake: 5_000 + u64::from(seed),
                    activation_epoch: 700,
                    deactivation_epoch: u64::MAX,
                    warmup_cooldown_rate: 0.09,
                },
                credits_observed: 12_345 + u64::from(seed),
            },
        };
        let partitions = vec![
            vec![reward(1, 10), reward(2, 20)],
            vec![],
            vec![reward(3, 30)],
        ];
        // Through the real byte layout, so a field dropped on either side shows up.
        let checkpoint = slate_format::Checkpoint {
            slot: 349_056_100,
            capitalization: 1,
            roll: None,
            stake_keys: vec![],
            pending_partitions: partitions
                .iter()
                .map(|p| p.iter().map(to_reward_record).collect())
                .collect(),
        };
        let back: Vec<Vec<StakeReward>> = slate_format::Checkpoint::decode(&checkpoint.encode())
            .unwrap()
            .pending_partitions
            .iter()
            .map(|p| p.iter().map(from_reward_record).collect())
            .collect();
        assert_eq!(back, partitions);
    }

    #[test]
    fn a_boundary_pays_commission_partitions_stakes_and_opens_the_window() {
        let (mut bank, stake_pk, voter) = bank_with_one_delegation();
        let fs = FeatureSet::all_enabled();

        let i = inputs(fs, 603_724_512_541_705_391, &mut bank);
        let activated = crate::activate_pending_features(&mut bank, 349_056_000);
        let out =
            process_epoch_boundary(&mut bank, &i, 808, 349_056_000, Hash::new_unique(), 1_000, activated);

        assert_eq!(out.paid_delegations, 1);
        assert_eq!(out.num_partitions, 1);
        assert!(
            out.commission_paid > 0,
            "vote commission is paid at the boundary"
        );
        assert_eq!(out.partitions.iter().map(Vec::len).sum::<usize>(), 1);
        assert_eq!(out.partitions[0][0].stake_pubkey, stake_pk);

        let sysvar = epoch_rewards_of(&bank);
        assert!(sysvar.active);
        assert_eq!(sysvar.num_partitions, 1);
        assert_eq!(
            sysvar.distribution_starting_block_height,
            1_000 + REWARD_CALCULATION_NUM_BLOCKS
        );
        assert_eq!(
            sysvar.distributed_rewards, out.commission_paid,
            "the vote commission is paid at the boundary, so it counts as distributed"
        );
        let staked: u64 = out.partitions.iter().flatten().map(|r| r.lamports).sum();
        assert!(
            sysvar.total_rewards > out.commission_paid + staked,
            "total_rewards is the whole inflation pool, not the part already paid"
        );

        use solana_account::ReadableAccount;
        let (vote, _) = bank.get_account_shared_data(&voter).unwrap();
        assert_eq!(vote.lamports(), 1_000_000_000 + out.commission_paid);
    }

    #[test]
    fn the_boundary_writes_a_stake_history_entry_for_the_ending_epoch() {
        let (mut bank, _, _) = bank_with_one_delegation();
        assert!(stake_history_of(&bank).is_none());

        let i = inputs(FeatureSet::all_enabled(), 600_000_000_000, &mut bank);
        let activated = crate::activate_pending_features(&mut bank, 349_056_000);
        process_epoch_boundary(&mut bank, &i, 808, 349_056_000, Hash::new_unique(), 1_000, activated);

        let history = stake_history_of(&bank).expect("entry written");
        assert!(
            history.iter().any(|(epoch, _)| *epoch == 807),
            "for the ENDING epoch, not 808"
        );
    }

    #[test]
    fn distributing_every_partition_closes_the_window() {
        let (mut bank, _, _) = bank_with_one_delegation();
        let i = inputs(
            FeatureSet::all_enabled(),
            603_724_512_541_705_391,
            &mut bank,
        );
        let activated = crate::activate_pending_features(&mut bank, 349_056_000);
        let out =
            process_epoch_boundary(&mut bank, &i, 808, 349_056_000, Hash::new_unique(), 1_000, activated);

        let start = epoch_rewards_of(&bank).distribution_starting_block_height;
        let index = partition_for_block(start, start, out.num_partitions).unwrap();
        let paid = distribute_partition(&mut bank, &out.partitions[index as usize], 349_056_001);
        assert!(paid.credited > 0);
        assert_eq!(paid.burned, 0);

        assert_eq!(
            partition_for_block(start + out.num_partitions, start, out.num_partitions),
            None
        );
        deactivate_epoch_rewards(&mut bank, 349_056_002);
        assert!(!epoch_rewards_of(&bank).active);
    }
}

#[cfg(test)]
mod partition_tests {
    use super::*;

    #[test]
    fn partition_count_is_one_block_per_4096_paid_delegations() {
        assert_eq!(num_reward_partitions(0), 1, "never zero blocks");
        assert_eq!(num_reward_partitions(1), 1);
        assert_eq!(num_reward_partitions(4096), 1);
        assert_eq!(
            num_reward_partitions(4097),
            2,
            "one over rolls to a second block"
        );
        assert_eq!(num_reward_partitions(1_300_000), 318);
    }

    #[test]
    fn partition_count_is_clamped_to_a_tenth_of_an_epoch() {
        let cap = SLOTS_PER_EPOCH / 10;
        assert_eq!(num_reward_partitions(usize::MAX / 2), cap);
    }

    fn entries(n: usize) -> Vec<(Pubkey, u64)> {
        (0..n).map(|i| (Pubkey::new_unique(), i as u64)).collect()
    }

    #[test]
    fn every_reward_lands_in_exactly_one_partition() {
        let rewards = entries(500);
        let parts = partition_rewards(&rewards, &Hash::new_unique(), 7);
        assert_eq!(parts.len(), 7);
        assert_eq!(parts.iter().map(Vec::len).sum::<usize>(), 500);

        let mut seen: Vec<Pubkey> = parts.iter().flatten().map(|(k, _)| *k).collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), 500, "no reward duplicated or dropped");
    }

    #[test]
    fn assignment_is_stable_for_the_same_seed() {
        let rewards = entries(200);
        let hash = Hash::new_unique();
        let a = partition_rewards(&rewards, &hash, 5);
        let b = partition_rewards(&rewards, &hash, 5);
        assert_eq!(
            a.iter().map(Vec::len).collect::<Vec<_>>(),
            b.iter().map(Vec::len).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_different_seed_reshuffles_the_assignment() {
        let rewards = entries(400);
        let a = partition_rewards(&rewards, &Hash::new_unique(), 9);
        let b = partition_rewards(&rewards, &Hash::new_unique(), 9);
        assert_ne!(
            a.iter().map(Vec::len).collect::<Vec<_>>(),
            b.iter().map(Vec::len).collect::<Vec<_>>()
        );
    }

    #[test]
    fn only_blocks_inside_the_distribution_window_pay() {
        let (start, n) = (1_000u64, 3u64);
        assert_eq!(
            partition_for_block(999, start, n),
            None,
            "before the window"
        );
        assert_eq!(partition_for_block(1_000, start, n), Some(0));
        assert_eq!(partition_for_block(1_002, start, n), Some(2));
        assert_eq!(
            partition_for_block(1_003, start, n),
            None,
            "end is exclusive"
        );
    }
}

#[cfg(test)]
pub(crate) mod reward_tests {
    use super::*;
    use solana_account::Account;
    use solana_stake_interface::{
        stake_flags::StakeFlags,
        state::{Delegation, Meta, Stake, StakeStateV2},
    };

    pub(crate) fn stake_account(voter: Pubkey, lamports: u64) -> AccountSharedData {
        let state = StakeStateV2::Stake(
            Meta::default(),
            Stake {
                delegation: Delegation {
                    voter_pubkey: voter,
                    stake: lamports,
                    activation_epoch: 0,
                    ..Delegation::default()
                },
                credits_observed: 0,
            },
            StakeFlags::empty(),
        );
        let mut data = vec![0u8; 200];
        bincode::serialize_into(data.as_mut_slice(), &state).unwrap();
        AccountSharedData::from(Account {
            lamports,
            data,
            owner: solana_sdk_ids::stake::id(),
            executable: false,
            rent_epoch: 0,
        })
    }

    pub(crate) fn vote_account(commission: u8, credits: u64) -> AccountSharedData {
        let mut vote = VoteStateV3 {
            commission,
            ..Default::default()
        };
        vote.increment_credits(100, credits);
        let mut data = vec![0u8; VoteStateV3::size_of()];
        bincode::serialize_into(data.as_mut_slice(), &VoteStateVersions::new_v3(vote)).unwrap();
        AccountSharedData::from(Account {
            lamports: 1_000_000_000,
            data,
            owner: solana_sdk_ids::vote::id(),
            executable: false,
            rent_epoch: 0,
        })
    }

    // Tests exercise the reward math, not the cache filter, so admit every voter they reference.
    pub(crate) fn all_voters(bank: &ReplayBank) -> std::collections::HashSet<Pubkey> {
        bank.stake_delegations()
            .iter()
            .map(|(_, d)| d.voter_pubkey)
            .collect()
    }

    fn bank_with(entries: &[(Pubkey, Pubkey)]) -> ReplayBank {
        let mut bank = ReplayBank::default();
        let mut keys = HashMap::new();
        for (stake_pk, voter) in entries {
            bank.insert(*stake_pk, stake_account(*voter, 5_000_000_000), 0);
            keys.insert(*stake_pk, ());
        }
        bank.set_stake_keys(keys.into_keys().collect());
        bank
    }

    #[test]
    fn a_delegation_with_no_vote_account_earns_nothing() {
        let (stake_pk, voter) = (Pubkey::new_unique(), Pubkey::new_unique());
        let bank = bank_with(&[(stake_pk, voter)]);
        let r = calculate_epoch_rewards(&bank, &all_voters(&bank), 1_000_000_000, 807, None);
        assert!(r.point_value.is_none());
        assert!(r.stake_rewards.is_empty());
    }

    #[test]
    fn a_delegation_whose_voter_is_not_vote_owned_earns_nothing() {
        let (stake_pk, voter) = (Pubkey::new_unique(), Pubkey::new_unique());
        let mut bank = bank_with(&[(stake_pk, voter)]);
        let mut wrong = vote_account(10, 100);
        solana_account::WritableAccount::set_owner(
            &mut wrong,
            solana_sdk_ids::system_program::id(),
        );
        bank.insert(voter, wrong, 0);

        let r = calculate_epoch_rewards(&bank, &all_voters(&bank), 1_000_000_000, 807, None);
        assert!(r.point_value.is_none());
        assert!(r.stake_rewards.is_empty());
    }

    #[test]
    fn a_funded_delegation_is_paid_and_the_commission_is_split() {
        let (stake_pk, voter) = (Pubkey::new_unique(), Pubkey::new_unique());
        let mut bank = bank_with(&[(stake_pk, voter)]);
        bank.insert(voter, vote_account(10, 1_000), 0);

        let r = calculate_epoch_rewards(&bank, &all_voters(&bank), 1_000_000_000, 807, None);
        let pv = r.point_value.expect("credits and stake produce points");
        assert!(pv.points > 0);
        assert_eq!(
            r.stake_rewards.len(),
            1,
            "this is the count num_partitions uses"
        );

        let paid = r.stake_rewards[0];
        assert_eq!(paid.stake_pubkey, stake_pk);
        assert!(paid.lamports > 0);

        let voters_reward = r.vote_commission[&voter];
        assert!(voters_reward > 0, "10% commission must pay the voter");
        assert!(
            paid.lamports > voters_reward * 5,
            "90/10 split: staker {} vs voter {voters_reward}",
            paid.lamports
        );
        assert_eq!(
            paid.stake.delegation.stake,
            5_000_000_000 + paid.lamports,
            "the calculation-time Stake already includes the reward"
        );
    }

    #[test]
    fn an_empty_bank_produces_no_point_value() {
        let bank = ReplayBank::default();
        let r = calculate_epoch_rewards(&bank, &all_voters(&bank), 1_000_000_000, 807, None);
        assert!(r.point_value.is_none());
        assert!(r.vote_commission.is_empty());
    }

    #[test]
    fn vote_state_membership_matches_agaves_rule() {
        assert!(vote_state_of(&vote_account(10, 5)).is_some());
        let mut closed = vote_account(10, 5);
        solana_account::WritableAccount::set_lamports(&mut closed, 0);
        assert!(vote_state_of(&closed).is_none());
    }
}

#[cfg(test)]
mod mainnet_tests {
    use super::*;
    use crate::{ReplayBank, build_feature_set, snapshot::read_manifest_fields, store::DiskStore};
    use std::fs::File;

    #[test]
    #[ignore = "needs the local mainnet snapshot + accounts store at /Users/mctursh/slate-data"]
    fn epoch_808_inflation_matches_mainnets_published_rate() {
        let snap = "/Users/mctursh/slate-data/\
                    snapshot-349047024-Cv8fHRuDLaRVhB8YTXGMxbMpZBC1BDGpN5MN99GFGqUv.tar.zst";
        let m = read_manifest_fields(File::open(snap).unwrap(), 349_047_024).unwrap();
        let inflation = m.inflation.expect("curve parsed");

        let store = DiskStore::create("/tmp/rewards-check.redb", 1 << 30).unwrap();
        let bank = ReplayBank::with_store(Box::new(store));
        let fs = build_feature_set(&bank, m.slot);

        let epoch = m.epoch + 1;
        let year = slot_in_year_for_inflation(&fs, epoch, m.slots_per_year);
        let r = previous_epoch_inflation_rewards(
            &fs,
            &inflation,
            m.capitalization,
            epoch,
            m.slots_per_year,
        );

        eprintln!(
            "epoch {epoch} | year {year:.4} | validator rate {:.6} | total {} lamports ({:.0} SOL)",
            r.validator_rate,
            r.validator_rewards,
            r.validator_rewards as f64 / 1e9
        );
        assert!(
            (0.030..0.055).contains(&r.validator_rate),
            "mainnet mid-2025 sits near 4%, got {}",
            r.validator_rate
        );
        assert!(r.validator_rewards > 0);
    }
}
