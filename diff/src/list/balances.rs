use core::cmp::Reverse;
use std::collections::HashMap;

use ssz::{ByteList, ContiguousList, Ssz, SszList, SszListMut};
use try_from_iterator::TryFromIterator;
use typenum::U4294967296;
use types::phase0::primitives::Gwei;

use crate::{
    error::Error,
    list::{Unlimited, position_set::PositionSet},
    patch::{Patch, PatchConfig},
};

#[derive(Ssz, Debug, Clone)]
#[ssz(derive_hash = false)]
pub struct BalancesPatch {
    /// Length of the base this patch was computed against. A patch only describes the base it was
    /// diffed from, so applying it to a list of any other length would silently produce a wrong
    /// result: the edit positions are absolute and the appended tail assumes the base ends where
    /// it did at diff time.
    base_len: u32,
    mode: Gwei,
    positions: PositionSet,
    deltas: ByteList<U4294967296>,
    appended: ContiguousList<Gwei, Unlimited>,
}

impl BalancesPatch {
    fn estimate_mode<C: SszList<Gwei> + ?Sized>(base: &C, changed: &C) -> Result<Gwei, Error> {
        let mut counts = HashMap::new();

        for (&before, &after) in base.iter().zip(changed.iter()) {
            if before == after || after == 0 {
                continue;
            }

            let after = i64::try_from(after).map_err(|_| Error::InvalidBalanceDelta)?;
            let before = i64::try_from(before).map_err(|_| Error::InvalidBalanceDelta)?;
            let delta = after.checked_sub(before).ok_or(Error::InvalidBalanceDelta)?;

            let count = counts.entry(zigzag(delta)).or_insert(0_usize);
            *count = count.saturating_add(1);
        }

        Ok(counts
            .into_iter()
            // Ties are broken by the smaller zigzagged delta to keep diffs deterministic.
            .max_by_key(|&(delta, count)| (count, Reverse(delta)))
            .map_or(0, |(delta, _)| delta))
    }
}

impl<C: SszListMut<Gwei> + ?Sized> Patch<C> for BalancesPatch {
    fn diff(_config: PatchConfig, base: &C, changed: &C) -> Result<Self, Error> {
        if base.len_usize() > changed.len_usize() {
            return Err(Error::UnsupportedDiff);
        }

        let mut positions = PositionSet::builder(base.len_usize());
        let mode = Self::estimate_mode(base, changed)?;
        let mode_delta = unzigzag(mode);

        let mut buffer = unsigned_varint::encode::u64_buffer();
        let mut deltas = Vec::new();

        for (index, (&before, &after)) in base.iter().zip(changed.iter()).enumerate() {
            if before == after {
                continue;
            }

            let after = i64::try_from(after).map_err(|_| Error::InvalidBalanceDelta)?;
            let before = i64::try_from(before).map_err(|_| Error::InvalidBalanceDelta)?;

            let encoded = if after == 0 {
                0
            } else {
                let delta = after
                    .checked_sub(before)
                    .ok_or(Error::InvalidBalanceDelta)?
                    .checked_sub(mode_delta)
                    .ok_or(Error::InvalidBalanceDelta)?;

                zigzag(delta)
                    .checked_add(1)
                    .ok_or(Error::InvalidBalanceDelta)?
            };

            deltas.extend_from_slice(unsigned_varint::encode::u64(encoded, &mut buffer));

            positions.record(index);
        }

        Ok(Self {
            base_len: u32::try_from(base.len_usize()).map_err(|_| Error::PatchListLimitExceeded)?,
            mode,
            positions: positions.finish(),
            deltas: ByteList::try_from(deltas)
                .expect("balance delta bytes should fit in the SSZ byte list"),
            appended: ContiguousList::try_from_iter(changed.iter().skip(base.len_usize()).copied())
                .expect("appended balances should fit in the SSZ list"),
        })
    }

    fn apply(self, base: &mut C) -> Result<(), Error> {
        let Self {
            base_len,
            mode,
            positions,
            deltas,
            appended,
        } = self;

        if base.len_usize() != usize::try_from(base_len).map_err(|_| Error::InvalidPatchEncoding)? {
            return Err(Error::PatchBaseLengthMismatch);
        }

        let mode = unzigzag(mode);
        let mut deltas = deltas.as_bytes();

        positions.apply(base, |balance| {
            let delta;
            (delta, deltas) =
                unsigned_varint::decode::u64(deltas).map_err(|_| Error::InvalidPatchEncoding)?;

            match delta {
                // set to zero
                0 => {
                    *balance = 0;
                    Ok(())
                }
                // zigzagged delta
                1.. => {
                    let delta = unzigzag(delta.saturating_sub(1))
                        .checked_add(mode)
                        .ok_or(Error::InvalidBalanceDelta)?;
                    let patched =
                        i64::try_from(*balance).map_err(|_| Error::InvalidBalanceDelta)?;
                    let patched = patched
                        .checked_add(delta)
                        .ok_or(Error::InvalidBalanceDelta)?;

                    u64::try_from(patched)
                        .map(|patched| *balance = patched)
                        .map_err(|_| Error::InvalidBalanceDelta)
                }
            }
        })?;

        if !deltas.is_empty() {
            return Err(Error::InvalidPatchEncoding);
        }

        base.extend(&mut appended.into_iter())
            .map_err(|_| Error::PatchListLimitExceeded)
    }
}

const fn zigzag(value: i64) -> u64 {
    ((value << 1) ^ (value >> 63)).cast_unsigned()
}

const fn unzigzag(value: u64) -> i64 {
    (value >> 1).cast_signed() ^ (value & 1).cast_signed().wrapping_neg()
}

#[cfg(test)]
mod tests {
    use ssz::PersistentList;
    use typenum::U64;

    use super::*;

    #[test]
    fn round_trips_over_persistent_list() {
        type List = PersistentList<Gwei, U64>;

        let base = List::try_from_iter((0..20).map(|index| 32_000_000_000 + index))
            .expect("length is below the maximum");

        for changed in [
            // unchanged
            List::try_from_iter((0..20).map(|index| 32_000_000_000 + index)),
            // the same increase everywhere, which is what the mode is for
            List::try_from_iter((0..20).map(|index| 32_000_100_000 + index)),
            // decreases, zeroing and an appended balance
            List::try_from_iter(
                (0..20)
                    .map(|index| {
                        if index % 3 == 0 {
                            0
                        } else {
                            31_000_000_000 + index
                        }
                    })
                    .chain([32_000_000_000]),
            ),
        ] {
            let changed = changed.expect("length is below the maximum");

            let patch = BalancesPatch::diff(PatchConfig::default(), &base, &changed)
                .expect("balances patch should represent the change");

            let mut applied = base.clone();
            patch.apply(&mut applied).expect("patch should apply");

            assert_eq!(applied, changed);
        }
    }

    #[test]
    fn a_base_of_the_wrong_length_is_rejected() {
        type List = PersistentList<Gwei, U64>;

        let base = List::try_from_iter((0..20).map(|index| 32_000_000_000 + index))
            .expect("length is below the maximum");

        let changed = List::try_from_iter((0..20).map(|index| 32_000_100_000 + index))
            .expect("length is below the maximum");

        let patch = BalancesPatch::diff(PatchConfig::default(), &base, &changed)
            .expect("balances patch should represent the change");

        let mut shorter = List::try_from_iter((0..10).map(|index| 32_000_000_000 + index))
            .expect("length is below the maximum");

        let error = patch
            .apply(&mut shorter)
            .expect_err("the patch was diffed against a twenty-balance list");

        assert!(matches!(error, Error::PatchBaseLengthMismatch));
    }

    #[test]
    fn a_uniform_decrease_is_picked_as_the_mode() {
        type List = PersistentList<Gwei, U64>;

        let base = List::try_from_iter((0..20).map(|index| 32_000_000_000 + index))
            .expect("length is below the maximum");

        let changed = List::try_from_iter((0..20).map(|index| 31_999_900_000 + index))
            .expect("length is below the maximum");

        let patch = BalancesPatch::diff(PatchConfig::default(), &base, &changed)
            .expect("balances patch should represent the change");

        assert_eq!(patch.mode, zigzag(-100_000));

        // Every balance moved by the mode, so every delta encodes as the single byte 1.
        assert_eq!(patch.deltas.as_bytes(), [1; 20]);

        let mut applied = base.clone();
        patch.apply(&mut applied).expect("patch should apply");

        assert_eq!(applied, changed);
    }

    #[test]
    fn zeroed_balances_do_not_contribute_to_the_mode() {
        type List = PersistentList<Gwei, U64>;

        let base = List::try_from_iter([10, 10, 10, 40]).expect("length is below the maximum");
        let changed = List::try_from_iter([0, 0, 0, 47]).expect("length is below the maximum");

        let patch = BalancesPatch::diff(PatchConfig::default(), &base, &changed)
            .expect("balances patch should represent the change");

        // The -10 of the three zeroings is the most repeated delta, but zeroings are skipped,
        // which leaves the lone +7.
        assert_eq!(patch.mode, zigzag(7));

        // The zeroings encode as the set-to-zero opcode, the increase as the mode itself.
        assert_eq!(patch.deltas.as_bytes(), [0, 0, 0, 1]);

        let mut applied = base.clone();
        patch.apply(&mut applied).expect("patch should apply");

        assert_eq!(applied, changed);
    }

    #[test]
    fn round_trips_mixed_increases_and_decreases() {
        type List = PersistentList<Gwei, U64>;

        let base = List::try_from_iter((0..20).map(|index| 32_000_000_000 + index * 1_000))
            .expect("length is below the maximum");

        let changed = List::try_from_iter((0..20).map(|index| {
            if index % 2 == 0 {
                32_000_000_000 + index * 1_000 + 500
            } else {
                32_000_000_000 + index * 1_000 - 700
            }
        }))
        .expect("length is below the maximum");

        let patch = BalancesPatch::diff(PatchConfig::default(), &base, &changed)
            .expect("balances patch should represent the change");

        let mut applied = base.clone();
        patch.apply(&mut applied).expect("patch should apply");

        assert_eq!(applied, changed);
    }

    #[test]
    fn round_trips_a_base_shorter_than_the_changed_list() {
        type List = PersistentList<Gwei, U64>;

        let base = List::try_from_iter((0..10).map(|index| 32_000_000_000 + index))
            .expect("length is below the maximum");

        let changed = List::try_from_iter(
            (0..10)
                .map(|index| 31_999_000_000 + index)
                .chain((0..4).map(|index| 32_000_000_000 + index)),
        )
        .expect("length is below the maximum");

        let patch = BalancesPatch::diff(PatchConfig::default(), &base, &changed)
            .expect("balances patch should represent the change");

        assert_eq!(patch.mode, zigzag(-1_000_000));

        let mut applied = base.clone();
        patch.apply(&mut applied).expect("patch should apply");

        assert_eq!(applied, changed);
    }
}
