use anyhow::{Result, ensure};
use bytesize::ByteSize;
use logging::warn_with_peers;
use sysinfo::{MemoryRefreshKind, RefreshKind, System};
use zstd::DEFAULT_COMPRESSION_LEVEL;

use crate::hierarchy::Hierarchy;

/// Rough resident size of one cached beacon state on mainnet. Cached entries are whole states,
/// dominated by the validator registry and balances, so this grows with the registry and is only
/// accurate to an order of magnitude.
const ESTIMATED_STATE_SIZE: ByteSize = ByteSize::mib(512);

/// Share of system memory the caches may be estimated to occupy before it is worth warning about.
const MEMORY_WARNING_PERCENT: u64 = 80;

/// Tuning options for how beacon states are persisted.
#[derive(Clone, Debug)]
pub struct StateStorageConfig {
    /// Layout of state frames and deltas written to the database.
    pub hierarchy: Hierarchy,
    /// Number of states cached in memory for every hierarchy layer, starting
    /// from the shallowest one - the full state snapshot, in the same order
    /// `hierarchy` exponents are listed in. Layers left unlisted are uncached.
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
        [5, 3, 3].into_iter().take(depth).collect()
    }

    /// Memory the state caches are expected to take once every layer is full.
    #[must_use]
    pub fn estimated_cache_size(&self) -> ByteSize {
        let cached_states = self
            .cache_sizes
            .iter()
            .map(|size| u64::try_from(*size).unwrap_or(u64::MAX))
            .fold(0, u64::saturating_add);

        ByteSize::b(ESTIMATED_STATE_SIZE.as_u64().saturating_mul(cached_states))
    }

    fn exceeds_memory_share(estimated_cache_size: ByteSize, total_memory: ByteSize) -> bool {
        estimated_cache_size.as_u64() > total_memory.as_u64() / 100 * MEMORY_WARNING_PERCENT
    }

    pub fn validate(&self, slots_per_epoch: u64) -> Result<()> {
        ensure!(
            self.cache_sizes.len() <= self.hierarchy.depth(),
            "number of state cache sizes ({}) must not exceed the number of \
            hierarchy layers ({})",
            self.cache_sizes.len(),
            self.hierarchy.depth(),
        );

        let total_memory = ByteSize::b(
            System::new_with_specifics(
                RefreshKind::nothing().with_memory(MemoryRefreshKind::nothing().with_ram()),
            )
            .total_memory(),
        );

        let estimated_cache_size = self.estimated_cache_size();

        if Self::exceeds_memory_share(estimated_cache_size, total_memory) {
            warn_with_peers!(
                "state caches are estimated to hold up to {estimated_cache_size} of states, \
                over {MEMORY_WARNING_PERCENT}% of the {total_memory} of memory this machine has; \
                the node will be OOM-killed once the caches fill",
            );
        }

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
    fn state_storage_config_accepts_fewer_cache_sizes_than_hierarchy_layers() -> Result<()> {
        let config = StateStorageConfig::default();

        assert!(config.cache_sizes.len() < config.hierarchy.depth());
        config.validate(MAINNET_SLOTS_PER_EPOCH)?;

        let single = StateStorageConfig {
            cache_sizes: vec![5],
            ..StateStorageConfig::default()
        };

        single.validate(MAINNET_SLOTS_PER_EPOCH)?;

        let exactly_as_deep = StateStorageConfig {
            hierarchy: Hierarchy::new([9, 5]).expect("exponents in tests are valid"),
            cache_sizes: vec![5, 3],
            ..StateStorageConfig::default()
        };

        exactly_as_deep.validate(MAINNET_SLOTS_PER_EPOCH)?;

        Ok(())
    }

    #[test]
    fn state_storage_config_rejects_more_cache_sizes_than_hierarchy_layers() {
        let too_many = StateStorageConfig {
            hierarchy: Hierarchy::new([9, 5]).expect("exponents in tests are valid"),
            cache_sizes: vec![5, 3, 3],
            ..StateStorageConfig::default()
        };

        let error = too_many
            .validate(MAINNET_SLOTS_PER_EPOCH)
            .expect_err("3 cache sizes do not fit into 2 hierarchy layers");

        assert_eq!(
            error.to_string(),
            "number of state cache sizes (3) must not exceed the number of hierarchy layers (2)",
        );
    }

    #[test]
    fn state_storage_config_accepts_an_oversized_cache() -> Result<()> {
        let oversized = StateStorageConfig {
            hierarchy: Hierarchy::new([5]).expect("exponents in tests are valid"),
            cache_sizes: vec![usize::MAX],
            ..StateStorageConfig::default()
        };

        oversized.validate(MAINNET_SLOTS_PER_EPOCH)?;

        Ok(())
    }

    #[test]
    fn estimated_cache_size_scales_with_the_number_of_cached_states() {
        let config = StateStorageConfig {
            hierarchy: Hierarchy::new([9, 5]).expect("exponents in tests are valid"),
            cache_sizes: vec![5, 3],
            ..StateStorageConfig::default()
        };

        assert_eq!(
            config.estimated_cache_size(),
            ByteSize::b(ESTIMATED_STATE_SIZE.as_u64() * 8),
        );

        let uncached = StateStorageConfig {
            cache_sizes: vec![],
            ..StateStorageConfig::default()
        };

        assert_eq!(uncached.estimated_cache_size(), ByteSize::b(0));

        let saturating = StateStorageConfig {
            hierarchy: Hierarchy::new([5]).expect("exponents in tests are valid"),
            cache_sizes: vec![usize::MAX],
            ..StateStorageConfig::default()
        };

        assert_eq!(saturating.estimated_cache_size(), ByteSize::b(u64::MAX));
    }

    #[test]
    fn the_memory_warning_triggers_at_80_percent_of_total_memory() {
        let total_memory = ByteSize::b(16_000_000_000);
        let threshold = ByteSize::b(12_800_000_000);

        assert!(!StateStorageConfig::exceeds_memory_share(
            threshold,
            total_memory,
        ));

        assert!(StateStorageConfig::exceeds_memory_share(
            ByteSize::b(threshold.as_u64() + 1),
            total_memory,
        ));

        assert!(!StateStorageConfig::exceeds_memory_share(
            ByteSize::b(0),
            ByteSize::b(0),
        ));
    }

    #[test]
    fn default_cache_sizes_are_truncated_to_the_hierarchy_depth() {
        assert_eq!(StateStorageConfig::default_cache_sizes(1), vec![5]);
        assert_eq!(StateStorageConfig::default_cache_sizes(3), vec![5, 3, 3]);
        assert_eq!(StateStorageConfig::default_cache_sizes(5), vec![5, 3, 3]);
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
