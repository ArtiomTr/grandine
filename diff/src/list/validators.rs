use ssz::{ContiguousList, H256, Ssz};
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
        positional::{EditAccumulator, PositionalEdit, PositionalPatch},
    },
    patch::{Patch, PatchConfig},
};

#[derive(Ssz, Debug, Clone)]
#[ssz(derive_hash = false)]
pub struct ValidatorListPatch {
    /// Length of the base this patch was computed against. Applying it to a list of any other
    /// length would silently produce a wrong result, because the edit positions are absolute and
    /// the appended tail assumes the base ends where it did at diff time.
    base_len: u32,
    effective_balance_edits: ContiguousList<PositionalEdit<Gwei>, Unlimited>,
    withdrawal_credentials_edits: ContiguousList<PositionalEdit<H256>, Unlimited>,
    other_edits: ContiguousList<PositionalEdit<OtherValidatorEdits>, Unlimited>,
    appended: ContiguousList<Validator, Unlimited>,
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

        let mut effective_balances = EditAccumulator::new();
        let mut withdrawal_credentials = EditAccumulator::new();
        let mut others = EditAccumulator::new();

        let base_iter = base.partial_validators().zip(base.effective_balances());
        let changed_iter = changed
            .partial_validators()
            .zip(changed.effective_balances());

        for (index, ((base_item, base_balance), (changed_item, changed_balance))) in
            base_iter.zip(changed_iter).enumerate()
        {
            effective_balances.push(index, base_balance, changed_balance);

            withdrawal_credentials.push(
                index,
                &base_item.withdrawal_credentials,
                &changed_item.withdrawal_credentials,
            );

            others.push(
                index,
                &OtherValidatorEdits::from(base_item),
                &OtherValidatorEdits::from(changed_item),
            );
        }

        Ok(Self {
            base_len: u32::try_from(common_len).map_err(|_| Error::PatchListLimitExceeded)?,
            effective_balance_edits: effective_balances.finish(),
            withdrawal_credentials_edits: withdrawal_credentials.finish(),
            other_edits: others.finish(),
            appended: ContiguousList::try_from_iter(changed.iter().skip(common_len))
                .expect("validator list patch appended items should fit in the SSZ list"),
        })
    }

    fn apply(self, base: &mut C) -> Result<(), Error> {
        let Self {
            base_len,
            effective_balance_edits,
            withdrawal_credentials_edits,
            other_edits,
            appended,
        } = self;

        if base.len_usize() != usize::try_from(base_len).map_err(|_| Error::InvalidPatchEncoding)? {
            return Err(Error::PatchBaseLengthMismatch);
        }

        PositionalPatch::apply_edits(effective_balance_edits, |index, effective_balance| {
            let ptr = base
                .effective_balance_mut(index)
                .map_err(|_| Error::PatchIndexOutOfBounds)?;

            *ptr = effective_balance;

            Ok(())
        })?;

        PositionalPatch::apply_edits(
            withdrawal_credentials_edits,
            |index, withdrawal_credentials| {
                let ptr = base
                    .partial_validator_mut(index)
                    .map_err(|_| Error::PatchIndexOutOfBounds)?;

                ptr.withdrawal_credentials = withdrawal_credentials;

                Ok(())
            },
        )?;

        PositionalPatch::apply_edits(other_edits, |index, edits| {
            let ptr = base
                .partial_validator_mut(index)
                .map_err(|_| Error::PatchIndexOutOfBounds)?;

            let OtherValidatorEdits {
                slashed,
                activation_eligibility_epoch,
                activation_epoch,
                exit_epoch,
                withdrawable_epoch,
            } = edits;

            ptr.slashed = slashed;
            ptr.activation_eligibility_epoch = activation_eligibility_epoch;
            ptr.activation_epoch = activation_epoch;
            ptr.exit_epoch = exit_epoch;
            ptr.withdrawable_epoch = withdrawable_epoch;

            Ok(())
        })?;

        for validator in appended {
            base.push(validator)
                .map_err(|_| Error::PatchListLimitExceeded)?;
        }

        Ok(())
    }
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
