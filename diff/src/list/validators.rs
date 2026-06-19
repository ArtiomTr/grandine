use std::{collections::VecDeque, fmt::Debug};

use derivative::Derivative;
use ssz::{ContiguousList, H256, Size, Ssz, SszRead, SszReadDefault, SszSize, SszWrite};
use try_from_iterator::TryFromIterator;
use typenum::Unsigned;
use types::phase0::{
    containers::Validator,
    primitives::{Epoch, Gwei},
    validator_list::{PartialValidator, ValidatorList},
};

use crate::{Error, Patch, PatchResult};

/// A patching algorithm specialized for [`ValidatorList`].
///
/// [`ValidatorList`] stores validator fields in separate columns so that the
/// frequently changing effective balances do not force cloning the rarely
/// changing remainder, and so that the immutable public keys are shared
/// structurally. This patch mirrors that layout:
///
/// * public keys are never diffed — existing entries can only be appended to,
///   so the common prefix is left untouched and new keys ride along inside the
///   appended [`Validator`]s;
/// * effective balances and the remaining validator fields are diffed
///   independently with positional edits, so a change to one column never
///   produces edits for the other.
#[derive(Clone, Ssz, Derivative)]
#[ssz(derive_hash = false)]
#[derivative(Debug(bound = ""))]
pub struct ValidatorListPatch<N: Unsigned> {
    effective_balance_edits: ContiguousList<PositionalEdit<Gwei, N>, N>,
    partial_edits: ContiguousList<PositionalEdit<PartialValidatorSsz, N>, N>,
    appended: ContiguousList<Validator, N>,
    new_len: u64,
}

/// SSZ representation of [`PartialValidator`] used to serialize positional edits.
///
/// [`PartialValidator`] itself is an internal detail of the `types` crate and
/// has no SSZ encoding; the diff crate owns the on-wire layout of its patches.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Ssz)]
#[ssz(derive_hash = false)]
struct PartialValidatorSsz {
    withdrawal_credentials: H256,
    slashed: bool,
    activation_eligibility_epoch: Epoch,
    activation_epoch: Epoch,
    exit_epoch: Epoch,
    withdrawable_epoch: Epoch,
}

impl From<&PartialValidator> for PartialValidatorSsz {
    fn from(value: &PartialValidator) -> Self {
        Self {
            withdrawal_credentials: value.withdrawal_credentials,
            slashed: value.slashed,
            activation_eligibility_epoch: value.activation_eligibility_epoch,
            activation_epoch: value.activation_epoch,
            exit_epoch: value.exit_epoch,
            withdrawable_epoch: value.withdrawable_epoch,
        }
    }
}

impl From<&PartialValidatorSsz> for PartialValidator {
    fn from(value: &PartialValidatorSsz) -> Self {
        Self {
            withdrawal_credentials: value.withdrawal_credentials,
            slashed: value.slashed,
            activation_eligibility_epoch: value.activation_eligibility_epoch,
            activation_epoch: value.activation_epoch,
            exit_epoch: value.exit_epoch,
            withdrawable_epoch: value.withdrawable_epoch,
        }
    }
}

#[derive(Clone, Ssz, Derivative)]
#[ssz(
    derive_hash = false,
    bound = "T: SszWrite",
    bound_for_read = "T: SszRead<C> + SszWrite"
)]
#[derivative(Debug(bound = "T: Debug"))]
struct PositionalEdit<T, N: Unsigned> {
    index: u64,
    values: ContiguousList<T, N>,
}

/// Diffs one validator column into positional edits over the common prefix.
///
/// Edits separated by at most a small gap of unchanged elements are merged into
/// a single run, trading a few redundant values for one fewer per-edit header.
fn diff_column<T, N>(
    base: impl Iterator<Item = T>,
    changed: impl Iterator<Item = T>,
) -> ContiguousList<PositionalEdit<T, N>, N>
where
    T: Eq + SszSize + SszReadDefault + SszWrite,
    N: Unsigned,
{
    // Per-edit header overhead (index plus length) in bytes; bridging a gap
    // shorter than this many fixed-size elements is cheaper than a fresh run.
    const PER_EDIT_OVERHEAD: usize = 16;

    let threshold = match T::SIZE {
        Size::Fixed { size } if size > 0 => PER_EDIT_OVERHEAD / size,
        _ => 0,
    };

    let mut edits = Vec::new();
    let mut last_edit: Option<(u64, usize, Vec<T>)> = None;
    let mut buffer = VecDeque::new();

    for (i, (base_i, changed_i)) in base.zip(changed).enumerate() {
        if base_i == changed_i {
            if last_edit.is_some() && threshold > 0 {
                if buffer.len() >= threshold {
                    buffer.pop_front();
                }
                buffer.push_back(changed_i);
            }
            continue;
        }

        if last_edit
            .as_ref()
            .is_some_and(|(_, end, _)| i - end - 1 <= threshold)
        {
            let (start, _, mut values) = last_edit
                .take()
                .expect("internal invariant: a recent positional edit must exist here");
            values.extend(buffer.drain(..));
            values.push(changed_i);
            last_edit = Some((start, i, values));
        } else {
            if let Some((start, _, values)) = last_edit.take() {
                edits.push((start, values));
            }
            buffer.clear();

            last_edit = Some((
                u64::try_from(i).expect("list index should fit in u64"),
                i,
                vec![changed_i],
            ));
        }
    }

    if let Some((start, _, values)) = last_edit {
        edits.push((start, values));
    }

    ContiguousList::try_from_iter(edits.into_iter().map(|(index, values)| PositionalEdit {
        index,
        values: ContiguousList::try_from(values)
            .expect("positional edit chunk should fit in the SSZ list"),
    }))
    .expect("positional patch should fit in the SSZ list")
}

/// Flattens positional edits into `(absolute index, value)` pairs in ascending order.
fn flatten_edits<T, N: Unsigned>(
    edits: &ContiguousList<PositionalEdit<T, N>, N>,
) -> impl Iterator<Item = (u64, &T)> {
    edits.iter().flat_map(|edit| {
        edit.values.iter().enumerate().map(|(offset, value)| {
            let offset = u64::try_from(offset).expect("edit offset should fit in u64");
            (edit.index + offset, value)
        })
    })
}

impl<N: Unsigned> Patch<ValidatorList<N>> for ValidatorListPatch<N> {
    fn diff(base: &ValidatorList<N>, changed: &ValidatorList<N>) -> PatchResult<Self> {
        // The validator registry only ever grows, so a shrinking diff is not
        // representable; the public keys we skip diffing would be lost.
        if base.len_usize() > changed.len_usize() {
            return Err(Error::UnsupportedDiff);
        }

        let common_len = base.len_usize();

        Ok(ValidatorListPatch {
            effective_balance_edits: diff_column(
                base.effective_balances().copied(),
                changed.effective_balances().copied(),
            ),
            partial_edits: diff_column(
                base.partial_validators().map(PartialValidatorSsz::from),
                changed.partial_validators().map(PartialValidatorSsz::from),
            ),
            appended: ContiguousList::try_from_iter(changed.into_iter().skip(common_len))
                .expect("appended validators should fit in the SSZ list"),
            new_len: u64::try_from(changed.len_usize()).expect("new list length should fit in u64"),
        })
    }

    fn apply(self, mut base: ValidatorList<N>) -> PatchResult<ValidatorList<N>> {
        let new_len =
            usize::try_from(self.new_len).expect("patch length should fit in usize for comparison");

        if base.len_usize() > new_len {
            return Err(Error::PatchLengthMismatch);
        }

        // Apply each column as a single batch so the Merkle cache is invalidated in
        // one traversal rather than once per index (the dominant cost otherwise).
        base.set_effective_balances(
            flatten_edits(&self.effective_balance_edits).map(|(index, value)| (index, *value)),
        )
        .map_err(|_| Error::PatchIndexOutOfBounds)?;

        base.set_partial_validators(
            flatten_edits(&self.partial_edits)
                .map(|(index, value)| (index, PartialValidator::from(value))),
        )
        .map_err(|_| Error::PatchIndexOutOfBounds)?;

        for validator in self.appended {
            base.push(validator)
                .map_err(|_| Error::PatchListLimitExceeded)?;
        }

        if base.len_usize() != new_len {
            return Err(Error::PatchLengthMismatch);
        }

        Ok(base)
    }
}

#[cfg(test)]
mod tests {
    use bls::PublicKeyBytes;
    use ssz::{H256, SszHash as _};
    use typenum::U8;
    use types::phase0::containers::Validator;

    use super::{ValidatorList, ValidatorListPatch};
    use crate::Patch as _;

    fn validator(seed: u8, effective_balance: u64, exit_epoch: u64) -> Validator {
        Validator {
            pubkey: PublicKeyBytes::repeat_byte(seed),
            withdrawal_credentials: H256::repeat_byte(seed),
            effective_balance,
            slashed: false,
            activation_eligibility_epoch: 0,
            activation_epoch: 0,
            exit_epoch,
            withdrawable_epoch: 0,
        }
    }

    fn list<const SIZE: usize>(validators: [Validator; SIZE]) -> ValidatorList<U8> {
        ValidatorList::try_from(validators).unwrap()
    }

    fn assert_roundtrip(before: ValidatorList<U8>, after: ValidatorList<U8>) {
        let patch = ValidatorListPatch::diff(&before, &after).unwrap();
        let actual = patch.apply(before).unwrap();

        assert_eq!(actual, after);
        assert_eq!(actual.hash_tree_root(), after.hash_tree_root());
    }

    #[test]
    fn no_op_patch() {
        let before = list([validator(1, 32, 0), validator(2, 32, 0)]);
        let after = before.clone();

        assert_roundtrip(before, after);
    }

    #[test]
    fn append_to_empty() {
        let before = list([]);
        let after = list([validator(1, 32, 0), validator(2, 32, 0)]);

        assert_roundtrip(before, after);
    }

    #[test]
    fn append_keeps_existing() {
        let before = list([validator(1, 32, 0), validator(2, 32, 0)]);
        let after = list([validator(1, 32, 0), validator(2, 32, 0), validator(3, 16, 5)]);

        assert_roundtrip(before, after);
    }

    #[test]
    fn edit_effective_balance_only() {
        let before = list([validator(1, 32, 0), validator(2, 32, 0), validator(3, 32, 0)]);
        let after = list([validator(1, 32, 0), validator(2, 24, 0), validator(3, 32, 0)]);

        let patch = ValidatorListPatch::diff(&before, &after).unwrap();

        // Only the balance column should carry an edit.
        assert_eq!(patch.effective_balance_edits.len(), 1);
        assert_eq!(patch.partial_edits.len(), 0);

        assert_roundtrip(before, after);
    }

    #[test]
    fn edit_partial_field_only() {
        let before = list([validator(1, 32, 0), validator(2, 32, 0), validator(3, 32, 0)]);
        let after = list([validator(1, 32, 0), validator(2, 32, 9), validator(3, 32, 0)]);

        let patch = ValidatorListPatch::diff(&before, &after).unwrap();

        // Only the partial-validator column should carry an edit.
        assert_eq!(patch.effective_balance_edits.len(), 0);
        assert_eq!(patch.partial_edits.len(), 1);

        assert_roundtrip(before, after);
    }

    #[test]
    fn edit_both_columns_and_append() {
        let before = list([validator(1, 32, 0), validator(2, 32, 0), validator(3, 32, 0)]);
        let after = list([
            validator(1, 31, 0),
            validator(2, 32, 7),
            validator(3, 32, 0),
            validator(4, 16, 0),
        ]);

        assert_roundtrip(before, after);
    }

    #[test]
    fn rejects_shrinking_list() {
        let before = list([validator(1, 32, 0), validator(2, 32, 0), validator(3, 32, 0)]);
        let after = list([validator(1, 32, 0), validator(2, 32, 0)]);

        assert!(ValidatorListPatch::diff(&before, &after).is_err());
    }

    // Exercises the batched cache invalidation (`set_effective_balances` /
    // `set_partial_validators`) across many tree levels, where a per-index bug would
    // surface as a `hash_tree_root` mismatch even though the values still match.
    #[test]
    fn scattered_edits_over_deep_tree() {
        use try_from_iterator::TryFromIterator as _;
        use typenum::U1024;

        let count = 130;
        let build = |edit: &dyn Fn(usize, &mut Validator)| {
            let mut validators = (0..count)
                .map(|i| validator(i as u8, 32_000_000_000, u64::MAX))
                .collect::<Vec<_>>();
            for (i, v) in validators.iter_mut().enumerate() {
                edit(i, v);
            }
            ValidatorList::<U1024>::try_from_iter(validators).unwrap()
        };

        let before = build(&|_, _| {});
        let after = build(&|i, v| {
            if i % 7 == 0 {
                v.effective_balance -= 1_000_000_000;
            }
            if i % 11 == 0 {
                v.exit_epoch = 4_096;
            }
        });

        let patch = ValidatorListPatch::diff(&before, &after).unwrap();
        let actual = patch.apply(before).unwrap();

        assert_eq!(actual, after);
        assert_eq!(actual.hash_tree_root(), after.hash_tree_root());
    }
}
