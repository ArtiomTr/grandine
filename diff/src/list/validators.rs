use bls::PublicKeyBytes;
use ssz::{ContiguousList, H256, Ssz, SszSize};
use try_from_iterator::TryFromIterator as _;
use types::{
    nonstandard::PartialValidator,
    phase0::{
        containers::Validator,
        primitives::{Epoch, Gwei},
    },
    traits::SszValidatorList,
};

use crate::{
    error::Error,
    list::{
        Unlimited,
        gwei_deltas::GweiDeltas,
        positional::{EditAccumulator, PositionalEdit, PositionalPatch},
    },
    patch::{Patch, PatchConfig},
};

#[derive(Ssz, Debug, Clone)]
#[ssz(derive_hash = false)]
pub struct ValidatorListPatch {
    base_len: u32,
    effective_balances: GweiDeltas,
    withdrawal_credentials_edits: ContiguousList<PositionalEdit<H256>, Unlimited>,
    other_edits: ContiguousList<PositionalEdit<OtherValidatorEdits>, Unlimited>,
    appended: ContiguousList<AppendedValidator, Unlimited>,
}

#[derive(Debug, Clone, Ssz)]
#[ssz(derive_hash = false)]
struct AppendedValidator {
    withdrawal_credentials: H256,
    effective_balance: Gwei,
    slashed: bool,
    activation_eligibility_epoch: Epoch,
    activation_epoch: Epoch,
    exit_epoch: Epoch,
    withdrawable_epoch: Epoch,
}

impl AppendedValidator {
    const fn new(partial_validator: &PartialValidator, effective_balance: Gwei) -> Self {
        let PartialValidator {
            withdrawal_credentials,
            slashed,
            activation_eligibility_epoch,
            activation_epoch,
            exit_epoch,
            withdrawable_epoch,
        } = *partial_validator;

        Self {
            withdrawal_credentials,
            effective_balance,
            slashed,
            activation_eligibility_epoch,
            activation_epoch,
            exit_epoch,
            withdrawable_epoch,
        }
    }

    /// Rebuilds the validator with a zero pubkey.
    ///
    /// Appended validators carry no pubkey in the patch - they are restored from the finalized
    /// validator list by the caller, so a state fresh out of `apply` has zero pubkeys at the end
    /// of its registry and a `hash_tree_root` that does not match the state the patch came from.
    const fn into_validator(self) -> Validator {
        let Self {
            withdrawal_credentials,
            effective_balance,
            slashed,
            activation_eligibility_epoch,
            activation_epoch,
            exit_epoch,
            withdrawable_epoch,
        } = self;

        Validator {
            pubkey: PublicKeyBytes::zero(),
            withdrawal_credentials,
            effective_balance,
            slashed,
            activation_eligibility_epoch,
            activation_epoch,
            exit_epoch,
            withdrawable_epoch,
        }
    }
}

#[derive(PartialEq, Eq, Debug, Clone, Copy, Ssz)]
#[ssz(derive_hash = false)]
struct OtherValidatorEdits {
    slashed: bool,
    activation_eligibility_epoch: Epoch,
    activation_epoch: Epoch,
    exit_epoch: Epoch,
    withdrawable_epoch: Epoch,
}

impl From<&PartialValidator> for OtherValidatorEdits {
    fn from(partial_validator: &PartialValidator) -> Self {
        let PartialValidator {
            slashed,
            activation_eligibility_epoch,
            activation_epoch,
            exit_epoch,
            withdrawable_epoch,
            ..
        } = *partial_validator;

        Self {
            slashed,
            activation_eligibility_epoch,
            activation_epoch,
            exit_epoch,
            withdrawable_epoch,
        }
    }
}

impl<C: SszValidatorList + ?Sized> Patch<C> for ValidatorListPatch {
    fn diff(_config: PatchConfig, base: &C, changed: &C) -> Result<Self, Error> {
        let common_len = base.len_usize();

        // Validators are never removed from the registry and `ValidatorList` has
        // no way to shrink, so a shorter `changed` cannot be represented.
        if changed.len_usize() < common_len {
            return Err(Error::UnsupportedDiff);
        }

        let effective_balances = GweiDeltas::diff(common_len, || {
            base.effective_balances()
                .zip(changed.effective_balances())
                .map(|(&before, &after)| (before, after))
        })?;

        let mut withdrawal_credentials = EditAccumulator::new();
        let mut others = EditAccumulator::new();

        for (index, (base_item, changed_item)) in base
            .partial_validators()
            .zip(changed.partial_validators())
            .enumerate()
        {
            // Between two states almost every validator is untouched, and one comparison of the
            // whole item settles both accumulators. Only the ones that differ pay for the
            // per-field comparison and for materialising the patch structs.
            let unchanged = base_item == changed_item;

            withdrawal_credentials.push_lazy(
                index,
                unchanged
                    || base_item.withdrawal_credentials == changed_item.withdrawal_credentials,
                || changed_item.withdrawal_credentials,
            );

            others.push_lazy(
                index,
                unchanged
                    || OtherValidatorEdits::from(base_item)
                        == OtherValidatorEdits::from(changed_item),
                || OtherValidatorEdits::from(changed_item),
            );
        }

        Ok(Self {
            base_len: u32::try_from(common_len).map_err(|_| Error::PatchListLimitExceeded)?,
            effective_balances,
            withdrawal_credentials_edits: withdrawal_credentials.finish(),
            other_edits: others.finish(),
            appended: ContiguousList::try_from_iter(
                changed
                    .partial_validators()
                    .skip(common_len)
                    .zip(changed.effective_balances().skip(common_len))
                    .map(|(partial_validator, &effective_balance)| {
                        AppendedValidator::new(partial_validator, effective_balance)
                    }),
            )
            .expect("validator list patch appended items should fit in the SSZ list"),
        })
    }

    fn apply(self, base: &mut C) -> Result<(), Error> {
        let Self {
            base_len,
            effective_balances,
            withdrawal_credentials_edits,
            other_edits,
            appended,
        } = self;

        if base.len_usize() != usize::try_from(base_len).map_err(|_| Error::InvalidPatchEncoding)? {
            return Err(Error::PatchBaseLengthMismatch);
        }

        let mut failure = None;

        let balances = effective_balances.into_index_ops()?;
        let balance_indices = ascending_indices(&balances)?;

        base.edit_effective_balances(&balance_indices, &mut |ordinal, effective_balance| {
            if failure.is_some() {
                return;
            }

            let (_, op) = balances[ordinal];

            match op.apply(*effective_balance) {
                Ok(patched) => *effective_balance = patched,
                Err(error) => failure = Some(error),
            }
        })
        .map_err(|_| Error::PatchIndexOutOfBounds)?;

        if let Some(error) = failure {
            return Err(error);
        }

        // The two streams cover the same column, so they are merged: a validator that both streams
        // touch is then looked up, and has its cached hash invalidated, once instead of twice.
        let edits = merge_edits(
            flatten(withdrawal_credentials_edits)?,
            flatten(other_edits)?,
        )?;

        let edit_indices = ascending_indices(&edits)?;

        base.edit_partial_validators(&edit_indices, &mut |ordinal, partial_validator| {
            let (_, withdrawal_credentials, other) = &edits[ordinal];

            if let Some(withdrawal_credentials) = withdrawal_credentials {
                partial_validator.withdrawal_credentials = *withdrawal_credentials;
            }

            if let Some(other) = other {
                let OtherValidatorEdits {
                    slashed,
                    activation_eligibility_epoch,
                    activation_epoch,
                    exit_epoch,
                    withdrawable_epoch,
                } = *other;

                partial_validator.slashed = slashed;
                partial_validator.activation_eligibility_epoch = activation_eligibility_epoch;
                partial_validator.activation_epoch = activation_epoch;
                partial_validator.exit_epoch = exit_epoch;
                partial_validator.withdrawable_epoch = withdrawable_epoch;
            }
        })
        .map_err(|_| Error::PatchIndexOutOfBounds)?;

        for validator in appended {
            base.push(validator.into_validator())
                .map_err(|_| Error::PatchListLimitExceeded)?;
        }

        Ok(())
    }
}

/// Expands positional edit runs into one `(index, value)` pair per position.
fn flatten<T: Clone + Eq + SszSize>(
    edits: ContiguousList<PositionalEdit<T>, Unlimited>,
) -> Result<Vec<(u64, T)>, Error> {
    let mut flattened = Vec::new();

    PositionalPatch::apply_edits(edits, |index, value| {
        flattened.push((index, value));
        Ok(())
    })?;

    Ok(flattened)
}

/// The indices of `edits`, which must already be in strictly ascending order.
///
/// A patch built by `diff` always is. One that is not is rejected rather than applied out of
/// order: the batched invalidation behind `edit_effective_balances` and `edit_partial_validators`
/// needs sorted indices, and an unsorted batch would leave stale cached hashes behind instead of
/// failing.
fn ascending_indices<T: Indexed>(edits: &[T]) -> Result<Vec<u64>, Error> {
    if edits
        .windows(2)
        .any(|pair| pair[0].index() >= pair[1].index())
    {
        return Err(Error::InvalidPatchEncoding);
    }

    Ok(edits.iter().map(Indexed::index).collect())
}

trait Indexed {
    fn index(&self) -> u64;
}

impl<T> Indexed for (u64, T) {
    fn index(&self) -> u64 {
        self.0
    }
}

impl<T, U> Indexed for (u64, T, U) {
    fn index(&self) -> u64 {
        self.0
    }
}

/// Merges the two streams of edits to the partial validator column into one edit per validator, in
/// index order.
fn merge_edits(
    withdrawal_credentials: Vec<(u64, H256)>,
    others: Vec<(u64, OtherValidatorEdits)>,
) -> Result<Vec<(u64, Option<H256>, Option<OtherValidatorEdits>)>, Error> {
    let mut withdrawal_credentials = withdrawal_credentials.into_iter().peekable();
    let mut others = others.into_iter().peekable();

    let mut merged = Vec::new();

    loop {
        let next = [
            withdrawal_credentials.peek().map(|(index, _)| *index),
            others.peek().map(|(index, _)| *index),
        ]
        .into_iter()
        .flatten()
        .min();

        let Some(index) = next else { break };

        let credentials = withdrawal_credentials
            .next_if(|(candidate, _)| *candidate == index)
            .map(|(_, value)| value);

        let other = others
            .next_if(|(candidate, _)| *candidate == index)
            .map(|(_, value)| value);

        merged.push((index, credentials, other));
    }

    Ok(merged)
}

#[cfg(test)]
mod tests {
    use ssz::{SszHash as _, SszRead as _, SszWrite as _};
    use types::{Validators, config::Config, preset::Minimal};

    use super::*;

    fn validator(index: u64) -> Validator {
        Validator {
            withdrawal_credentials: H256::from_low_u64_be(index),
            effective_balance: 32_000_000_000_u64.saturating_add(index),
            slashed: index.is_multiple_of(3),
            activation_eligibility_epoch: index,
            activation_epoch: index.saturating_add(1),
            exit_epoch: Epoch::MAX,
            withdrawable_epoch: Epoch::MAX,
            ..Validator::default()
        }
    }

    fn validators(count: u64) -> Validators<Minimal> {
        Validators::<Minimal>::try_from_iter((0..count).map(validator)).expect("list is not full")
    }

    fn round_trip(base: &Validators<Minimal>, changed: &Validators<Minimal>) {
        let patch = <ValidatorListPatch as Patch<Validators<Minimal>>>::diff(
            PatchConfig::default(),
            base,
            changed,
        )
        .expect("patch should represent the change");

        let encoded = patch.to_ssz().expect("patch should serialize");

        let patch = ValidatorListPatch::from_ssz(&Config::minimal(), encoded)
            .expect("patch should deserialize");

        let mut applied = base.clone();

        Patch::apply(patch, &mut applied).expect("patch should apply");

        assert_eq!(applied, *changed);
        assert_eq!(applied.hash_tree_root(), changed.hash_tree_root());
    }

    #[test]
    fn edits_at_scattered_indices_round_trip() {
        let base = validators(30);
        let mut changed = base.clone();

        // Only an effective balance.
        *changed
            .effective_balance_mut(1)
            .expect("index is within bounds") = 16_000_000_000;

        // Only withdrawal credentials.
        changed
            .partial_validator_mut(7)
            .expect("index is within bounds")
            .withdrawal_credentials = H256::repeat_byte(0xff);

        // Only the fields carried by `OtherValidatorEdits`.
        let partial = changed
            .partial_validator_mut(13)
            .expect("index is within bounds");

        partial.slashed = true;
        partial.exit_epoch = 99;
        partial.withdrawable_epoch = 128;

        // Every accumulator at once, at the last index.
        *changed
            .effective_balance_mut(29)
            .expect("index is within bounds") = 1;

        let partial = changed
            .partial_validator_mut(29)
            .expect("index is within bounds");

        partial.withdrawal_credentials = H256::repeat_byte(0xee);
        partial.activation_epoch = 5;

        round_trip(&base, &changed);
    }

    #[test]
    fn appended_validators_round_trip() {
        let base = validators(4);
        let mut changed = base.clone();

        for index in 4..9 {
            changed.push(validator(index)).expect("list is not full");
        }

        // An edit to an existing validator alongside the appended ones.
        *changed
            .effective_balance_mut(0)
            .expect("index is within bounds") = 7;

        round_trip(&base, &changed);
    }

    #[test]
    fn an_unchanged_list_round_trips() {
        let base = validators(6);

        round_trip(&base, &base.clone());
    }

    #[test]
    fn growing_from_an_empty_list_round_trips() {
        round_trip(&validators(0), &validators(3));
    }

    #[test]
    fn a_shorter_list_is_rejected() {
        let base = validators(4);
        let changed = validators(3);

        let error = <ValidatorListPatch as Patch<Validators<Minimal>>>::diff(
            PatchConfig::default(),
            &base,
            &changed,
        )
        .expect_err("the validator registry cannot shrink");

        assert!(matches!(error, Error::UnsupportedDiff));
    }

    #[test]
    fn a_base_of_the_wrong_length_is_rejected() {
        let base = validators(4);
        let mut changed = base.clone();

        *changed
            .effective_balance_mut(3)
            .expect("index is within bounds") = 1;

        let patch = <ValidatorListPatch as Patch<Validators<Minimal>>>::diff(
            PatchConfig::default(),
            &base,
            &changed,
        )
        .expect("patch should represent the change");

        let mut shorter = validators(2);

        let error = Patch::apply(patch, &mut shorter)
            .expect_err("the patch was diffed against a four-validator list");

        assert!(matches!(error, Error::PatchBaseLengthMismatch));
    }
}
