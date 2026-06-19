use ssz::SszHash as _;
use types::{
    preset::Preset,
    traits::{BeaconBlock, BeaconState},
};

pub enum ProcessSlots {
    Always,
    IfNeeded,
    Never,
}

impl ProcessSlots {
    pub fn should_process<P: Preset>(
        self,
        state: &impl BeaconState<P>,
        block: &(impl BeaconBlock<P> + ?Sized),
    ) -> bool {
        match self {
            Self::Always => true,
            // The test for equality is intentional. It ensures that blocks attempting to "rewind"
            // the state are rejected early by `slot_processing::process_slots`.
            // `state.slot < block.slot` would also work, but the block would be rejected as invalid
            // later, while verifying the state root.
            Self::IfNeeded => state.slot() != block.slot(),
            Self::Never => false,
        }
    }
}

#[cfg_attr(feature = "tracing", tracing::instrument(level = "debug", fields(slot = state.slot()), skip_all))]
pub fn process_slot<P: Preset>(state: &mut impl BeaconState<P>) {
    let slot = state.slot();

    // > Cache state root
    //
    // This is what makes the first `process_slot` after a state load so expensive: it
    // recomputes the entire state Merkle tree (dominated by the validator registry) from a
    // cold cache. Every later call only rehashes the handful of nodes invalidated since, so
    // it is orders of magnitude cheaper. The child span isolates this cost in traces.
    let previous_state_root = {
        #[cfg(feature = "tracing")]
        let _span = tracing::debug_span!("hash_state_root").entered();
        state.hash_tree_root()
    };
    *state.state_roots_mut().mod_index_mut(slot) = previous_state_root;

    // > Cache latest block header state root
    if state.latest_block_header().state_root.is_zero() {
        state.latest_block_header_mut().state_root = previous_state_root;
    }

    // > Cache block root
    let previous_block_root = {
        #[cfg(feature = "tracing")]
        let _span = tracing::debug_span!("hash_latest_block_header").entered();
        state.latest_block_header().hash_tree_root()
    };
    *state.block_roots_mut().mod_index_mut(slot) = previous_block_root;

    state.cache_mut().advance_slot();
}
