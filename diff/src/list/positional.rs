use std::{collections::VecDeque, fmt::Debug};

use derivative::Derivative;
use ssz::{
    BundleSize, ContiguousList, PersistentList, Size, Ssz, SszRead, SszReadDefault, SszSize,
    SszWrite,
};
use try_from_iterator::TryFromIterator;
use typenum::Unsigned;

use crate::{Error, Patch, PatchResult};

/// A positional edit patching algorithm for PersistentList.
#[derive(Clone, Ssz, Derivative)]
#[ssz(
    derive_hash = false,
    bound = "T: SszWrite",
    bound_for_read = "T: SszRead<C> + SszWrite"
)]
#[derivative(Debug(bound = "T: Debug"))]
pub struct ListPositionalPatch<T, N: Unsigned> {
    edits: ContiguousList<PositionalEdit<T, N>, N>,
    appended: ContiguousList<T, N>,
    new_len: u64,
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

const PER_EDIT_OVERHEAD: usize = 16;

const fn merge_gap_threshold<T: SszSize>() -> usize {
    match T::SIZE {
        Size::Fixed { size } if size > 0 => PER_EDIT_OVERHEAD / size,
        _ => 0,
    }
}

impl<T, N, B> Patch<PersistentList<T, N, B>> for ListPositionalPatch<T, N>
where
    T: Clone + Eq + SszReadDefault + SszWrite,
    N: Unsigned,
    B: BundleSize<T>,
{
    fn diff(
        base: &PersistentList<T, N, B>,
        changed: &PersistentList<T, N, B>,
    ) -> PatchResult<Self> {
        let common_len = base.len_usize().min(changed.len_usize());
        let threshold = merge_gap_threshold::<T>();

        let mut accumulated_edits = Vec::new();
        let mut last_edit: Option<(u64, usize, Vec<T>)> = None;
        let mut buffer = VecDeque::new();

        for (i, (base_i, changed_i)) in base.into_iter().zip(changed.into_iter()).enumerate() {
            if base_i == changed_i {
                if last_edit.is_some() && threshold > 0 {
                    if buffer.len() >= threshold {
                        buffer.pop_front();
                    }
                    buffer.push_back(changed_i.clone());
                }
                continue;
            }

            if last_edit
                .as_ref()
                .is_some_and(|(_, end, _)| i - end - 1 <= threshold)
            {
                let (start, _, mut edits) = last_edit
                    .expect("internal invariant: a recent positional edit must exist here");
                let (l, r) = buffer.as_slices();
                edits.extend_from_slice(l);
                edits.extend_from_slice(r);
                buffer.clear();
                edits.push(changed_i.clone());
                last_edit = Some((start, i, edits));
            } else {
                if let Some((start, _, edits)) = last_edit {
                    accumulated_edits.push(PositionalEdit {
                        index: start,
                        values: ContiguousList::try_from_iter(edits)
                            .expect("positional edit chunk should fit in the SSZ list"),
                    });
                }
                buffer.clear();

                last_edit = Some((
                    u64::try_from(i).expect("list index should fit in u64"),
                    i,
                    vec![changed_i.clone()],
                ));
            }
        }

        if let Some((start, _, edits)) = last_edit {
            accumulated_edits.push(PositionalEdit {
                index: start,
                values: ContiguousList::try_from(edits)
                    .expect("positional edit chunk should fit in the SSZ list"),
            });
        };

        Ok(ListPositionalPatch {
            edits: ContiguousList::try_from(accumulated_edits)
                .expect("positional patch should fit in the SSZ list"),
            appended: ContiguousList::try_from_iter(changed.into_iter().skip(common_len).cloned())
                .expect("positional patch appended items should fit in the SSZ list"),
            new_len: u64::try_from(changed.len_usize()).expect("new list length should fit in u64"),
        })
    }

    fn apply(self, mut base: PersistentList<T, N, B>) -> PatchResult<PersistentList<T, N, B>> {
        let new_len =
            usize::try_from(self.new_len).expect("patch length should fit in usize for comparison");

        if base.len_usize() > new_len {
            base = base.slice(..new_len);
        }

        for edit in &self.edits {
            for (i, value) in edit.values.iter().enumerate() {
                let ptr = base
                    .get_mut(edit.index + i as u64)
                    .map_err(|_| Error::PatchIndexOutOfBounds)?;
                *ptr = value.clone();
            }
        }

        base.extend(self.appended.into_iter())
            .map_err(|_| Error::PatchListLimitExceeded)?;

        if base.len_usize() != new_len {
            return Err(Error::PatchLengthMismatch);
        }

        Ok(base)
    }
}

#[cfg(test)]
mod tests {
    use ssz::{PersistentList, SszHash};
    use typenum::{U8, U64};

    use super::ListPositionalPatch;
    use crate::Patch as _;

    #[test]
    fn no_op_patch() {
        let before = PersistentList::<u64, U8>::try_from([1, 2, 3]).unwrap();
        let after = before.clone();

        let patch = ListPositionalPatch::diff(&before, &after).unwrap();

        let actual = patch.apply(before).unwrap();
        assert_eq!(actual, after);
        assert_eq!(actual.hash_tree_root(), after.hash_tree_root());
    }

    #[test]
    fn append_item() {
        let before = PersistentList::<u64, U8>::try_from([1, 2]).unwrap();
        let after = PersistentList::<u64, U8>::try_from([1, 2, 3]).unwrap();

        let patch = ListPositionalPatch::diff(&before, &after).unwrap();
        let actual = patch.apply(before).unwrap();

        assert_eq!(actual, after);
        assert_eq!(actual.hash_tree_root(), after.hash_tree_root())
    }

    #[test]
    fn append_to_empty() {
        let before = PersistentList::<u64, U8>::try_from([]).unwrap();
        let after = PersistentList::<u64, U8>::try_from([1, 2, 3]).unwrap();

        let patch = ListPositionalPatch::diff(&before, &after).unwrap();
        let actual = patch.apply(before).unwrap();

        assert_eq!(actual, after);
        assert_eq!(actual.hash_tree_root(), after.hash_tree_root());
    }

    #[test]
    fn edit_item() {
        let before = PersistentList::<u64, U8>::try_from([1, 2, 3]).unwrap();
        let after = PersistentList::<u64, U8>::try_from([1, 9, 3]).unwrap();

        let patch = ListPositionalPatch::diff(&before, &after).unwrap();
        let actual = patch.apply(before).unwrap();

        assert_eq!(actual, after);
        assert_eq!(actual.hash_tree_root(), after.hash_tree_root());
    }

    #[test]
    fn edit_first_and_last_items() {
        let before = PersistentList::<u64, U8>::try_from([1, 2, 3, 4]).unwrap();
        let after = PersistentList::<u64, U8>::try_from([9, 2, 3, 8]).unwrap();

        let patch = ListPositionalPatch::diff(&before, &after).unwrap();
        let actual = patch.apply(before).unwrap();

        assert_eq!(actual, after);
        assert_eq!(actual.hash_tree_root(), after.hash_tree_root());
    }

    #[test]
    fn delete_item() {
        let before = PersistentList::<u64, U8>::try_from([1, 2, 3]).unwrap();
        let after = PersistentList::<u64, U8>::try_from([1, 2]).unwrap();

        let patch = ListPositionalPatch::diff(&before, &after).unwrap();
        let actual = patch.apply(before).unwrap();

        assert_eq!(actual, after);
        assert_eq!(actual.hash_tree_root(), after.hash_tree_root());
    }

    #[test]
    fn delete_to_empty() {
        let before = PersistentList::<u64, U8>::try_from([1, 2, 3]).unwrap();
        let after = PersistentList::<u64, U8>::try_from([]).unwrap();

        let patch = ListPositionalPatch::diff(&before, &after).unwrap();
        let actual = patch.apply(before).unwrap();

        assert_eq!(actual, after);
        assert_eq!(actual.hash_tree_root(), after.hash_tree_root());
    }

    #[test]
    fn edit_and_append() {
        let before = PersistentList::<u64, U8>::try_from([1, 2, 3]).unwrap();
        let after = PersistentList::<u64, U8>::try_from([1, 9, 3, 4, 5]).unwrap();

        let patch = ListPositionalPatch::diff(&before, &after).unwrap();
        let actual = patch.apply(before).unwrap();

        assert_eq!(actual, after);
        assert_eq!(actual.hash_tree_root(), after.hash_tree_root());
    }

    #[test]
    fn edit_then_truncate() {
        let before = PersistentList::<u64, U8>::try_from([1, 2, 3, 4, 5]).unwrap();
        let after = PersistentList::<u64, U8>::try_from([1, 9, 3]).unwrap();

        let patch = ListPositionalPatch::diff(&before, &after).unwrap();
        let actual = patch.apply(before).unwrap();

        assert_eq!(actual, after);
        assert_eq!(actual.hash_tree_root(), after.hash_tree_root());
    }

    #[test]
    fn collapses_byte_edits_separated_by_small_gap() {
        let before = PersistentList::<u8, U8>::try_from([0, 0, 0, 0, 0, 0, 0]).unwrap();
        let after = PersistentList::<u8, U8>::try_from([1, 0, 0, 0, 0, 0, 2]).unwrap();

        let patch = ListPositionalPatch::diff(&before, &after).unwrap();

        assert_eq!(patch.edits.len(), 1);
        let actual = patch.apply(before).unwrap();
        assert_eq!(actual, after);
        assert_eq!(actual.hash_tree_root(), after.hash_tree_root());
    }

    #[test]
    fn extends_collapsed_byte_edits() {
        let before = PersistentList::<u8, U8>::try_from([0, 0, 0, 0, 0, 0, 0, 0]).unwrap();
        let after = PersistentList::<u8, U8>::try_from([1, 0, 2, 0, 0, 0, 0, 3]).unwrap();

        let patch = ListPositionalPatch::diff(&before, &after).unwrap();

        assert_eq!(patch.edits.len(), 1);
        let actual = patch.apply(before).unwrap();
        assert_eq!(actual, after);
        assert_eq!(actual.hash_tree_root(), after.hash_tree_root());
    }

    #[test]
    fn u64_edits_at_threshold_are_collapsed() {
        let before = PersistentList::<u64, U8>::try_from([0, 0, 0, 0]).unwrap();
        let after = PersistentList::<u64, U8>::try_from([1, 0, 0, 2]).unwrap();

        let patch = ListPositionalPatch::diff(&before, &after).unwrap();

        assert_eq!(patch.edits.len(), 1);
        let actual = patch.apply(before).unwrap();
        assert_eq!(actual, after);
        assert_eq!(actual.hash_tree_root(), after.hash_tree_root());
    }

    #[test]
    fn u64_edits_over_threshold_are_not_collapsed() {
        let before = PersistentList::<u64, U8>::try_from([0, 0, 0, 0, 0]).unwrap();
        let after = PersistentList::<u64, U8>::try_from([1, 0, 0, 0, 2]).unwrap();

        let patch = ListPositionalPatch::diff(&before, &after).unwrap();

        assert_eq!(patch.edits.len(), 2);
        let actual = patch.apply(before).unwrap();
        assert_eq!(actual, after);
        assert_eq!(actual.hash_tree_root(), after.hash_tree_root());
    }

    #[test]
    fn multiple_separated_edit_ranges() {
        let before =
            PersistentList::<u64, U64>::try_from([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]).unwrap();
        let after =
            PersistentList::<u64, U64>::try_from([1, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0]).unwrap();

        let patch = ListPositionalPatch::diff(&before, &after).unwrap();

        assert_eq!(patch.edits.len(), 3);
        let actual = patch.apply(before).unwrap();
        assert_eq!(actual, after);
        assert_eq!(actual.hash_tree_root(), after.hash_tree_root());
    }
}
