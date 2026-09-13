//! Information about stake and voter rewards based on stake state.
//! Used by `solana-runtime`.

use {
    crate::points::{
        calculate_stake_points_and_credits, CalculatedStakePoints, InflationPointCalculationEvent,
        PointValue, SkippedReason,
    },
    solana_account::{state_traits::StateMut, AccountSharedData, WritableAccount},
    solana_clock::Epoch,
    solana_instruction::error::InstructionError,
    solana_stake_interface::{
        error::StakeError,
        stake_history::StakeHistory,
        state::{Stake, StakeStateV2},
    },
    solana_vote_interface::state::VoteStateV3 as VoteState,
};

#[derive(Debug, PartialEq, Eq)]
struct CalculatedStakeRewards {
    staker_rewards: u64,
    voter_rewards: u64,
    new_credits_observed: u64,
}

// utility function, used by runtime
// returns a tuple of (stakers_reward,voters_reward)
#[doc(hidden)]
pub fn redeem_rewards(
    rewarded_epoch: Epoch,
    stake_state: StakeStateV2,
    stake_account: &mut AccountSharedData,
    vote_state: &VoteState,
    point_value: &PointValue,
    stake_history: &StakeHistory,
    inflation_point_calc_tracer: Option<impl Fn(&InflationPointCalculationEvent)>,
    new_rate_activation_epoch: Option<Epoch>,
) -> Result<(u64, u64), InstructionError> {
    if let StakeStateV2::Stake(meta, mut stake, stake_flags) = stake_state {
        if let Some(inflation_point_calc_tracer) = inflation_point_calc_tracer.as_ref() {
            inflation_point_calc_tracer(
                &InflationPointCalculationEvent::EffectiveStakeAtRewardedEpoch(stake.stake(
                    rewarded_epoch,
                    stake_history,
                    new_rate_activation_epoch,
                )),
            );
            inflation_point_calc_tracer(&InflationPointCalculationEvent::RentExemptReserve(
                meta.rent_exempt_reserve,
            ));
            inflation_point_calc_tracer(&InflationPointCalculationEvent::Commission(
                vote_state.commission,
            ));
        }

        if let Some((stakers_reward, voters_reward)) = redeem_stake_rewards(
            rewarded_epoch,
            &mut stake,
            point_value,
            vote_state,
            stake_history,
            inflation_point_calc_tracer,
            new_rate_activation_epoch,
        ) {
            stake_account.checked_add_lamports(stakers_reward)?;
            stake_account.set_state(&StakeStateV2::Stake(meta, stake, stake_flags))?;

            Ok((stakers_reward, voters_reward))
        } else {
            Err(StakeError::NoCreditsToRedeem.into())
        }
    } else {
        Err(InstructionError::InvalidAccountData)
    }
}

fn redeem_stake_rewards(
    rewarded_epoch: Epoch,
    stake: &mut Stake,
    point_value: &PointValue,
    vote_state: &VoteState,
    stake_history: &StakeHistory,
    inflation_point_calc_tracer: Option<impl Fn(&InflationPointCalculationEvent)>,
    new_rate_activation_epoch: Option<Epoch>,
) -> Option<(u64, u64)> {
    if let Some(inflation_point_calc_tracer) = inflation_point_calc_tracer.as_ref() {
        inflation_point_calc_tracer(&InflationPointCalculationEvent::CreditsObserved(
            stake.credits_observed,
            None,
        ));
    }
    calculate_stake_rewards(
        rewarded_epoch,
        stake,
        point_value,
        vote_state,
        stake_history,
        inflation_point_calc_tracer.as_ref(),
        new_rate_activation_epoch,
    )
    .map(|calculated_stake_rewards| {
        if let Some(inflation_point_calc_tracer) = inflation_point_calc_tracer {
            inflation_point_calc_tracer(&InflationPointCalculationEvent::CreditsObserved(
                stake.credits_observed,
                Some(calculated_stake_rewards.new_credits_observed),
            ));
        }
        stake.credits_observed = calculated_stake_rewards.new_credits_observed;
        stake.delegation.stake += calculated_stake_rewards.staker_rewards;
        (
            calculated_stake_rewards.staker_rewards,
            calculated_stake_rewards.voter_rewards,
        )
    })
}

/// for a given stake and vote_state, calculate what distributions and what updates should be made
/// returns a tuple in the case of a payout of:
///   * staker_rewards to be distributed
///   * voter_rewards to be distributed
///   * new value for credits_observed in the stake
///
/// returns None if there's no payout or if any deserved payout is < 1 lamport
fn calculate_stake_rewards(
    rewarded_epoch: Epoch,
    stake: &Stake,
    point_value: &PointValue,
    vote_state: &VoteState,
    stake_history: &StakeHistory,
    inflation_point_calc_tracer: Option<impl Fn(&InflationPointCalculationEvent)>,
    new_rate_activation_epoch: Option<Epoch>,
) -> Option<CalculatedStakeRewards> {
    // ensure to run to trigger (optional) inflation_point_calc_tracer
    let CalculatedStakePoints {
        points,
        new_credits_observed,
        mut force_credits_update_with_skipped_reward,
    } = calculate_stake_points_and_credits(
        stake,
        vote_state,
        stake_history,
        inflation_point_calc_tracer.as_ref(),
        new_rate_activation_epoch,
    );

    // Drive credits_observed forward unconditionally when rewards are disabled
    // or when this is the stake's activation epoch
    if point_value.rewards == 0 {
        if let Some(inflation_point_calc_tracer) = inflation_point_calc_tracer.as_ref() {
            inflation_point_calc_tracer(&SkippedReason::DisabledInflation.into());
        }
        force_credits_update_with_skipped_reward = true;
    } else if stake.delegation.activation_epoch == rewarded_epoch {
        // not assert!()-ed; but points should be zero
        if let Some(inflation_point_calc_tracer) = inflation_point_calc_tracer.as_ref() {
            inflation_point_calc_tracer(&SkippedReason::JustActivated.into());
        }
        force_credits_update_with_skipped_reward = true;
    }

    if force_credits_update_with_skipped_reward {
        return Some(CalculatedStakeRewards {
            staker_rewards: 0,
            voter_rewards: 0,
            new_credits_observed,
        });
    }

    if points == 0 {
        if let Some(inflation_point_calc_tracer) = inflation_point_calc_tracer.as_ref() {
            inflation_point_calc_tracer(&SkippedReason::ZeroPoints.into());
        }
        return None;
    }
    if point_value.points == 0 {
        if let Some(inflation_point_calc_tracer) = inflation_point_calc_tracer.as_ref() {
            inflation_point_calc_tracer(&SkippedReason::ZeroPointValue.into());
        }
        return None;
    }

    // The final unwrap is safe, as points_value.points is guaranteed to be non zero above.
    let rewards = points
        .checked_mul(u128::from(point_value.rewards))
        .expect("Rewards intermediate calculation should fit within u128")
        .checked_div(point_value.points)
        .unwrap();

    let rewards = u64::try_from(rewards).expect("Rewards should fit within u64");

    // don't bother trying to split if fractional lamports got truncated
    if rewards == 0 {
        if let Some(inflation_point_calc_tracer) = inflation_point_calc_tracer.as_ref() {
            inflation_point_calc_tracer(&SkippedReason::ZeroReward.into());
        }
        return None;
    }
    #[allow(deprecated)]
    let (voter_rewards, staker_rewards, is_split) =
        commission_split(vote_state.commission, rewards);
    if let Some(inflation_point_calc_tracer) = inflation_point_calc_tracer.as_ref() {
        inflation_point_calc_tracer(&InflationPointCalculationEvent::SplitRewards(
            rewards,
            voter_rewards,
            staker_rewards,
            (*point_value).clone(),
        ));
    }

    if (voter_rewards == 0 || staker_rewards == 0) && is_split {
        // don't collect if we lose a whole lamport somewhere
        //  is_split means there should be tokens on both sides,
        //  uncool to move credits_observed if one side didn't get paid
        if let Some(inflation_point_calc_tracer) = inflation_point_calc_tracer.as_ref() {
            inflation_point_calc_tracer(&SkippedReason::TooEarlyUnfairSplit.into());
        }
        return None;
    }

    Some(CalculatedStakeRewards {
        staker_rewards,
        voter_rewards,
        new_credits_observed,
    })
}

// VoteStateV3 dropped commission_split in vote-interface 3.0; this is agave 3.0.14's free-function form, arithmetic unchanged.
fn commission_split(commission: u8, on: u64) -> (u64, u64, bool) {
    match commission.min(100) {
        0 => (0, on, false),
        100 => (on, 0, false),
        split => {
            let on = u128::from(on);
            // Calculate mine and theirs independently and symmetrically instead of
            // using the remainder of the other to treat them strictly equally.
            // This is also to cancel the rewarding if either of the parties
            // should receive only fractional lamports, resulting in not being rewarded at all.
            // Thus, note that we intentionally discard any residual fractional lamports.
            let mine = on
                .checked_mul(u128::from(split))
                .expect("multiplication of a u64 and u8 should not overflow")
                / 100u128;
            let theirs = on
                .checked_mul(u128::from(
                    100u8
                        .checked_sub(split)
                        .expect("commission cannot be greater than 100"),
                ))
                .expect("multiplication of a u64 and u8 should not overflow")
                / 100u128;

            (mine as u64, theirs as u64, true)
        }
    }
}
