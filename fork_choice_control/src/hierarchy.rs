use core::fmt::{self, Display};
use std::str::FromStr;

use anyhow::{Result, ensure};
use helper_functions::misc;
use types::{phase0::primitives::Slot, preset::Preset};

#[derive(Debug, Clone)]
pub struct Hierarchy {
    exponents: Vec<u8>,
}

impl Display for Hierarchy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Some((head, tail)) = self.exponents.split_first() else {
            return Ok(());
        };

        write!(f, "{head}")?;
        for i in tail {
            write!(f, ",{i}")?;
        }

        Ok(())
    }
}

impl FromStr for Hierarchy {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let exponents = s
            .split(',')
            .map(|v| v.parse::<u8>().map_err(Into::into))
            .collect::<Result<Vec<_>>>()?;

        Self::new(exponents)
    }
}

impl Default for Hierarchy {
    fn default() -> Self {
        Hierarchy {
            exponents: vec![0, 5, 9, 15],
        }
    }
}

impl Hierarchy {
    pub fn new(exponents: impl IntoIterator<Item = u8>) -> Result<Self> {
        let exponents = exponents.into_iter().collect::<Vec<_>>();
        ensure!(exponents.is_sorted(), "exponents must be sorted");
        ensure!(
            exponents.windows(2).all(|w| w[0] != w[1]),
            "exponents can't be duplicate"
        );
        ensure!(!exponents.is_empty(), "exponents must not be empty");
        ensure!(
            exponents.iter().all(|&e| e <= 63),
            "max value allowed for exponent is 63"
        );

        Ok(Self { exponents })
    }

    pub fn parent_of<P: Preset>(&self, slot: Slot) -> Option<Slot> {
        // Slots, that aren't part of hierarchy, don't have parents
        if !self.contains::<P>(slot) {
            return None;
        }

        let epoch = misc::compute_epoch_at_slot::<P>(slot);
        let start_of_epoch = misc::compute_start_slot_at_epoch::<P>(epoch);

        if slot != start_of_epoch {
            unreachable!("")
        }

        // Genesis state is always a top-level tree root.
        if epoch == 0 {
            return None;
        }

        let tz = u8::try_from(epoch.trailing_zeros())
            .expect("zero count in epoch number must fit in u8");

        let level_index = self.exponents.iter().rposition(|&e| e <= tz);

        let parent_exp = match level_index {
            Some(i) if i + 1 < self.exponents.len() => self.exponents[i + 1],
            Some(_) => return None, // we're at the top level,
            None => self.exponents[0],
        };

        let step = 1u64
            .checked_shl(parent_exp.into())
            .expect("exponentiation result must fit in u64");

        let parent_epoch = (epoch - 1) & !(step - 1);

        Some(misc::compute_start_slot_at_epoch::<P>(parent_epoch))
    }

    pub fn contains<P: Preset>(&self, slot: Slot) -> bool {
        let epoch = misc::compute_epoch_at_slot::<P>(slot);
        let start_of_epoch = misc::compute_start_slot_at_epoch::<P>(epoch);

        if slot != start_of_epoch {
            return false;
        }

        let &last_exponent = self
            .exponents
            .first()
            .expect("exponent list cannot be empty");
        let base = 1u64
            .checked_shl(last_exponent.into())
            .expect("exponentiation result must fit in u64");

        epoch % base == 0
    }
}

#[cfg(test)]
mod tests {
    use types::preset::Mainnet;

    use crate::hierarchy::Hierarchy;

    #[test]
    fn should_allow_construct_exponent_list() {
        assert!(Hierarchy::new([0, 1, 2]).is_ok());
        assert!(Hierarchy::new([1, 5, 8, 9, 17]).is_ok());
        assert!(Hierarchy::new([3]).is_ok());
        assert!(Hierarchy::new([1, 31]).is_ok());
    }

    #[test]
    fn should_reject_invalid_exponent_lists() {
        // non-sorted exponent list is not allowed
        assert!(Hierarchy::new([1, 3, 2]).is_err());
        // empty exponents are not allowed
        assert!(Hierarchy::new([]).is_err());
        // exponents are too large, they'll cause an overflow
        assert!(Hierarchy::new([31, 36, 105, 230]).is_err());
        // duplicate exponents
        assert!(Hierarchy::new([1, 1, 1, 3, 5]).is_err());
    }

    #[test]
    fn should_reference_epoch_start_for_mid_epoch_states() {
        let hierarchy = Hierarchy::new([1, 2, 3]).unwrap();
        assert_eq!(hierarchy.parent_of::<Mainnet>(1), Some(0));
        assert_eq!(hierarchy.parent_of::<Mainnet>(31), Some(0));
        assert_eq!(hierarchy.parent_of::<Mainnet>(33), Some(32));
        assert_eq!(hierarchy.parent_of::<Mainnet>(65), Some(64));
    }

    #[test]
    fn should_not_return_parent_for_root_slots() {
        let hierarchy = Hierarchy::new([1, 2, 3]).unwrap();
        assert_eq!(hierarchy.parent_of::<Mainnet>(0), None);
        assert_eq!(hierarchy.parent_of::<Mainnet>(8 * 32), None);
        assert_eq!(hierarchy.parent_of::<Mainnet>(2 * 8 * 32), None);
        assert_eq!(hierarchy.parent_of::<Mainnet>(3 * 8 * 32), None);
        assert_eq!(hierarchy.parent_of::<Mainnet>(4 * 8 * 32), None);
    }

    #[test]
    fn should_construct_correct_branch() {
        let hierarchy = Hierarchy::new([1, 2, 3]).unwrap();

        assert_eq!(
            hierarchy.parent_of::<Mainnet>((8 + 4 + 2) * 32),
            Some((8 + 4) * 32)
        );
        assert_eq!(hierarchy.parent_of::<Mainnet>((8 + 4) * 32), Some((8) * 32));
        assert_eq!(hierarchy.parent_of::<Mainnet>(8 * 32), None);
    }

    #[test]
    fn incomplete_branch_must_fallback() {
        let hierarchy = Hierarchy::new([1, 2, 3]).unwrap();

        assert_eq!(hierarchy.parent_of::<Mainnet>(2 * 32), Some(0));
        assert_eq!(hierarchy.parent_of::<Mainnet>(4 * 32), Some(0));
        assert_eq!(hierarchy.parent_of::<Mainnet>((2 + 4) * 32), Some(4 * 32));
    }
}
