use anyhow::{Result, ensure};
use zstd::DEFAULT_COMPRESSION_LEVEL;

use crate::hierarchy::Hierarchy;

const MAX_STATE_CACHE_SIZE: usize = 1024;

/// Tuning options for how beacon states are persisted.
#[derive(Clone, Debug)]
pub struct StateStorageConfig {
    /// Layout of state frames and deltas written to the database.
    pub hierarchy: Hierarchy,
    /// Number of states cached in memory for every hierarchy layer, starting
    /// from the shallowest one - the full state snapshot, in the same order
    /// `hierarchy` exponents are listed in.
    pub cache_sizes: Vec<usize>,
    /// zstd compression level used for state frames and deltas.
    pub compression_level: i32,
}

impl Default for StateStorageConfig {
    fn default() -> Self {
        let hierarchy = Hierarchy::default();

        Self {
            cache_sizes: Self::default_cache_sizes(hierarchy.depth()),
            hierarchy,
            compression_level: DEFAULT_COMPRESSION_LEVEL,
        }
    }
}

impl StateStorageConfig {
    /// Cache sizes for a hierarchy of `depth` layers, shallowest layer first. Only the shallowest
    /// layers are worth caching - they are the ones every deeper read has to walk through.
    #[must_use]
    pub fn default_cache_sizes(depth: usize) -> Vec<usize> {
        [5, 3, 3]
            .into_iter()
            .chain(core::iter::repeat(0))
            .take(depth)
            .collect()
    }

    pub fn validate(&self, slots_per_epoch: u64) -> Result<()> {
        ensure!(
            self.cache_sizes.len() == self.hierarchy.depth(),
            "number of state cache sizes ({}) must match the number of \
            hierarchy layers ({})",
            self.cache_sizes.len(),
            self.hierarchy.depth(),
        );

        // Every cached entry is a whole beacon state, so even the upper bound is far more than any
        // node can hold. It exists so that an absurd value fails here with a clear message instead
        // of inside the cache the layers are built from.
        ensure!(
            self.cache_sizes
                .iter()
                .all(|size| *size <= MAX_STATE_CACHE_SIZE),
            "state cache sizes must not exceed {MAX_STATE_CACHE_SIZE}",
        );

        // States are only ever persisted at hierarchy slots, and every read path that loads a
        // persisted state as an anchor requires it to be at an epoch start. The deepest layer is
        // written every `2^exponents.last()` slots, so that has to be a whole number of epochs.
        let deepest_exponent = *self
            .hierarchy
            .exponents()
            .last()
            .expect("Hierarchy::new rejects empty exponents");

        let deepest_interval = 1u64
            .checked_shl(deepest_exponent.into())
            .expect("Hierarchy::new rejects exponents above 63");

        ensure!(
            deepest_interval.is_multiple_of(slots_per_epoch),
            "deepest state hierarchy exponent ({deepest_exponent}) writes a state every \
            {deepest_interval} slots, which is not a whole number of epochs \
            ({slots_per_epoch} slots); states must be persisted at epoch starts",
        );

        let level_range = zstd::compression_level_range();

        ensure!(
            level_range.contains(&self.compression_level),
            "state compression level must be in range {}..={}",
            level_range.start(),
            level_range.end(),
        );

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAINNET_SLOTS_PER_EPOCH: u64 = 32;

    #[test]
    fn state_storage_config_validates_cache_sizes_against_hierarchy_depth() -> Result<()> {
        let config = StateStorageConfig::default();

        assert_eq!(config.cache_sizes.len(), config.hierarchy.depth());
        config.validate(MAINNET_SLOTS_PER_EPOCH)?;

        let too_few = StateStorageConfig {
            cache_sizes: vec![5, 3],
            ..StateStorageConfig::default()
        };

        assert!(too_few.validate(MAINNET_SLOTS_PER_EPOCH).is_err());

        let too_many = StateStorageConfig {
            hierarchy: Hierarchy::new([9, 5]).expect("exponents in tests are valid"),
            cache_sizes: vec![5, 3, 3],
            ..StateStorageConfig::default()
        };

        assert!(too_many.validate(MAINNET_SLOTS_PER_EPOCH).is_err());

        Ok(())
    }

    #[test]
    fn state_storage_config_rejects_an_absurd_cache_size() {
        let too_large = StateStorageConfig {
            hierarchy: Hierarchy::new([5]).expect("exponents in tests are valid"),
            cache_sizes: vec![usize::MAX],
            ..StateStorageConfig::default()
        };

        assert!(too_large.validate(MAINNET_SLOTS_PER_EPOCH).is_err());

        let at_the_limit = StateStorageConfig {
            hierarchy: Hierarchy::new([5]).expect("exponents in tests are valid"),
            cache_sizes: vec![MAX_STATE_CACHE_SIZE],
            ..StateStorageConfig::default()
        };

        at_the_limit
            .validate(MAINNET_SLOTS_PER_EPOCH)
            .expect("a cache size at the limit is accepted");
    }

    #[test]
    fn default_cache_sizes_match_any_hierarchy_depth() {
        assert_eq!(StateStorageConfig::default_cache_sizes(1), vec![5]);
        assert_eq!(StateStorageConfig::default_cache_sizes(3), vec![5, 3, 3]);
        assert_eq!(
            StateStorageConfig::default_cache_sizes(5),
            vec![5, 3, 3, 0, 0],
        );
    }

    #[test]
    fn state_storage_config_rejects_a_sub_epoch_deepest_layer() -> Result<()> {
        // Every read path that loads a persisted state as an anchor requires it to be at an epoch
        // start, so a layer written more often than once per epoch would make the database
        // unloadable.
        let sub_epoch = StateStorageConfig {
            hierarchy: Hierarchy::new([9, 3]).expect("exponents in tests are valid"),
            cache_sizes: StateStorageConfig::default_cache_sizes(2),
            ..StateStorageConfig::default()
        };

        assert!(sub_epoch.validate(MAINNET_SLOTS_PER_EPOCH).is_err());

        // The same hierarchy is valid on a preset with 8 slots per epoch.
        sub_epoch.validate(8)?;

        let epoch_aligned = StateStorageConfig {
            hierarchy: Hierarchy::new([9, 5]).expect("exponents in tests are valid"),
            cache_sizes: StateStorageConfig::default_cache_sizes(2),
            ..StateStorageConfig::default()
        };

        epoch_aligned.validate(MAINNET_SLOTS_PER_EPOCH)?;

        Ok(())
    }

    #[test]
    fn state_storage_config_validates_the_compression_level_range() -> Result<()> {
        let level_range = zstd::compression_level_range();

        let in_range = StateStorageConfig {
            compression_level: *level_range.end(),
            ..StateStorageConfig::default()
        };

        in_range.validate(MAINNET_SLOTS_PER_EPOCH)?;

        let above = StateStorageConfig {
            compression_level: level_range.end() + 1,
            ..StateStorageConfig::default()
        };

        assert!(above.validate(MAINNET_SLOTS_PER_EPOCH).is_err());

        let below = StateStorageConfig {
            compression_level: level_range.start() - 1,
            ..StateStorageConfig::default()
        };

        assert!(below.validate(MAINNET_SLOTS_PER_EPOCH).is_err());

        Ok(())
    }
}
