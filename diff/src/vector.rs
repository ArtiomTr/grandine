use core::fmt::Debug;

use derivative::Derivative;
use ssz::{
    BundleSize, ContiguousList, IncompletePersistentVector, IncompletePersistentVectorElements,
    PersistentVector, PersistentVectorElements, Ssz, SszRead, SszReadDefault, SszWrite,
};
use try_from_iterator::TryFromIterator;
use typenum::Unsigned;

use crate::{Error, Patch, PatchResult};

#[derive(Clone, Ssz, Derivative)]
#[ssz(
    derive_hash = false,
    bound = "T: SszWrite",
    bound_for_read = "T: SszRead<C> + SszWrite"
)]
#[derivative(Debug(bound = "T: Debug"))]
pub struct VectorEdit<T, N: Unsigned> {
    index: u64,
    values: ContiguousList<T, N>,
}

#[derive(Clone, Ssz, Derivative)]
#[ssz(
    derive_hash = false,
    bound = "T: SszWrite",
    bound_for_read = "T: SszRead<C> + SszWrite"
)]
#[derivative(Debug(bound = "T: Debug"))]
pub struct VectorPatch<T, N: Unsigned> {
    edits: ContiguousList<VectorEdit<T, N>, N>,
}

/// Diff two same-length iterators, and produce edits iterator. Returned edits,
/// when applied on `base` iterator, will produce `changed` iterator.
///
/// This is a helper function for implementing VectorPatch on both
/// PersistentVector and IncompletePersistentVector, avoiding code duplication.
///
/// This function doesn't check if both iterators have equal length - the caller
/// is responsible to ensure that. Otherwise, this function will produce
/// incomplete edit sequence.
fn get_edits<'a, T: Clone + Eq + 'a, N: Unsigned>(
    base: impl IntoIterator<Item = &'a T>,
    changed: impl IntoIterator<Item = &'a T>,
) -> ContiguousList<VectorEdit<T, N>, N> {
    let mut last_edit: Option<(u64, Vec<T>)> = None;
    let mut accumulated = Vec::new();

    for (index, (base, changed)) in base.into_iter().zip(changed.into_iter()).enumerate() {
        if base == changed {
            if let Some((index, items)) = last_edit {
                accumulated.push(VectorEdit {
                    index,
                    values: ContiguousList::try_from(items)
                        .expect("vector edit chunk should fit in the SSZ list"),
                });
            }
            last_edit = None;
            continue;
        }

        if let Some((_, ref mut items)) = last_edit {
            items.push(changed.clone());
        } else {
            last_edit = Some((
                u64::try_from(index).expect("vector index should fit in u64"),
                vec![changed.clone()],
            ))
        }
    }

    if let Some((index, items)) = last_edit {
        accumulated.push(VectorEdit {
            index,
            values: ContiguousList::try_from(items)
                .expect("vector edit chunk should fit in the SSZ list"),
        })
    }

    ContiguousList::try_from(accumulated).expect("vector patch should fit in the SSZ list")
}

fn apply_edits<T, N: Unsigned>(
    patch: VectorPatch<T, N>,
    mut apply_edit: impl FnMut(u64, T) -> PatchResult<()>,
) -> PatchResult<()> {
    for edit in patch.edits {
        for (i, item) in edit.values.into_iter().enumerate() {
            let index = edit.index + u64::try_from(i).expect("edit item index should fit in u64");
            apply_edit(index, item)?;
        }
    }

    Ok(())
}

impl<T, N, B> Patch<PersistentVector<T, N, B>> for VectorPatch<T, N>
where
    T: Clone + Eq + SszReadDefault + SszWrite,
    N: PersistentVectorElements<T, B>,
    B: BundleSize<T>,
{
    fn diff(
        base: &PersistentVector<T, N, B>,
        changed: &PersistentVector<T, N, B>,
    ) -> PatchResult<Self> {
        Ok(VectorPatch {
            edits: ContiguousList::try_from_iter(get_edits::<T, N>(base, changed))
                .expect("vector patch should fit in the SSZ list"),
        })
    }

    fn apply(self, mut base: PersistentVector<T, N, B>) -> PatchResult<PersistentVector<T, N, B>> {
        apply_edits(self, |i, value| {
            *base.get_mut(i).map_err(|_| Error::PatchIndexOutOfBounds)? = value;
            Ok(())
        })?;

        Ok(base)
    }
}

impl<T, N, B> Patch<IncompletePersistentVector<T, N, B>> for VectorPatch<T, N>
where
    T: Clone + Eq + SszReadDefault + SszWrite,
    N: IncompletePersistentVectorElements<T, B>,
    B: BundleSize<T>,
{
    fn diff(
        base: &IncompletePersistentVector<T, N, B>,
        changed: &IncompletePersistentVector<T, N, B>,
    ) -> PatchResult<Self> {
        Ok(VectorPatch {
            edits: ContiguousList::try_from_iter(get_edits::<T, N>(base, changed))
                .expect("vector patch should fit in the SSZ list"),
        })
    }

    fn apply(
        self,
        mut base: IncompletePersistentVector<T, N, B>,
    ) -> PatchResult<IncompletePersistentVector<T, N, B>> {
        apply_edits(self, |i, v| {
            *base.get_mut(i).map_err(|_| Error::PatchIndexOutOfBounds)? = v;
            Ok(())
        })?;

        Ok(base)
    }
}

#[cfg(test)]
mod test {
    use ssz::{IncompletePersistentVector, PersistentVector, SszHash};
    use try_from_iterator::TryFromIterator;
    use typenum::{U4, U5};

    use crate::{Patch, VectorPatch};

    #[test]
    fn edit_persistent_vector_item() {
        let before = PersistentVector::<_, U4>::try_from_iter(vec![1u64, 2, 3, 4]).unwrap();
        let after = PersistentVector::<_, U4>::try_from_iter(vec![1u64, 9, 3, 4]).unwrap();

        let patch = VectorPatch::diff(&before, &after).unwrap();
        let actual = patch.apply(before.clone()).unwrap();

        assert_eq!(actual, after);
        assert_eq!(actual.hash_tree_root(), after.hash_tree_root());
    }

    #[test]
    fn edit_multiple_persistent_vector_items() {
        let before = PersistentVector::<_, U4>::try_from_iter(vec![1u64, 2, 3, 4]).unwrap();
        let after = PersistentVector::<_, U4>::try_from_iter(vec![1u64, 8, 3, 9]).unwrap();

        let patch = VectorPatch::diff(&before, &after).unwrap();
        let actual = patch.apply(before.clone()).unwrap();

        assert_eq!(actual, after);
        assert_eq!(actual.hash_tree_root(), after.hash_tree_root());
    }

    #[test]
    fn edit_incomplete_persistent_vector_item() {
        let before =
            IncompletePersistentVector::<_, U5>::try_from_iter(vec![1u64, 2, 3, 4, 5]).unwrap();
        let after =
            IncompletePersistentVector::<_, U5>::try_from_iter(vec![1u64, 2, 9, 4, 5]).unwrap();

        let patch = VectorPatch::diff(&before, &after).unwrap();
        let actual = patch.apply(before.clone()).unwrap();

        assert_eq!(actual, after);
        assert_eq!(actual.hash_tree_root(), after.hash_tree_root());
    }

    #[test]
    fn edit_incomplete_persistent_vector_tail_item() {
        let before =
            IncompletePersistentVector::<_, U5>::try_from_iter(vec![1u64, 2, 3, 4, 5]).unwrap();
        let after =
            IncompletePersistentVector::<_, U5>::try_from_iter(vec![1u64, 2, 3, 4, 9]).unwrap();

        let patch = VectorPatch::diff(&before, &after).unwrap();
        let actual = patch.apply(before.clone()).unwrap();

        assert_eq!(actual, after);
        assert_eq!(actual.hash_tree_root(), after.hash_tree_root());
    }
}
