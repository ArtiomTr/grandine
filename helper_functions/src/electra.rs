use anyhow::{Result, ensure};
use arithmetic::{U64Ext as _, UsizeExt as _};
use ssz::ContiguousList;
use try_from_iterator::TryFromIterator as _;
use typenum::Unsigned as _;
use types::{
    altair::consts::{PROPOSER_WEIGHT, WEIGHT_DENOMINATOR},
    config::Config,
    electra::containers::{Attestation, IndexedAttestation},
    nonstandard::{PartialValidator, SlashingKind},
    phase0::{
        consts::FAR_FUTURE_EPOCH,
        primitives::{Epoch, Gwei, ValidatorIndex},
    },
    preset::Preset,
    traits::{BeaconState, PostElectraAttestation, PostElectraBeaconState},
};

use crate::{
    accessors::{beacon_committee, get_beacon_proposer_index, get_current_epoch},
    error::Error,
    misc::{get_committee_indices, get_max_effective_balance},
    mutators::{balance, compute_exit_epoch_and_update_churn, decrease_balance, increase_balance},
    predicates::has_execution_withdrawal_credential,
    slot_report::SlotReport,
};

// > Check if ``validator`` is eligible to be placed into the activation queue.
#[must_use]
pub const fn is_eligible_for_activation_queue<P: Preset>(
    validator: &PartialValidator,
    effective_balance: Gwei,
) -> bool {
    validator.activation_eligibility_epoch == FAR_FUTURE_EPOCH
        && effective_balance >= P::MIN_ACTIVATION_BALANCE
}

// > Check if ``validator`` is fully withdrawable.
#[must_use]
pub fn is_fully_withdrawable_validator(
    validator: &PartialValidator,
    balance: Gwei,
    epoch: Epoch,
) -> bool {
    has_execution_withdrawal_credential(validator)
        && validator.withdrawable_epoch <= epoch
        && balance > 0
}

// > Check if ``validator`` is partially withdrawable.
#[must_use]
pub fn is_partially_withdrawable_validator<P: Preset>(
    validator: &PartialValidator,
    effective_balance: Gwei,
    balance: Gwei,
) -> bool {
    let max_effective_balance = get_max_effective_balance::<P>(validator);
    let has_max_effective_balance = effective_balance == max_effective_balance;
    let has_excess_balance = balance > max_effective_balance;

    has_execution_withdrawal_credential(validator)
        && has_max_effective_balance
        && has_excess_balance
}

pub fn get_indexed_attestation<P: Preset>(
    state: &impl BeaconState<P>,
    attestation: &Attestation<P>,
) -> Result<IndexedAttestation<P>> {
    // `get_attesting_indices` already returns the indices sorted, which is the order
    // `IndexedAttestation` requires.
    let attesting_indices =
        ContiguousList::try_from_iter(get_attesting_indices(state, attestation)?).expect(
            "Attestation.aggregation_bits and IndexedAttestation.attesting_indices \
         have the same maximum length",
        );

    Ok(IndexedAttestation {
        attesting_indices,
        data: attestation.data,
        signature: attestation.signature,
    })
}

// > Return the set of attesting indices corresponding to ``aggregation_bits`` and ``committee_bits``.
///
/// The indices come back sorted and without duplicates.
///
/// The committees an attestation covers are disjoint, so sorting the concatenation is all the
/// deduplication a set would have done. Sorted is the order every caller wants:
/// `get_indexed_attestation` needs it to build an `IndexedAttestation`, and the callers that walk
/// the registry, the balances or the participation flags at these indices turn what would be
/// random access into a forward scan.
#[cfg_attr(feature = "tracing", tracing::instrument(level = "debug", skip_all))]
pub fn get_attesting_indices<P: Preset>(
    state: &impl BeaconState<P>,
    attestation: &impl PostElectraAttestation<P>,
) -> Result<Vec<ValidatorIndex>> {
    let mut output = vec![];
    let committee_indices = get_committee_indices::<P>(attestation.committee_bits());
    let mut committee_offset: usize = 0;

    for index in committee_indices {
        let committee = beacon_committee(state, attestation.data().slot, index)?;
        let attesters_before = output.len();

        for (i, index) in committee.into_iter().enumerate() {
            let bit_index = committee_offset.try_add(i)?;

            if attestation
                .aggregation_bits()
                .get_bit(bit_index)
                .is_some_and(|bit| bit)
            {
                output.push(index);
            }
        }

        ensure!(
            output.len() > attesters_before,
            Error::NoCommitteeAttesters { index },
        );

        committee_offset = committee_offset.try_add(committee.len())?;
    }

    // This works the same as `assert len(attestation.aggregation_bits) == committee_offset`
    ensure!(
        committee_offset == attestation.aggregation_bits().len_usize(),
        Error::ParticipantsCountMismatch {
            aggregation_bitlist_length: attestation.aggregation_bits().len_usize(),
            participants_count: committee_offset
        },
    );

    output.sort_unstable();

    Ok(output)
}

// > Initiate the exit of the validator with index ``index``.
pub fn initiate_validator_exit<P: Preset>(
    config: &Config,
    state: &mut impl PostElectraBeaconState<P>,
    validator_index: ValidatorIndex,
) -> Result<()> {
    let validator = state.validators().partial_validator(validator_index)?;

    // > Return if validator already initiated exit
    if validator.exit_epoch != FAR_FUTURE_EPOCH {
        return Ok(());
    }

    // > Compute exit queue epoch
    let effective_balance = state.validators().effective_balance(validator_index)?;
    let exit_queue_epoch = compute_exit_epoch_and_update_churn(config, state, effective_balance)?;

    // > Set validator exit epoch and withdrawable epoch
    let validator = state
        .validators_mut()
        .partial_validator_mut(validator_index)?;

    validator.exit_epoch = exit_queue_epoch;

    validator.withdrawable_epoch = exit_queue_epoch
        .checked_add(config.min_validator_withdrawability_delay)
        .ok_or(Error::EpochOverflow)?;

    Ok(())
}

// > Slash the validator with index ``slashed_index``.
pub fn slash_validator<P: Preset>(
    config: &Config,
    state: &mut impl PostElectraBeaconState<P>,
    slashed_index: ValidatorIndex,
    whistleblower_index: Option<ValidatorIndex>,
    kind: SlashingKind,
    mut slot_report: impl SlotReport,
) -> Result<()> {
    initiate_validator_exit(config, state, slashed_index)?;

    let epoch = get_current_epoch(state);
    let effective_balance = state.validators().effective_balance(slashed_index)?;
    let slashing_penalty = effective_balance / P::MIN_SLASHING_PENALTY_QUOTIENT_ELECTRA;

    let validator = state
        .validators_mut()
        .partial_validator_mut(slashed_index)?;
    validator.slashed = true;
    validator.withdrawable_epoch = validator
        .withdrawable_epoch
        .max(epoch.try_add(P::EpochsPerSlashingsVector::U64)?);

    let s = state.slashings_mut().mod_index_mut(epoch);
    *s = s.try_add(effective_balance)?;

    decrease_balance(balance(state, slashed_index)?, slashing_penalty);

    // > Apply proposer and whistleblower rewards
    let proposer_index = get_beacon_proposer_index(config, state)?;
    let whistleblower_index = whistleblower_index.unwrap_or(proposer_index);
    let whistleblower_reward = effective_balance / P::WHISTLEBLOWER_REWARD_QUOTIENT_ELECTRA;
    let proposer_reward = whistleblower_reward.try_mul(PROPOSER_WEIGHT.get())? / WEIGHT_DENOMINATOR;
    let remaining_reward = whistleblower_reward.try_sub(proposer_reward)?;

    increase_balance(balance(state, proposer_index)?, proposer_reward)?;
    increase_balance(balance(state, whistleblower_index)?, remaining_reward)?;

    slot_report.set_slashing_penalty(slashed_index, slashing_penalty);
    slot_report.add_slashing_reward(kind, proposer_reward);
    slot_report.add_whistleblowing_reward(whistleblower_index, remaining_reward);

    Ok(())
}
