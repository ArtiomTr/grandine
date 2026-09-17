//! A short chain of real blocks and states on the minimal preset, for storage tests that need
//! more than hand-built rows.

use std::{collections::HashSet, sync::Arc};

use anyhow::Result;
use database::Database;
use fork_choice_store::{ChainLink, PayloadPresence, Store, StoreConfig};
use pubkey_cache::PubkeyCache;
use ssz::{SszHash as _, SszRead as _, SszWrite as _};
use std_ext::ArcExt as _;
use transition_functions::combined;
use types::{
    combined::{BeaconState, SignedBeaconBlock},
    config::Config,
    nonstandard::{PayloadStatus, StorageMode},
    phase0::{
        containers::Checkpoint,
        primitives::{H256, Slot},
    },
    preset::Minimal,
    traits::{BeaconState as _, SignedBeaconBlock as _},
};

use crate::{Storage, hierarchy::Hierarchy, state_storage_config::StateStorageConfig};

pub struct TestChain {
    pub storage: Storage<Minimal>,
    pub genesis_block: Arc<SignedBeaconBlock<Minimal>>,
    pub genesis_state: Arc<BeaconState<Minimal>>,
    pub blocks: Vec<Arc<SignedBeaconBlock<Minimal>>>,
    /// The state at every slot from genesis to the last block.
    pub states: Vec<Arc<BeaconState<Minimal>>>,
}

impl TestChain {
    /// A chain with a block at each of `block_slots`, next to an empty in-memory storage whose
    /// hierarchy has a node every 4 slots. Nothing is written to the storage.
    pub fn new(block_slots: impl IntoIterator<Item = Slot>) -> Result<Self> {
        Self::with_storage_mode(block_slots, StorageMode::default())
    }

    pub fn with_storage_mode(
        block_slots: impl IntoIterator<Item = Slot>,
        storage_mode: StorageMode,
    ) -> Result<Self> {
        let config = Arc::new(Config::minimal());
        let pubkey_cache = Arc::new(PubkeyCache::default());
        let hierarchy = Hierarchy::new([4, 2])?;

        let storage = Storage::new(
            config.clone_arc(),
            pubkey_cache.clone_arc(),
            Database::in_memory(),
            storage_mode,
            StateStorageConfig {
                cache_sizes: vec![0; hierarchy.depth()],
                hierarchy,
                ..StateStorageConfig::default()
            },
            None,
        )?;

        let (genesis_state, _) = factory::min_genesis_state::<Minimal>(&config, &pubkey_cache)?;
        let genesis_block = Arc::new(genesis::beacon_block(&genesis_state));

        let mut chain = Self {
            storage,
            genesis_block,
            genesis_state: genesis_state.clone_arc(),
            blocks: vec![],
            states: vec![genesis_state],
        };

        for block_slot in block_slots {
            chain.push_block(block_slot)?;
        }

        Ok(chain)
    }

    /// Builds a block at `slot` on top of the last state, extending `states` up to it.
    fn push_block(&mut self, slot: Slot) -> Result<()> {
        let config = self.storage.config();
        let pubkey_cache = &self.storage.pubkey_cache;
        let mut state = self
            .states
            .last()
            .expect("genesis is always present")
            .clone_arc();

        while state.slot().saturating_add(1) < slot {
            let next_slot = state.slot().saturating_add(1);
            combined::process_slots(config, pubkey_cache, state.make_mut(), next_slot)?;
            self.states.push(state.clone_arc());
        }

        let (block, post_state) =
            factory::empty_block(config, pubkey_cache, state, slot, H256::zero())?;

        self.blocks.push(block);
        self.states.push(post_state);

        Ok(())
    }

    /// A block at `slot` competing with the one the chain has there, built on the state before
    /// it. The chain itself is left unchanged.
    pub fn sibling(&self, slot: Slot) -> Result<ChainLink<Minimal>> {
        let parent_slot = slot.checked_sub(1).expect("genesis has no siblings");
        let parent_state = self.state(parent_slot);

        let (block, state) = factory::empty_block(
            self.storage.config(),
            &self.storage.pubkey_cache,
            parent_state,
            slot,
            H256::repeat_byte(1),
        )?;

        Ok(chain_link(block, state))
    }

    /// A store anchored at genesis over the chain's storage. Chain links carry their states, so
    /// the store is only there for what `Storage::append` asks of it.
    pub fn store(&self) -> Store<Minimal, Storage<Minimal>> {
        Store::new(
            self.storage.config().clone_arc(),
            self.storage.pubkey_cache.clone_arc(),
            StoreConfig::default(),
            self.genesis_block.clone_arc(),
            self.genesis_state.clone_arc(),
            Arc::new(self.storage.clone()),
            true,
            true,
            HashSet::new(),
            Arc::default(),
        )
    }

    /// The root of the stored state at `slot`, recomputed from its contents, because
    /// `stored_state` may carry a cached root that would echo the expected one back.
    pub fn stored_state_root(&self, slot: Slot) -> Result<Option<H256>> {
        let config = self.storage.config();

        self.storage
            .stored_state(slot, Some(self.genesis_state.validators()))?
            .map(|state| {
                Ok(BeaconState::<Minimal>::from_ssz(config, state.to_ssz()?)?.hash_tree_root())
            })
            .transpose()
    }

    pub fn state(&self, slot: Slot) -> Arc<BeaconState<Minimal>> {
        self.states[usize::try_from(slot).expect("test slots fit in usize")].clone_arc()
    }

    /// The chain link of the block at `slot`, or of genesis at slot 0.
    pub fn link(&self, slot: Slot) -> ChainLink<Minimal> {
        let block = if slot == 0 {
            self.genesis_block.clone_arc()
        } else {
            self.blocks
                .iter()
                .find(|block| block.message().slot() == slot)
                .expect("the chain has a block at the requested slot")
                .clone_arc()
        };

        chain_link(block, self.state(slot))
    }
}

fn chain_link(
    block: Arc<SignedBeaconBlock<Minimal>>,
    state: Arc<BeaconState<Minimal>>,
) -> ChainLink<Minimal> {
    ChainLink {
        block_root: block.message().hash_tree_root(),
        block,
        state: Some(state),
        current_justified_checkpoint: Checkpoint::default(),
        finalized_checkpoint: Checkpoint::default(),
        unrealized_justified_checkpoint: Checkpoint::default(),
        unrealized_finalized_checkpoint: Checkpoint::default(),
        payload_status: PayloadStatus::Valid,
        parent_payload_presence: PayloadPresence::default(),
    }
}
