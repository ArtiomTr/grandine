use core::fmt::Debug;

use derivative::Derivative;
use ssz::{BundleSize, ContiguousList, PersistentList, Ssz, SszRead, SszReadDefault, SszWrite};
use try_from_iterator::TryFromIterator as _;
use typenum::Unsigned;

use crate::{Error, Patch, PatchResult};

#[derive(Clone, Ssz, Derivative)]
#[ssz(
    derive_hash = false,
    bound = "T: SszWrite",
    bound_for_read = "T: SszRead<C> + SszWrite"
)]
#[derivative(Debug(bound = "T: Debug"))]
pub struct ListQueuePatch<T, N: Unsigned> {
    delete: u64,
    appended: ContiguousList<T, N>,
}

impl<T, N, B> Patch<PersistentList<T, N, B>> for ListQueuePatch<T, N>
where
    T: Clone + Eq + SszReadDefault + SszWrite,
    N: Unsigned,
    B: BundleSize<T>,
{
    fn diff(
        base: &PersistentList<T, N, B>,
        changed: &PersistentList<T, N, B>,
    ) -> PatchResult<Self> {
        let mut base_iter = base.into_iter().peekable();
        let mut changed_iter = changed.into_iter().peekable();
        let mut to_delete = 0;

        loop {
            let Some(base_i) = base_iter.peek() else {
                break;
            };

            let Some(changed_i) = changed_iter.peek() else {
                to_delete = u64::try_from(base.len_usize())
                    .expect("queue patch delete count should fit in u64");
                break;
            };

            if *base_i == *changed_i {
                break;
            }

            to_delete += 1;
            let _ = base_iter.next();
        }

        if changed_iter.peek().is_none() {
            let appended = ContiguousList::try_from_iter(changed_iter.cloned())
                .expect("queue patch appended items should fit in the SSZ list");

            return Ok(Self {
                delete: to_delete,
                appended,
            });
        }

        loop {
            let Some(base_i) = base_iter.peek() else {
                break;
            };

            let Some(changed_i) = changed_iter.peek() else {
                return Err(Error::UnsupportedDiff);
            };

            if *base_i != *changed_i {
                return Err(Error::UnsupportedDiff);
            }

            let _ = base_iter.next();
            let _ = changed_iter.next();
        }

        let appended = ContiguousList::try_from_iter(changed_iter.cloned())
            .expect("queue patch appended items should fit in the SSZ list");

        Ok(Self {
            delete: to_delete,
            appended,
        })
    }

    fn apply(self, base: PersistentList<T, N, B>) -> PatchResult<PersistentList<T, N, B>> {
        if usize::try_from(self.delete).map_or(true, |delete| delete > base.len_usize()) {
            return Err(Error::PatchIndexOutOfBounds);
        }

        let mut new_list = base.slice(self.delete as usize..);
        new_list
            .extend(self.appended)
            .map_err(|_| Error::PatchListLimitExceeded)?;
        Ok(new_list)
    }
}

#[cfg(test)]
mod tests {
    use ssz::{PersistentList, SszHash as _};
    use try_from_iterator::TryFromIterator as _;
    use typenum::U8;

    use super::ListQueuePatch;
    use crate::Patch as _;

    fn list(values: impl IntoIterator<Item = u64>) -> PersistentList<u64, U8> {
        PersistentList::try_from_iter(values).expect("test list should fit in the SSZ list")
    }

    fn assert_queue_patch(
        before: impl IntoIterator<Item = u64>,
        after: impl IntoIterator<Item = u64>,
        expected_delete: u64,
        expected_appended: &[u64],
    ) {
        let before = list(before);
        let after = list(after);

        let patch = ListQueuePatch::diff(&before, &after).unwrap();

        assert_eq!(patch.delete, expected_delete);
        assert_eq!(patch.appended.as_ref(), expected_appended);

        let actual = patch.apply(before.clone()).unwrap();

        assert_eq!(actual, after);
        assert_eq!(actual.hash_tree_root(), after.hash_tree_root());
    }

    #[test]
    fn queue_patch_keeps_unchanged_list() {
        assert_queue_patch([1, 2, 3], [1, 2, 3], 0, &[]);
    }

    #[test]
    fn queue_patch_appends_to_empty_list() {
        assert_queue_patch([], [1, 2, 3], 0, &[1, 2, 3]);
    }

    #[test]
    fn queue_patch_appends_to_existing_tail() {
        assert_queue_patch([1, 2], [1, 2, 3, 4], 0, &[3, 4]);
    }

    #[test]
    fn queue_patch_skips_prefix() {
        assert_queue_patch([1, 2, 3, 4], [3, 4], 2, &[]);
    }

    #[test]
    fn queue_patch_skips_prefix_and_appends() {
        assert_queue_patch([1, 2, 3, 4], [3, 4, 5, 6], 2, &[5, 6]);
    }

    #[test]
    fn queue_patch_deletes_everything() {
        assert_queue_patch([1, 2, 3], [], 3, &[]);
    }

    #[test]
    fn queue_patch_replaces_when_no_suffix_overlaps() {
        assert_queue_patch([1, 2, 3], [4, 5], 3, &[4, 5]);
    }

    #[test]
    fn queue_patch_rejects_shortened_non_prefix_tail() {
        let before = list([1, 2, 3]);
        let after = list([1, 2]);

        assert!(ListQueuePatch::diff(&before, &after).is_err());
    }

    #[test]
    fn queue_patch_rejects_mutated_retained_item() {
        let before = list([1, 2, 3]);
        let after = list([1, 9, 3]);

        assert!(ListQueuePatch::diff(&before, &after).is_err());
    }
}
