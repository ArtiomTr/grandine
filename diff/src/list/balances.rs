use derivative::Derivative;
use ssz::{BundleSize, ByteList, ContiguousList, PersistentList, Ssz};
use try_from_iterator::TryFromIterator;
use typenum::{U4294967296, Unsigned};

use crate::{Error, Patch, PatchResult};

const fn zigzag(value: i64) -> u64 {
    ((value << 1) ^ (value >> 63)) as u64
}

const fn unzigzag(value: u64) -> i64 {
    ((value >> 1) as i64) ^ -((value & 1) as i64)
}

#[derive(Clone, Ssz, Derivative)]
#[ssz(derive_hash = false)]
#[derivative(Debug(bound = ""))]
pub struct ListBalancesPatch<N: Unsigned> {
    deltas: ByteList<U4294967296>,
    appended: ContiguousList<u64, N>,
}

impl<N: Unsigned, B: BundleSize<u64>> Patch<PersistentList<u64, N, B>> for ListBalancesPatch<N> {
    fn diff(
        base: &PersistentList<u64, N, B>,
        changed: &PersistentList<u64, N, B>,
    ) -> PatchResult<Self> {
        if base.len_usize() > changed.len_usize() {
            return Err(Error::UnsupportedDiff);
        }

        let mut buffer = unsigned_varint::encode::u64_buffer();
        let mut deltas = Vec::new();

        for (&base, &changed) in base.into_iter().zip(changed.into_iter()) {
            let changed = i64::try_from(changed).map_err(|_| Error::InvalidBalanceDelta)?;
            let base = i64::try_from(base).map_err(|_| Error::InvalidBalanceDelta)?;
            let delta = changed
                .checked_sub(base)
                .ok_or(Error::InvalidBalanceDelta)?;
            let encoded = unsigned_varint::encode::u64(zigzag(delta), &mut buffer);

            deltas.extend_from_slice(encoded);
        }

        Ok(ListBalancesPatch {
            deltas: ByteList::try_from(deltas)
                .expect("balance delta bytes should fit in the SSZ byte list"),
            appended: ContiguousList::try_from_iter(
                changed.into_iter().skip(base.len_usize()).copied(),
            )
            .expect("appended balances should fit in the SSZ list"),
        })
    }

    fn apply(self, mut base: PersistentList<u64, N, B>) -> PatchResult<PersistentList<u64, N, B>> {
        let mut ptr = self.deltas.as_bytes();

        for balance in (&mut base).into_iter() {
            let (delta, rem) =
                unsigned_varint::decode::u64(ptr).map_err(|_| Error::InvalidPatchEncoding)?;

            let delta = unzigzag(delta);

            let base_balance = i64::try_from(*balance).map_err(|_| Error::InvalidBalanceDelta)?;
            let patched = base_balance
                .checked_add(delta)
                .ok_or(Error::InvalidBalanceDelta)?;

            *balance = u64::try_from(patched).map_err(|_| Error::InvalidBalanceDelta)?;

            ptr = rem;
        }

        if !ptr.is_empty() {
            return Err(Error::InvalidPatchEncoding);
        }

        base.extend(self.appended)
            .map_err(|_| Error::PatchListLimitExceeded)?;
        Ok(base)
    }
}

// #[cfg(test)]
// mod tests {
//     use core::ops::Mul;

//     use ssz::{MerkleElements, PersistentList, SszHash as _};
//     use try_from_iterator::TryFromIterator as _;
//     use typenum::{Prod, U8, U20, U64, U1024, U4096, Unsigned};

//     use super::ListBalancesPatch;
//     use crate::Patch as _;

//     #[test]
//     fn keeps_unchanged_list() {
//         let before = PersistentList::<U8>::try_from_iter([1, 2, 3]).unwrap();
//         let after = PersistentList::<U8>::try_from_iter([1, 2, 3]).unwrap();
//         let patch = ListBalancesPatch::diff(&before, &after).unwrap();
//         let actual = patch.apply(before).unwrap();

//         assert_eq!(actual, after);
//         assert_eq!(actual.hash_tree_root(), after.hash_tree_root());
//     }

//     #[test]
//     fn appends_to_empty_list() {
//         let before = PersistentList::<U8>::try_from_iter([]).unwrap();
//         let after = PersistentList::<U8>::try_from_iter([1, 2, 3]).unwrap();
//         let patch = ListBalancesPatch::diff(&before, &after).unwrap();
//         let actual = patch.apply(before).unwrap();

//         assert_eq!(actual, after);
//         assert_eq!(actual.hash_tree_root(), after.hash_tree_root());
//     }

//     #[test]
//     fn appends_to_existing_list() {
//         let before = PersistentList::<U8>::try_from_iter([1, 2]).unwrap();
//         let after = PersistentList::<U8>::try_from_iter([1, 2, 3, 4]).unwrap();
//         let patch = ListBalancesPatch::diff(&before, &after).unwrap();
//         let actual = patch.apply(before).unwrap();

//         assert_eq!(actual, after);
//         assert_eq!(actual.hash_tree_root(), after.hash_tree_root());
//     }

//     #[test]
//     fn edits_one_balance() {
//         let before = PersistentList::<U8>::try_from_iter([32, 64, 96]).unwrap();
//         let after = PersistentList::<U8>::try_from_iter([32, 65, 96]).unwrap();
//         let patch = ListBalancesPatch::diff(&before, &after).unwrap();
//         let actual = patch.apply(before).unwrap();

//         assert_eq!(actual, after);
//         assert_eq!(actual.hash_tree_root(), after.hash_tree_root());
//     }

//     #[test]
//     fn edits_first_and_last_balances() {
//         let before = PersistentList::<U8>::try_from_iter([10, 20, 30, 40]).unwrap();
//         let after = PersistentList::<U8>::try_from_iter([9, 20, 30, 42]).unwrap();
//         let patch = ListBalancesPatch::diff(&before, &after).unwrap();
//         let actual = patch.apply(before).unwrap();

//         assert_eq!(actual, after);
//         assert_eq!(actual.hash_tree_root(), after.hash_tree_root());
//     }

//     #[test]
//     fn edits_and_appends() {
//         let before = PersistentList::<U8>::try_from_iter([10, 20, 30]).unwrap();
//         let after = PersistentList::<U8>::try_from_iter([11, 20, 29, 40, 50]).unwrap();
//         let patch = ListBalancesPatch::diff(&before, &after).unwrap();
//         let actual = patch.apply(before).unwrap();

//         assert_eq!(actual, after);
//         assert_eq!(actual.hash_tree_root(), after.hash_tree_root());
//     }

//     #[test]
//     fn handles_sparse_changes() {
//         let before = PersistentList::<U64>::try_from_iter([10; 64]).unwrap();
//         let mut after = PersistentList::<U64>::try_from_iter([10; 64]).unwrap();
//         *after.get_mut(0).unwrap() = 11;
//         *after.get_mut(31).unwrap() = 12;
//         *after.get_mut(63).unwrap() = 13;
//         let patch = ListBalancesPatch::diff(&before, &after).unwrap();
//         let actual = patch.apply(before).unwrap();

//         assert_eq!(actual, after);
//         assert_eq!(actual.hash_tree_root(), after.hash_tree_root());
//     }

//     #[test]
//     fn handles_dense_small_deltas() {
//         let before = PersistentList::<U1024>::try_from_iter([100; 1024]);
//         let after = PersistentList::<U1024>::try_from_iter(
//             (0..1024).map(|i| if i % 2 == 0 { 99 } else { 98 }),
//         );
//         let patch = ListBalancesPatch::diff(&before, &after).unwrap();
//         let actual = patch.apply(before).unwrap();

//         assert_eq!(actual, after);
//         assert_eq!(actual.hash_tree_root(), after.hash_tree_root());
//     }

//     #[test]
//     fn handles_dense_large_delta_classes() {
//         let before = list::<U4096>([1_000_000; 4096]);
//         let after = list::<U4096>((0..4096).map(|i| 1_000_000 - 1_000 - (i % 16) as u64 * 1_000));
//         let patch = ListBalancesPatch::diff(&before, &after).unwrap();

//         assert_eq!(patch.apply(before).unwrap(), after);
//     }

//     #[test]
//     fn handles_mixed_positive_and_negative_deltas() {
//         assert_balance_patch::<U8>([10, 20, 30, 40], [15, 18, 30, 35]);
//     }

//     #[test]
//     fn rejects_shrinking_list() {
//         let before = list::<U8>([1, 2, 3]);
//         let after = list::<U8>([1, 2]);

//         assert!(ListBalancesPatch::diff(&before, &after).is_err());
//     }
// }
