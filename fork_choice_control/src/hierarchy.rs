use core::{fmt, str::FromStr};

use anyhow::{Result, ensure};
use types::{config::Config, phase0::primitives::Slot, preset::Preset};

/// A hierarchy of storage.
///
/// The hierarchy is defined by the exponent of each level. The exponents define
/// the frequency at which frames and deltas are written for every layer - the
/// smaller the exponent is, the more frequently states are written and the
/// larger the layer is. For example, an exponent of `5` means the layer is
/// written every `2^5` slots, or in other words, every epoch.
///
/// The root level of the hierarchy (the last exponent in the list) specifies
/// how often the full state - the frame - is written to disk. Every subsequent
/// layer is a delta computed from the previous layer. This means the deeper the
/// layer is, the more deltas need to be applied in order to compute the state.
///
/// For states that aren't included in the hierarchy, meaning they aren't
/// written to disk, the retrieval algorithm is simple: load the closest older
/// state along with all the blocks between that older state and the requested
/// one, then use state transitions to advance to the requested state. This is
/// generally slower than applying a patch, but it doesn't require additional
/// disk space.
///
/// The hierarchy is phase-anchored - meaning, it is computed not from absolute
/// slot index, but from current phase start. This is done to avoid computing
/// cross-phase patches - it adds unnecessary complexity, without clear upsides.
#[derive(Debug, Clone)]
pub struct Hierarchy {
    exponents: Vec<u8>,
}

impl fmt::Display for Hierarchy {
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
        Self {
            exponents: vec![5, 9, 11, 13, 16, 18, 21],
        }
    }
}

impl Hierarchy {
    pub fn new(exponents: impl IntoIterator<Item = u8>) -> Result<Self> {
        let exponents = exponents.into_iter().collect::<Vec<_>>();
        ensure!(exponents.is_sorted(), "exponents must be sorted");
        ensure!(
            exponents
                .iter()
                .zip(exponents.iter().skip(1))
                .all(|(previous, next)| previous != next),
            "exponents can't be duplicate"
        );
        ensure!(!exponents.is_empty(), "exponents must not be empty");
        ensure!(
            exponents.iter().all(|&e| e <= 63),
            "max value allowed for exponent is 63"
        );

        Ok(Self { exponents })
    }

    /// Check if state is a part of hierarchy or not.
    #[inline]
    pub fn contains<P: Preset>(&self, config: &Config, checkpoint: Slot, slot: Slot) -> bool {
        let (relative_slot, _) = Self::relative_slot::<P>(config, checkpoint, slot);

        self.contains_relative(relative_slot)
    }

    /// Get the parent, that this state depends on.
    #[inline]
    pub fn parent_of<P: Preset>(
        &self,
        config: &Config,
        checkpoint: Slot,
        slot: Slot,
    ) -> Option<Slot> {
        let (relative_slot, anchor) = Self::relative_slot::<P>(config, checkpoint, slot);

        self.parent_of_relative(relative_slot)
            .map(|parent| parent.saturating_add(anchor))
    }

    /// Returns whether `slot` is a bottom-level hierarchy node. Top-level
    /// nodes are snapshots rather than leaves, including in a one-level
    /// hierarchy.
    pub fn is_leaf<P: Preset>(&self, config: &Config, checkpoint: Slot, slot: Slot) -> bool {
        // In a one-level hierarchy, there is no leafs.
        if self.exponents.len() < 2 {
            return false;
        }

        if !self.contains::<P>(config, checkpoint, slot) {
            return false;
        }

        let (relative_slot, _) = Self::relative_slot::<P>(config, checkpoint, slot);

        if relative_slot == 0 {
            return false;
        }

        let trailing_zeros = u8::try_from(relative_slot.trailing_zeros())
            .expect("trailing zero count must fit in u8");

        self.exponents.iter().rposition(|&e| e <= trailing_zeros) == Some(0)
    }

    pub fn spine<P: Preset>(&self, config: &Config, checkpoint: Slot, slot: Slot) -> Vec<Slot> {
        let (relative_slot, anchor) = Self::relative_slot::<P>(config, checkpoint, slot);

        let base = 1u64
            .checked_shl(self.exponents[0].into())
            .expect("exponentiation result must fit in u64");

        let mut spine_root = relative_slot.next_multiple_of(base);
        let mut spine = Vec::with_capacity(self.exponents.len());

        while let Some(parent) = self.parent_of_relative(spine_root) {
            spine.push(parent.saturating_add(anchor));
            spine_root = parent;
        }

        spine
    }

    #[must_use]
    pub const fn depth(&self) -> usize {
        self.exponents.len()
    }

    /// The exponent of every layer, deepest first - the same order the
    /// `--state-hierarchy` flag lists them in.
    #[must_use]
    pub fn exponents(&self) -> &[u8] {
        &self.exponents
    }

    /// `contains` method implementation, without normalizing slot first.
    fn contains_relative(&self, slot: Slot) -> bool {
        let base = 1u64
            .checked_shl(self.exponents[0].into())
            .expect("exponentiation result must fit in u64");

        slot.is_multiple_of(base)
    }

    /// `parent_of` method implementation, without normalizing slot first.
    fn parent_of_relative(&self, slot: Slot) -> Option<Slot> {
        // Slots, that aren't part of hierarchy, don't have parents
        if !self.contains_relative(slot) {
            return None;
        }

        // Anchor is always a top-level tree root.
        if slot == 0 {
            return None;
        }

        let tz = u8::try_from(slot.trailing_zeros()).expect("trailing zero count must fit in u8");

        let level_index = self.exponents.iter().rposition(|&e| e <= tz);

        let parent_exp = match level_index {
            Some(index) if index.saturating_add(1) < self.exponents.len() => {
                self.exponents[index.saturating_add(1)]
            }
            Some(_) => return None, // we're at the top level,
            None => self.exponents[0],
        };

        let step = 1u64
            .checked_shl(parent_exp.into())
            .expect("exponentiation result must fit in u64");

        Some(slot.saturating_sub(1) & !step.saturating_sub(1))
    }

    /// Convert absolute slot number to anchored, relative slot.
    ///
    /// Returns: `(relative_slot, anchor)`.
    #[inline]
    fn relative_slot<P: Preset>(config: &Config, checkpoint: Slot, slot: Slot) -> (Slot, Slot) {
        let phase = config.phase_at_slot::<P>(slot);
        let phase_start = config
            .fork_slot::<P>(phase)
            .into_option()
            .expect("the active phase must have a finite fork slot");
        let anchor = if slot >= checkpoint {
            phase_start.max(checkpoint)
        } else {
            phase_start
        };

        (slot.saturating_sub(anchor), anchor)
    }
}

#[cfg(test)]
mod tests {
    use types::preset::Mainnet;

    use super::*;

    fn hierarchy(exponents: impl IntoIterator<Item = u8>) -> Hierarchy {
        Hierarchy::new(exponents).expect("exponents in tests are valid")
    }

    #[test]
    fn is_leaf_is_always_false_in_a_one_level_hierarchy() {
        let config = Config::mainnet();
        let hierarchy = hierarchy([5]);

        for slot in [0, 32, 512, 2048] {
            assert!(!hierarchy.is_leaf::<Mainnet>(&config, 0, slot));
        }
    }

    #[test]
    fn is_leaf_classifies_nodes_by_level() {
        let config = Config::mainnet();
        let hierarchy = hierarchy([5, 9, 11]);

        // The anchor is a top-level snapshot, not a leaf.
        assert!(!hierarchy.is_leaf::<Mainnet>(&config, 0, 0));

        // Bottom-level nodes.
        assert!(hierarchy.is_leaf::<Mainnet>(&config, 0, 32));
        assert!(hierarchy.is_leaf::<Mainnet>(&config, 0, 96));
        assert!(hierarchy.is_leaf::<Mainnet>(&config, 0, 480));

        // Upper-level nodes.
        assert!(!hierarchy.is_leaf::<Mainnet>(&config, 0, 512));
        assert!(!hierarchy.is_leaf::<Mainnet>(&config, 0, 1024));
        assert!(!hierarchy.is_leaf::<Mainnet>(&config, 0, 2048));

        // Slots outside the hierarchy.
        assert!(!hierarchy.is_leaf::<Mainnet>(&config, 0, 1));
        assert!(!hierarchy.is_leaf::<Mainnet>(&config, 0, 16));
        assert!(!hierarchy.is_leaf::<Mainnet>(&config, 0, 500));
    }

    #[test]
    fn is_leaf_is_relative_to_an_unaligned_anchor() {
        let config = Config::mainnet();
        let hierarchy = hierarchy([5, 9]);
        let checkpoint = 96;

        // Relative 0 - the anchor itself.
        assert!(!hierarchy.is_leaf::<Mainnet>(&config, checkpoint, 96));

        // Relative 512 - a top-level node, even though the absolute slot has
        // only five trailing zeros.
        assert!(!hierarchy.is_leaf::<Mainnet>(&config, checkpoint, 608));

        // Relative 928 - a bottom-level node, even though the absolute slot has
        // ten trailing zeros.
        assert!(hierarchy.is_leaf::<Mainnet>(&config, checkpoint, 1024));
    }

    #[test]
    fn is_leaf_restarts_at_a_phase_boundary() {
        let config = Config {
            altair_fork_epoch: 4,
            ..Config::mainnet()
        };

        let hierarchy = hierarchy([5, 9]);

        // Phase 0 spans slots 0..128 and is anchored at 0.
        assert!(hierarchy.is_leaf::<Mainnet>(&config, 0, 32));
        assert!(!hierarchy.is_leaf::<Mainnet>(&config, 0, 0));

        // Altair starts at slot 128, which becomes the new anchor.
        assert!(!hierarchy.is_leaf::<Mainnet>(&config, 0, 128));
        assert!(hierarchy.is_leaf::<Mainnet>(&config, 0, 160));
        assert!(!hierarchy.is_leaf::<Mainnet>(&config, 0, 640));
    }

    #[test]
    fn contains_matches_multiples_of_the_deepest_exponent() {
        let config = Config::mainnet();
        let hierarchy = hierarchy([5, 9, 11]);

        for slot in [0, 32, 64, 480, 2048] {
            assert!(hierarchy.contains::<Mainnet>(&config, 0, slot));
        }

        for slot in [1, 31, 33, 500] {
            assert!(!hierarchy.contains::<Mainnet>(&config, 0, slot));
        }
    }

    #[test]
    fn contains_is_relative_to_an_unaligned_anchor() {
        let config = Config::mainnet();
        let hierarchy = hierarchy([5, 9]);
        let checkpoint = 96;

        // At and after the checkpoint slots are counted from it.
        assert!(hierarchy.contains::<Mainnet>(&config, checkpoint, 96));
        assert!(hierarchy.contains::<Mainnet>(&config, checkpoint, 128));
        assert!(hierarchy.contains::<Mainnet>(&config, checkpoint, 1024));
        assert!(!hierarchy.contains::<Mainnet>(&config, checkpoint, 120));

        // Before the checkpoint the phase start is still the anchor.
        assert!(hierarchy.contains::<Mainnet>(&config, checkpoint, 0));
        assert!(hierarchy.contains::<Mainnet>(&config, checkpoint, 64));
        assert!(!hierarchy.contains::<Mainnet>(&config, checkpoint, 80));
    }

    #[test]
    fn contains_restarts_at_a_phase_boundary() {
        let config = Config {
            altair_fork_epoch: 4,
            ..Config::mainnet()
        };

        let hierarchy = hierarchy([5, 9]);

        assert!(hierarchy.contains::<Mainnet>(&config, 0, 96));
        assert!(hierarchy.contains::<Mainnet>(&config, 0, 128));
        assert!(hierarchy.contains::<Mainnet>(&config, 0, 160));
        assert!(!hierarchy.contains::<Mainnet>(&config, 0, 144));
    }

    #[test]
    fn parent_of_returns_none_outside_the_hierarchy() {
        let config = Config::mainnet();
        let hierarchy = hierarchy([5, 9, 11]);

        for slot in [1, 31, 33, 500] {
            assert_eq!(hierarchy.parent_of::<Mainnet>(&config, 0, slot), None);
        }
    }

    #[test]
    fn parent_of_returns_none_at_the_anchor_and_at_top_level_nodes() {
        let config = Config::mainnet();
        let hierarchy = hierarchy([5, 9, 11]);

        assert_eq!(hierarchy.parent_of::<Mainnet>(&config, 0, 0), None);
        assert_eq!(hierarchy.parent_of::<Mainnet>(&config, 0, 2048), None);
        assert_eq!(hierarchy.parent_of::<Mainnet>(&config, 0, 4096), None);
    }

    #[test]
    fn parent_of_resolves_each_level_to_the_next_larger_exponent() {
        let config = Config::mainnet();
        let hierarchy = hierarchy([5, 9, 11, 13]);

        // Bottom level snaps to a multiple of 2^9.
        assert_eq!(hierarchy.parent_of::<Mainnet>(&config, 0, 32), Some(0));
        assert_eq!(hierarchy.parent_of::<Mainnet>(&config, 0, 544), Some(512));

        // 2^9 nodes snap to a multiple of 2^11.
        assert_eq!(hierarchy.parent_of::<Mainnet>(&config, 0, 512), Some(0));
        assert_eq!(hierarchy.parent_of::<Mainnet>(&config, 0, 2560), Some(2048));

        // 2^11 nodes snap to a multiple of 2^13.
        assert_eq!(hierarchy.parent_of::<Mainnet>(&config, 0, 2048), Some(0));
        assert_eq!(
            hierarchy.parent_of::<Mainnet>(&config, 0, 10240),
            Some(8192)
        );

        // 2^13 nodes are top-level.
        assert_eq!(hierarchy.parent_of::<Mainnet>(&config, 0, 8192), None);
    }

    #[test]
    fn parent_of_is_relative_to_an_unaligned_anchor() {
        let config = Config::mainnet();
        let hierarchy = hierarchy([5, 9]);
        let checkpoint = 96;

        assert_eq!(
            hierarchy.parent_of::<Mainnet>(&config, checkpoint, 96),
            None
        );
        assert_eq!(
            hierarchy.parent_of::<Mainnet>(&config, checkpoint, 128),
            Some(96)
        );
        assert_eq!(
            hierarchy.parent_of::<Mainnet>(&config, checkpoint, 1024),
            Some(608)
        );

        // Relative 512 is a top-level node.
        assert_eq!(
            hierarchy.parent_of::<Mainnet>(&config, checkpoint, 608),
            None
        );
    }

    #[test]
    fn spine_is_empty_at_the_anchor() {
        let config = Config::mainnet();
        let hierarchy = hierarchy([5, 9, 11]);

        assert_eq!(
            hierarchy.spine::<Mainnet>(&config, 0, 0),
            Vec::<Slot>::new()
        );
        assert_eq!(
            hierarchy.spine::<Mainnet>(&config, 96, 96),
            Vec::<Slot>::new()
        );
    }

    #[test]
    fn spine_returns_ancestors_deepest_first_and_stops_at_a_top_level_node() {
        let config = Config::mainnet();
        let hierarchy = hierarchy([5, 9, 11, 13]);

        // 10784 is a bottom-level node under 10752, 10240 and the top-level
        // 8192. The anchor is not an ancestor - 8192 is a snapshot.
        assert_eq!(
            hierarchy.spine::<Mainnet>(&config, 0, 10784),
            vec![10752, 10240, 8192],
        );

        assert_eq!(
            hierarchy.spine::<Mainnet>(&config, 0, 8192),
            Vec::<Slot>::new()
        );
    }

    #[test]
    fn spine_of_a_slot_outside_the_hierarchy_uses_the_next_hierarchy_slot() {
        let config = Config::mainnet();
        let hierarchy = hierarchy([5, 9, 11, 13]);

        assert_eq!(
            hierarchy.spine::<Mainnet>(&config, 0, 10770),
            hierarchy.spine::<Mainnet>(&config, 0, 10784),
        );
    }

    #[test]
    fn spine_is_relative_to_an_unaligned_anchor() {
        let config = Config::mainnet();
        let hierarchy = hierarchy([5, 9]);
        let checkpoint = 96;

        assert_eq!(
            hierarchy.spine::<Mainnet>(&config, checkpoint, 1024),
            vec![608],
        );
    }

    #[test]
    fn relative_slot_anchors_at_the_phase_start_across_a_boundary() {
        let config = Config {
            altair_fork_epoch: 4,
            ..Config::mainnet()
        };

        assert_eq!(Hierarchy::relative_slot::<Mainnet>(&config, 0, 96), (96, 0));
        assert_eq!(
            Hierarchy::relative_slot::<Mainnet>(&config, 0, 128),
            (0, 128)
        );
        assert_eq!(
            Hierarchy::relative_slot::<Mainnet>(&config, 0, 160),
            (32, 128)
        );
    }

    #[test]
    fn relative_slot_anchors_at_a_checkpoint_after_the_fork_slot() {
        let config = Config {
            altair_fork_epoch: 4,
            ..Config::mainnet()
        };

        let checkpoint = 200;

        // At and after the checkpoint the anchor is max(phase_start, checkpoint).
        assert_eq!(
            Hierarchy::relative_slot::<Mainnet>(&config, checkpoint, 300),
            (100, 200),
        );

        // Below the checkpoint the checkpoint is ignored.
        assert_eq!(
            Hierarchy::relative_slot::<Mainnet>(&config, checkpoint, 150),
            (22, 128),
        );
        assert_eq!(
            Hierarchy::relative_slot::<Mainnet>(&config, checkpoint, 100),
            (100, 0),
        );
    }

    #[test]
    fn relative_slot_anchors_at_a_checkpoint_before_the_fork_slot() {
        let config = Config {
            altair_fork_epoch: 4,
            ..Config::mainnet()
        };

        let checkpoint = 64;

        // The phase start wins over an older checkpoint.
        assert_eq!(
            Hierarchy::relative_slot::<Mainnet>(&config, checkpoint, 300),
            (172, 128),
        );

        // Within the phase that contains the checkpoint, the checkpoint wins.
        assert_eq!(
            Hierarchy::relative_slot::<Mainnet>(&config, checkpoint, 100),
            (36, 64),
        );

        // Below the checkpoint the phase start is the anchor.
        assert_eq!(
            Hierarchy::relative_slot::<Mainnet>(&config, checkpoint, 32),
            (32, 0),
        );
    }

    #[test]
    fn new_rejects_invalid_exponents() {
        assert_eq!(
            Hierarchy::new([9, 5])
                .expect_err("unsorted exponents must be rejected")
                .to_string(),
            "exponents must be sorted",
        );

        assert_eq!(
            Hierarchy::new([5, 5, 9])
                .expect_err("duplicate exponents must be rejected")
                .to_string(),
            "exponents can't be duplicate",
        );

        assert_eq!(
            Hierarchy::new([])
                .expect_err("empty exponents must be rejected")
                .to_string(),
            "exponents must not be empty",
        );

        assert_eq!(
            Hierarchy::new([5, 64])
                .expect_err("exponents above 63 must be rejected")
                .to_string(),
            "max value allowed for exponent is 63",
        );
    }

    /// Pruning keeps `spine(boundary) ∪ {boundary}` and deletes everything
    /// else below the boundary. That is only sound if every state written
    /// after the boundary can still reach a stored ancestor, so no hierarchy
    /// node above the boundary may depend on a node that pruning deletes.
    #[test]
    fn pruning_retention_set_covers_every_ancestor_below_the_boundary() {
        const LIMIT: Slot = 8192;

        let config = Config::mainnet();

        for exponents in [vec![5], vec![5, 9], vec![5, 9, 11], vec![5, 7, 9, 11]] {
            let hierarchy = hierarchy(exponents.iter().copied());

            // An anchor of 31 is what checkpoint sync produces when the
            // checkpoint epoch starts with an empty slot. It is the only case
            // in which a prune boundary lands on a hierarchy node.
            for anchor in [0, 31] {
                let nodes = (anchor..LIMIT)
                    .filter(|&slot| hierarchy.contains::<Mainnet>(&config, anchor, slot))
                    .collect::<Vec<_>>();

                for boundary in (31..LIMIT).step_by(32) {
                    let mut retained = hierarchy.spine::<Mainnet>(&config, anchor, boundary);

                    if hierarchy.contains::<Mainnet>(&config, anchor, boundary) {
                        retained.push(boundary);
                    }

                    for &node in nodes.iter().filter(|&&slot| slot > boundary) {
                        let mut ancestor = hierarchy.parent_of::<Mainnet>(&config, anchor, node);

                        while let Some(slot) = ancestor {
                            assert!(
                                slot > boundary || retained.contains(&slot),
                                "hierarchy {exponents:?} anchored at {anchor}: ancestor {slot} \
                                 of node {node} is not retained at boundary {boundary}",
                            );

                            ancestor = hierarchy.parent_of::<Mainnet>(&config, anchor, slot);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn depth_is_the_number_of_levels() {
        assert_eq!(hierarchy([5]).depth(), 1);
        assert_eq!(hierarchy([5, 9, 11]).depth(), 3);
        assert_eq!(Hierarchy::default().depth(), 7);
    }

    #[test]
    fn display_and_from_str_round_trip() {
        assert_eq!(Hierarchy::default().to_string(), "5,9,11,13,16,18,21");
        assert_eq!(hierarchy([5]).to_string(), "5");

        let parsed = "5,9,11"
            .parse::<Hierarchy>()
            .expect("hierarchy string is valid");

        assert_eq!(parsed.to_string(), "5,9,11");
        assert_eq!(
            Hierarchy::default()
                .to_string()
                .parse::<Hierarchy>()
                .expect("hierarchy string is valid")
                .to_string(),
            Hierarchy::default().to_string(),
        );
    }

    #[test]
    fn from_str_rejects_invalid_strings() {
        "".parse::<Hierarchy>()
            .expect_err("an empty string is not a hierarchy");

        "5,abc"
            .parse::<Hierarchy>()
            .expect_err("exponents must be numeric");

        "5,-1"
            .parse::<Hierarchy>()
            .expect_err("exponents must be unsigned");
        assert_eq!(
            "9,5"
                .parse::<Hierarchy>()
                .expect_err("unsorted exponents must be rejected")
                .to_string(),
            "exponents must be sorted",
        );
    }
}
