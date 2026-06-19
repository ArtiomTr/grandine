use core::{cell::OnceCell, num::NonZeroU64};
use std::{
    borrow::Cow,
    fmt::{self, Display},
    io::Write,
    marker::PhantomData,
    num::NonZeroUsize,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::{Context as _, Error as AnyhowError, Result, bail, ensure};
use arc_swap::ArcSwapOption;
use cached::{Cached as _, SizedCache};
use database::{Compression, CompressionSelector, Database, PrefixableKey};
use derive_more::Display;
use diff::{BeaconStatePatch, Patch};
use fork_choice_store::{ChainLink, Store};
use genesis::AnchorCheckpointProvider;
use helper_functions::{accessors, misc};
use itertools::Itertools as _;
use logging::{debug_with_peers, info_with_peers, warn_with_peers};
use nonzero_ext::nonzero;
use parking_lot::Mutex;
use pubkey_cache::PubkeyCache;
use reqwest::Client;
use ssz::{Ssz, SszRead, SszReadDefault, SszWrite};
use std_ext::ArcExt as _;
use thiserror::Error;
use tracing::{Level, Span, field, info, span};
use transition_functions::combined;
use typenum::Unsigned as _;
use types::{
    Validators,
    combined::{BeaconState, DataColumnSidecar, SignedBeaconBlock},
    config::Config,
    deneb::{
        containers::{BlobIdentifier, BlobSidecar},
        primitives::BlobIndex,
    },
    fulu::{containers::DataColumnIdentifier, primitives::ColumnIndex},
    nonstandard::{BlobSidecarWithId, DataColumnSidecarWithId, FinalizedCheckpoint, StorageMode},
    phase0::{
        consts::GENESIS_SLOT,
        primitives::{Epoch, H256, Slot},
        validator_list::PubkeyList,
    },
    preset::{Mainnet, Preset},
    redacting_url::RedactingUrl,
    traits::{BeaconState as _, SignedBeaconBlock as _},
};

use crate::{checkpoint_sync, hierarchy::Hierarchy};

pub const DEFAULT_ARCHIVAL_EPOCH_INTERVAL: NonZeroU64 = nonzero!(32_u64);
pub const MAX_DATA_COLUMN_EPOCHS_TO_PRUNE: usize = 100;

pub enum StateLoadStrategy<P: Preset> {
    Auto {
        state_slot: Option<Slot>,
        checkpoint_sync_url: Option<RedactingUrl>,
        anchor_checkpoint_provider: AnchorCheckpointProvider<P>,
    },
    Remote {
        checkpoint_sync_url: RedactingUrl,
    },
    Anchor {
        block: Arc<SignedBeaconBlock<P>>,
        state: Arc<BeaconState<P>>,
    },
}

#[expect(clippy::struct_field_names)]
#[derive(Clone)]
pub struct Storage<P: Preset> {
    config: Arc<Config>,
    pub(crate) database: Arc<Database>,
    storage_mode: StorageMode,
    pub(crate) pubkey_cache: Arc<PubkeyCache>,
    hierarchy: Hierarchy,
    anchor_slot: Arc<AtomicU64>,
    frame_cache: Arc<Mutex<SizedCache<H256, Arc<BeaconState<P>>>>>,
    _phantom: PhantomData<P>,
}

#[derive(Debug, Ssz)]
#[ssz(derive_hash = false)]
struct StatePointDelta<P: Preset> {
    from: H256,
    patch: BeaconStatePatch<P>,
}

impl<P: Preset> Storage<P> {
    #[must_use]
    pub fn new(
        config: Arc<Config>,
        pubkey_cache: Arc<PubkeyCache>,
        database: Database,
        hierarchy: Hierarchy,
        storage_mode: StorageMode,
    ) -> Self {
        Self {
            config,
            pubkey_cache,
            database: Arc::new(database),
            storage_mode,
            hierarchy,
            anchor_slot: Arc::new(AtomicU64::new(0)),
            frame_cache: Arc::new(Mutex::new(SizedCache::with_size(3))),
            _phantom: PhantomData,
        }
    }

    #[must_use]
    pub(crate) const fn config(&self) -> &Arc<Config> {
        &self.config
    }

    #[must_use]
    pub const fn archive_storage_enabled(&self) -> bool {
        self.storage_mode.is_archive()
    }

    #[must_use]
    pub const fn prune_storage_enabled(&self) -> bool {
        self.storage_mode.is_prune()
    }

    #[expect(clippy::too_many_lines)]
    pub async fn load(
        &self,
        client: &Client,
        state_load_strategy: StateLoadStrategy<P>,
    ) -> Result<(StateStorage<'_, P>, bool)> {
        let anchor_block;
        let anchor_state;
        let unfinalized_blocks: UnfinalizedBlocks<P>;
        let loaded_from_remote;

        match state_load_strategy {
            StateLoadStrategy::Auto {
                state_slot,
                checkpoint_sync_url,
                anchor_checkpoint_provider,
            } => 'block: {
                // Attempt to load local state first: either latest or from specified slot.
                let local_state_storage = match state_slot {
                    Some(slot) => self.load_state_by_iteration(slot, None)?,
                    None => self.load_latest_state(None)?,
                };

                if let Some(url) = checkpoint_sync_url {
                    if local_state_storage.is_none() {
                        let result = if let Some(checkpoint) =
                            anchor_checkpoint_provider.checkpoint().checkpoint_synced()
                        {
                            info_with_peers!(
                                "anchor checkpoint is already loaded from remote checkpoint sync server"
                            );
                            Ok(checkpoint)
                        } else {
                            checkpoint_sync::load_finalized_from_remote(&self.config, client, &url)
                                .await
                                .context(Error::CheckpointSyncFailed)
                        };

                        match result {
                            Ok(FinalizedCheckpoint { block, state }) => {
                                anchor_block = block;
                                anchor_state = state;
                                unfinalized_blocks = Box::new(core::iter::empty());
                                loaded_from_remote = true;
                                break 'block;
                            }
                            Err(error) => warn_with_peers!("{error:#}"),
                        }
                    } else {
                        warn_with_peers!(
                            "skipping checkpoint sync: existing database found; \
                             pass --force-checkpoint-sync to force checkpoint sync",
                        );
                    }
                }

                match local_state_storage {
                    OptionalStateStorage::Full(state_storage) => {
                        (anchor_state, anchor_block, unfinalized_blocks) = state_storage;
                    }
                    // State might not be found but unfinalized blocks could be present.
                    OptionalStateStorage::UnfinalizedOnly(local_unfinalized_blocks) => {
                        let FinalizedCheckpoint { block, state } =
                            anchor_checkpoint_provider.checkpoint().value;

                        anchor_block = block;
                        anchor_state = state;
                        unfinalized_blocks = local_unfinalized_blocks;
                    }
                    OptionalStateStorage::None => {
                        let FinalizedCheckpoint { block, state } =
                            anchor_checkpoint_provider.checkpoint().value;

                        anchor_block = block;
                        anchor_state = state;
                        unfinalized_blocks = Box::new(core::iter::empty());
                    }
                }

                loaded_from_remote = false;
            }
            StateLoadStrategy::Remote {
                checkpoint_sync_url,
            } => {
                let FinalizedCheckpoint { block, state } =
                    checkpoint_sync::load_finalized_from_remote(
                        &self.config,
                        client,
                        &checkpoint_sync_url,
                    )
                    .await
                    .context(Error::CheckpointSyncFailed)?;

                anchor_block = block;
                anchor_state = state;
                unfinalized_blocks = Box::new(core::iter::empty());
                loaded_from_remote = true;
            }
            StateLoadStrategy::Anchor { block, state } => {
                anchor_block = block;
                anchor_state = state;
                unfinalized_blocks = Box::new(core::iter::empty());
                loaded_from_remote = false;
            }
        }

        // decompress and load all missing anchor state pubkeys into cache
        if let Err(error) = self.pubkey_cache.load_and_persist_state_keys(&anchor_state) {
            warn_with_peers!(
                "error occurred while loading anchor state keys into pubkey_cache: {error:?}"
            );
        }

        let anchor_slot = anchor_block.message().slot();
        let anchor_block_root = anchor_block.message().hash_tree_root();
        let anchor_state_root = anchor_block.message().state_root();

        info_with_peers!("loaded state at slot {anchor_slot}");

        let anchor_validators = anchor_state.validators();

        let mut batch = vec![
            serialize(FinalizedBlockByRoot(anchor_block_root), &anchor_block)?,
            serialize(BlockRootBySlot(anchor_slot), anchor_block_root)?,
            serialize(SlotByStateRoot(anchor_state_root), anchor_slot)?,
        ];

        // The referential point used to align hierarchical state diffs must stay
        // fixed across restarts. On a fresh start (first run or
        // `--force-checkpoint-sync`, both of which load from remote/genesis without
        // a pre-existing anchor) we establish a new referential point and persist
        // its snapshot. On restart we instead pick up the anchor that was stored on
        // disk, so newly persisted states keep being diffed against the same point
        // as before the node was stopped.
        let stored_anchor = if loaded_from_remote {
            None
        } else {
            self.get::<StateAnchor>(StateAnchorKey)?
        };

        let hierarchy_anchor_slot = if let Some(StateAnchor { slot, block_root }) = stored_anchor {
            info_with_peers!(
                "reusing stored anchor referential point at slot {slot} (block root {block_root:?})"
            );

            slot
        } else {
            let prepared_anchor_state =
                prepare_state(anchor_state.clone_arc(), anchor_validators.len_usize());
            let prepared_anchor_slot = prepared_anchor_state.slot();

            batch.push(serialize(
                StateByBlockRoot::snapshot(anchor_block_root),
                &prepared_anchor_state,
            )?);

            batch.push(serialize(
                StateAnchorKey,
                StateAnchor {
                    slot: prepared_anchor_slot,
                    block_root: anchor_block_root,
                },
            )?);

            prepared_anchor_slot
        };

        self.append_finalized_validator_pubkeys_to_batch(&mut batch, anchor_validators)?;

        self.database.put_batch(batch)?;

        self.anchor_slot
            .store(hierarchy_anchor_slot, Ordering::SeqCst);

        // The cache holds referential frames with their pubkeys intact; only the
        // serialized snapshot above is zeroized to shrink it on disk.
        self.frame_cache
            .lock()
            .cache_set(anchor_block_root, anchor_state.clone_arc());

        let state_storage = (anchor_state, anchor_block, unfinalized_blocks);

        Ok((state_storage, loaded_from_remote))
    }

    fn load_latest_state(
        &self,
        finalized_validators: Option<&Validators<P>>,
    ) -> Result<OptionalStateStorage<'_, P>> {
        if let Some((state, block, blocks)) =
            self.load_state_and_blocks_from_checkpoint(finalized_validators)?
        {
            Ok(OptionalStateStorage::Full((state, block, blocks)))
        } else {
            info_with_peers!(
                "latest state checkpoint was not found; \
                 attempting to find stored state by iteration",
            );

            self.load_state_by_iteration(Slot::MAX, finalized_validators)
        }
    }

    pub(crate) fn append<'cl>(
        &self,
        unfinalized: impl Iterator<Item = &'cl ChainLink<P>>,
        finalized: impl DoubleEndedIterator<Item = &'cl ChainLink<P>>,
        store: &Store<P, Self>,
    ) -> Result<AppendedBlockSlots> {
        let mut slots = AppendedBlockSlots::default();
        let mut store_head_slot = 0;
        let mut checkpoint_state_appended = false;
        let mut batch = vec![];

        let finalized_validators = store.finalized_validators();

        let unfinalized = unfinalized.zip(core::iter::repeat(false));
        let finalized = finalized.rev().zip(core::iter::repeat(true));

        let mut chain = unfinalized
            .chain(finalized)
            .filter(|(chain_link, is_finalized)| *is_finalized || chain_link.is_valid())
            .peekable();

        if let Some(StateCheckpoint { head_slot, .. }) =
            self.load_state_checkpoint(Some(&finalized_validators))?
        {
            store_head_slot = head_slot;
        }

        if let Some((chain_link, _)) = chain.peek() {
            store_head_slot = chain_link.slot().max(store_head_slot);
        }

        debug_with_peers!("saving store head slot: {store_head_slot}");

        let mut update_finalized_validators = false;

        for (chain_link, finalized) in chain {
            let block_root = chain_link.block_root;
            let block = &chain_link.block;
            let state_slot = chain_link.slot();

            if !self.prune_storage_enabled() {
                if finalized && !self.contains_finalized_block(block_root)? {
                    slots.finalized.push(state_slot);
                    batch.push(serialize(FinalizedBlockByRoot(block_root), block)?);
                } else if !self.contains_unfinalized_block(block_root)? {
                    slots.unfinalized.push(state_slot);
                    batch.push(serialize(UnfinalizedBlockByRoot(block_root), block)?);
                }

                batch.push(serialize(BlockRootBySlot(state_slot), block_root)?);
            }

            if finalized {
                if !self.prune_storage_enabled() {
                    batch.push(serialize(
                        SlotByStateRoot(block.message().state_root()),
                        state_slot,
                    )?);
                }

                let state = OnceCell::new();
                let is_epoch_start = misc::is_epoch_start::<P>(state_slot);

                let (relative_slot, use_anchor) = state_slot
                    .checked_sub(self.anchor_slot.load(Ordering::SeqCst))
                    .map(|slot| (slot, true))
                    .unwrap_or((state_slot, false));

                if !checkpoint_state_appended
                    && ((store.is_forward_synced() && is_epoch_start)
                        || self.hierarchy.contains::<P>(relative_slot))
                {
                    info_with_peers!("saving checkpoint block & state in slot {state_slot}");

                    batch.push(serialize(
                        BlockCheckpoint::<P>::KEY,
                        BlockCheckpoint {
                            block: block.clone_arc(),
                        },
                    )?);

                    batch.push(serialize(
                        StateCheckpoint::<P>::KEY,
                        StateCheckpoint {
                            block_root,
                            head_slot: store_head_slot,
                            state: prepare_state(
                                state.get_or_init(|| chain_link.state(store)).clone_arc(),
                                finalized_validators.len_usize(),
                            ),
                        },
                    )?);

                    checkpoint_state_appended = true;
                    update_finalized_validators = true;
                }

                if !self.prune_storage_enabled() && self.hierarchy.contains::<P>(relative_slot) {
                    info_with_peers!("saving state in slot {state_slot}");

                    let full_state = state.get_or_init(|| chain_link.state(store)).clone_arc();

                    update_finalized_validators = true;

                    let Some(parent) = self.hierarchy.parent_of::<P>(relative_slot) else {
                        // Referential frame: zeroize the finalized pubkeys for
                        // storage only; the in-memory frame keeps them intact.
                        batch.push(serialize(
                            StateByBlockRoot::snapshot(block_root),
                            &prepare_state(full_state, finalized_validators.len_usize()),
                        )?);
                        continue;
                    };

                    let parent_absolute = parent
                        + use_anchor
                            .then(|| self.anchor_slot.load(Ordering::SeqCst))
                            .unwrap_or_default();

                    let Some(parent_block_root) = self.block_root_by_slot(parent_absolute)? else {
                        todo!("put a warning & save full snapshot in here, if it is not a leaf");
                    };

                    // TODO: the chain states are being processed from newest to oldest one. In an unlikely event, we
                    // may reference the state, that was not yet persisted. Due to this, we won't be able to compute
                    // diff properly.
                    let Some((parent_key, parent_state)) = self.state_with_key_by_block_root(
                        parent_block_root,
                        Some(&finalized_validators),
                    )?
                    else {
                        todo!("put a warning & save full snapshot in here, if it is not a leaf");
                    };

                    // Diffs are always computed between full (non-zeroized) states.
                    let diff = BeaconStatePatch::diff(&parent_state, &full_state)?;

                    batch.push(serialize(parent_key.extend_chain(block_root), diff)?);
                }
            }
        }

        if update_finalized_validators {
            self.append_finalized_validator_pubkeys_to_batch(&mut batch, &finalized_validators)?;
        }

        self.database.put_batch(batch)?;

        Ok(slots)
    }

    pub(crate) fn append_blob_sidecars(
        &self,
        blob_sidecars: impl IntoIterator<Item = BlobSidecarWithId<P>>,
    ) -> Result<Vec<BlobIdentifier>> {
        let mut batch = vec![];
        let mut persisted_blob_ids = vec![];

        for blob_sidecar_with_id in blob_sidecars {
            let BlobSidecarWithId {
                blob_sidecar,
                blob_id,
            } = blob_sidecar_with_id;

            let BlobIdentifier { block_root, index } = blob_id;

            let slot = blob_sidecar.signed_block_header.message.slot;

            batch.push(serialize(
                BlobSidecarByBlobId(block_root, index),
                blob_sidecar,
            )?);

            batch.push(serialize(SlotBlobId(slot, block_root, index), blob_id)?);

            persisted_blob_ids.push(blob_id);
        }

        self.database.put_batch(batch)?;

        Ok(persisted_blob_ids)
    }

    pub(crate) fn append_states(
        &self,
        states_with_block_roots: impl Iterator<Item = (Arc<BeaconState<P>>, H256)>,
        finalized_validators: &Validators<P>,
    ) -> Result<Vec<Slot>> {
        let mut slots = vec![];
        let mut batch = vec![];
        let mut update_finalized_validators = false;

        for (state, block_root) in states_with_block_roots {
            if !self.contains_prefixed_key(StateByBlockRoot::snapshot(block_root))? {
                let archival_state = state.clone_arc();

                // slots.push(state.slot());
                // batch.push(serialize(
                //     StateByBlockRoot(block_root),
                //     prepare_state(archival_state, finalized_validators.len_usize()),
                // )?);

                update_finalized_validators = true;
            }
        }

        if update_finalized_validators {
            self.append_finalized_validator_pubkeys_to_batch(&mut batch, finalized_validators)?;
        }

        self.database.put_batch(batch)?;

        Ok(slots)
    }

    pub(crate) fn blob_sidecar_by_id(
        &self,
        blob_id: BlobIdentifier,
    ) -> Result<Option<Arc<BlobSidecar<P>>>> {
        let BlobIdentifier { block_root, index } = blob_id;

        self.get(BlobSidecarByBlobId(block_root, index))
    }

    pub(crate) fn prune_old_blob_sidecars(&self, up_to_slot: Slot) -> Result<()> {
        let results = self
            .database
            .iterator_descending(..=SlotBlobId(up_to_slot, H256::zero(), 0).to_string())?;

        let (mut keys_to_remove, blobs_to_remove): (Vec<_>, Vec<_>) =
            itertools::process_results(results, |iter| {
                iter.take_while(|(key_bytes, _)| SlotBlobId::has_prefix(key_bytes))
                    .map(|(k, v)| (k.into_owned(), v))
                    .unzip()
            })?;

        for blob_bytes in blobs_to_remove {
            let BlobIdentifier { block_root, index } =
                BlobIdentifier::from_ssz_default(blob_bytes)?;

            keys_to_remove.push(BlobSidecarByBlobId(block_root, index).to_string().into());
        }

        self.database.delete_batch(keys_to_remove)
    }

    pub(crate) fn prune_old_blocks_and_states(&self, up_to_slot: Slot) -> Result<()> {
        let results = self
            .database
            .iterator_descending(..=BlockRootBySlot(up_to_slot.saturating_sub(1)).to_string())?;

        let (mut keys_to_remove, block_roots_to_remove): (Vec<_>, Vec<_>) =
            itertools::process_results(results, |iter| {
                iter.take_while(|(key_bytes, _)| BlockRootBySlot::has_prefix(key_bytes))
                    .map(|(k, v)| (k.into_owned(), v))
                    .unzip()
            })?;

        for block_root_bytes in block_roots_to_remove {
            let block_root = H256::from_ssz_default(block_root_bytes)?;

            keys_to_remove.push(FinalizedBlockByRoot(block_root).to_string().into());
            // keys_to_remove.push(StateByBlockRoot(block_root).to_string().into());
        }

        self.database.delete_batch(keys_to_remove)
    }

    pub(crate) fn prune_old_state_roots(&self, up_to_slot: Slot) -> Result<()> {
        let mut keys_to_remove = vec![];

        let results = self
            .database
            .iterator_ascending(SlotByStateRoot(H256::zero()).to_string()..)?;

        let results = itertools::process_results(results, |iter| {
            iter.take_while(|(key_bytes, _)| SlotByStateRoot::has_prefix(key_bytes))
                .map(|(k, v)| (k.into_owned(), v))
                .collect::<Vec<_>>()
        })?;

        for (key_bytes, value_bytes) in results {
            let slot = Slot::from_ssz_default(value_bytes)?;

            if slot < up_to_slot {
                keys_to_remove.push(key_bytes);
            }
        }

        self.database.delete_batch(keys_to_remove)
    }

    pub(crate) fn prune_unfinalized_blocks(&self, last_finalized_slot: Slot) -> Result<Vec<Slot>> {
        let mut slots = vec![];
        let mut keys_to_remove = vec![];

        let results = self
            .database
            .iterator_ascending(serialize_key(UnfinalizedBlockByRoot(H256::zero()))..)?;

        let results = itertools::process_results(results, |iter| {
            iter.take_while(|(key_bytes, _)| UnfinalizedBlockByRoot::has_prefix(key_bytes))
                .map(|(k, v)| (k.into_owned(), v))
                .collect::<Vec<_>>()
        })?;

        for (key_bytes, value_bytes) in results {
            let unfinalized_block = SignedBeaconBlock::<P>::from_ssz(&self.config, value_bytes)?;
            let block_slot = unfinalized_block.message().slot();

            if block_slot <= last_finalized_slot {
                slots.push(block_slot);
                keys_to_remove.push(key_bytes);
            }
        }

        for slot in &slots {
            if let Some(block_root) = self.block_root_by_slot(*slot)? {
                // remove only if slot -> root points to unfinalized block
                if !self.contains_finalized_block(block_root)? {
                    keys_to_remove
                        .push(serialize_key(BlockRootBySlot(*slot)).as_bytes().to_owned());
                }
            }
        }

        self.database.delete_batch(keys_to_remove)?;

        Ok(slots)
    }

    pub(crate) fn append_data_column_sidecars(
        &self,
        data_column_sidecars: impl IntoIterator<Item = DataColumnSidecarWithId<P>>,
    ) -> Result<Vec<DataColumnIdentifier>> {
        let mut batch = vec![];
        let mut persisted_data_column_ids = vec![];

        for data_column_sidecar_with_id in data_column_sidecars {
            let DataColumnSidecarWithId {
                data_column_sidecar,
                data_column_id,
            } = data_column_sidecar_with_id;

            let DataColumnIdentifier { block_root, index } = data_column_id;

            let slot = data_column_sidecar.slot();

            batch.push(serialize(
                DataColumnSidecarByColumnId(block_root, index),
                data_column_sidecar,
            )?);

            batch.push(serialize(
                SlotColumnId(slot, block_root, index),
                data_column_id,
            )?);

            persisted_data_column_ids.push(data_column_id);
        }

        self.database.put_batch(batch)?;

        Ok(persisted_data_column_ids)
    }

    pub(crate) fn data_column_sidecar_by_id(
        &self,
        data_column_id: DataColumnIdentifier,
    ) -> Result<Option<Arc<DataColumnSidecar<P>>>> {
        let DataColumnIdentifier { block_root, index } = data_column_id;

        self.get(DataColumnSidecarByColumnId(block_root, index))
    }

    pub(crate) fn prune_old_data_column_sidecars(&self, up_to_slot: Slot) -> Result<()> {
        let results = self
            .database
            .iterator_descending(..=SlotColumnId(up_to_slot, H256::zero(), 0).to_string())?;

        let (mut keys_to_remove, columns_to_remove): (Vec<_>, Vec<_>) =
            itertools::process_results(results, |iter| {
                iter.take_while(|(key_bytes, _)| SlotColumnId::has_prefix(key_bytes))
                    .take(
                        // Limit number of entries to prune per single transaction
                        MAX_DATA_COLUMN_EPOCHS_TO_PRUNE
                            .saturating_mul(P::SlotsPerEpoch::USIZE)
                            .saturating_mul(P::NumberOfColumns::USIZE),
                    )
                    .map(|(k, v)| (k.into_owned(), v))
                    .unzip()
            })?;

        for column_bytes in columns_to_remove {
            let DataColumnIdentifier { block_root, index } =
                DataColumnIdentifier::from_ssz_default(column_bytes)?;

            keys_to_remove.push(
                DataColumnSidecarByColumnId(block_root, index)
                    .to_string()
                    .into(),
            )
        }

        self.database.delete_batch(keys_to_remove)
    }

    pub(crate) fn checkpoint_state_slot(
        &self,
        finalized_validators: &Validators<P>,
    ) -> Result<Option<Slot>> {
        if let Some(StateCheckpoint { head_slot, .. }) =
            self.load_state_checkpoint(Some(finalized_validators))?
        {
            return Ok(Some(head_slot));
        }

        Ok(None)
    }

    pub(crate) fn genesis_block_root(&self, store: &Store<P, Self>) -> Result<H256> {
        self.block_root_by_slot_with_store(store, GENESIS_SLOT)?
            .ok_or(Error::GenesisBlockRootNotFound)
            .map_err(Into::into)
    }

    pub(crate) fn contains_finalized_block(&self, block_root: H256) -> Result<bool> {
        self.contains_key(FinalizedBlockByRoot(block_root))
    }

    pub(crate) fn contains_unfinalized_block(&self, block_root: H256) -> Result<bool> {
        self.contains_key(UnfinalizedBlockByRoot(block_root))
    }

    pub(crate) fn finalized_block_by_root(
        &self,
        block_root: H256,
    ) -> Result<Option<Arc<SignedBeaconBlock<P>>>> {
        self.get(FinalizedBlockByRoot(block_root))
    }

    pub(crate) fn unfinalized_block_by_root(
        &self,
        block_root: H256,
    ) -> Result<Option<Arc<SignedBeaconBlock<P>>>> {
        self.get(UnfinalizedBlockByRoot(block_root))
    }

    pub(crate) fn block_root_by_slot(&self, slot: Slot) -> Result<Option<H256>> {
        self.get(BlockRootBySlot(slot))
    }

    #[tracing::instrument(skip_all, fields(block_root = %block_root))]
    fn state_with_key_by_block_root(
        &self,
        block_root: H256,
        finalized_validators: Option<&Validators<P>>,
    ) -> Result<Option<(StateByBlockRoot, Arc<BeaconState<P>>)>> {
        let span = span!(Level::INFO, "load_first", block_root = %block_root, key = field::Empty, parents = field::Empty).entered();

        let prefix = StateByBlockRoot {
            block_root,
            parents: Vec::new(),
        }
        .to_string();

        // Fetch only the key first. If this resolves to an already-cached referential frame we can
        // return it without ever reading or decompressing the (large) snapshot value from disk,
        // which would otherwise waste ~170ms on a guaranteed cache hit.
        let Some(full_key) = self.database.next_key(&prefix)? else {
            return Ok(None);
        };
        span.record("key", String::from_utf8(full_key.clone()).unwrap());

        let Some(key) = full_key.strip_prefix(prefix.as_bytes()) else {
            return Ok(None);
        };

        let parents_iter = key.chunks_exact(64);
        ensure!(parents_iter.remainder().is_empty(), "invalid state key");
        let parents = parents_iter
            .map(|root| {
                let root_str = str::from_utf8(root)?;
                let mut root = H256::default();

                hex::decode_to_slice(root_str, &mut root.0)?;
                Ok(root)
            })
            .collect::<Result<Vec<_>>>()?;

        span.record("parents", format!("{parents:?}"));
        span.exit();

        if parents.is_empty()
            && let Some(cached_frame) = self.frame_cache.lock().cache_get(&block_root)
        {
            let _span = span!(Level::INFO, "frame_cache_hit", block_root = %block_root).entered();

            return Ok(Some((
                StateByBlockRoot {
                    block_root,
                    parents,
                },
                cached_frame.clone(),
            )));
        }

        // Not a cached frame: now read and decompress the value addressed by the key we found.
        let value = {
            let load_value_span =
                span!(Level::INFO, "load_first_value", bytes = field::Empty).entered();
            let Some(value) = self.database.get(&full_key)? else {
                return Ok(None);
            };
            load_value_span.record("bytes", value.len());
            value
        };

        let mut items = vec![value];
        let mut frame = None;

        for &block_root in parents.iter() {
            if let Some(cached_frame) = self.frame_cache.lock().cache_get(&block_root) {
                frame = Some(cached_frame.clone());
                break;
            }

            let _span = span!(Level::INFO, "load_next", block_root = %block_root).entered();
            let search_key = StateByBlockRoot {
                block_root,
                parents: Vec::new(),
            }
            .to_string();
            let Some((key, value)) = self.database.next(&search_key)? else {
                bail!("unable to reconstruct state, as it references non-existent state");
            };

            if !key.starts_with(search_key.as_bytes()) {
                bail!("unable to reconstruct state, as it references non-existent state");
            }

            items.push(value);
        }

        items.reverse();

        let (mut frame, deltas) = if let Some(frame) = frame {
            (frame, items.as_slice())
        } else {
            let Some((frame_bytes, deltas)) = items.split_first() else {
                unreachable!("items cannot be empty")
            };
            let frame_block_root = parents.last().copied().unwrap_or(block_root);

            let span = span!(Level::INFO, "deserialize frame").entered();
            let mut frame = Arc::<BeaconState<P>>::from_ssz(&self.config, frame_bytes)?;
            span.exit();

            // The frame snapshot is stored with its finalized pubkeys zeroized to
            // shrink it on disk. Restore them before caching so the cache holds a
            // full referential frame and every diff is applied on a normal state.
            self.restore_validators_to_state(frame.make_mut(), finalized_validators)?;

            // Seed the frame's cached root here, at the single point where a frame enters
            // the cache (this branch only runs on a cache miss). The frame is the post-state
            // of `frame_block_root`, so its root is that block's `state_root`. Doing it once,
            // now, keeps every cached frame self-contained: later cache hits return an
            // already-rooted state, and `stored_state` never re-sets the root on the shared
            // `Arc` (which would panic, since the root lives in a write-once cell).
            if let Some(block) = self.finalized_block_by_root(frame_block_root)? {
                frame.set_cached_root(block.message().state_root());
            }

            self.frame_cache
                .lock()
                .cache_set(frame_block_root, frame.clone());

            (frame, deltas)
        };

        for delta in deltas {
            let _span = span!(Level::INFO, "apply_delta", block_root = %block_root).entered();
            let patch = {
                let _span = span!(Level::INFO, "apply_delta::from_ssz").entered();
                BeaconStatePatch::from_ssz(&self.config, delta)?
            };
            frame = {
                let _span = span!(Level::INFO, "apply_delta::apply").entered();
                patch.apply(frame)?
            };
        }

        Ok(Some((
            StateByBlockRoot {
                block_root,
                parents,
            },
            frame,
        )))
    }

    #[tracing::instrument(skip_all, fields(block_root = %block_root))]
    fn state_by_block_root(
        &self,
        block_root: H256,
        finalized_validators: Option<&Validators<P>>,
    ) -> Result<Option<Arc<BeaconState<P>>>> {
        // Pubkeys are zeroized/restored only for referential, full-state frames.
        // `state_with_key_by_block_root` already restores the finalized pubkeys on
        // the referential frame before caching it and applying any diffs (the diffs
        // are computed between full states), so the returned state always has its
        // pubkeys intact. Restoring again here would needlessly `make_mut` (deep
        // clone) shared cached frames and apply a frame-level step to delta-derived
        // states.
        Ok(self
            .state_with_key_by_block_root(block_root, finalized_validators)?
            .map(|(_, state)| state))
    }

    pub(crate) fn slot_by_state_root(&self, state_root: H256) -> Result<Option<Slot>> {
        self.get(SlotByStateRoot(state_root))
    }

    // Like `block_root_by_slot`, but looks for the root in `store` first.
    pub(crate) fn block_root_by_slot_with_store(
        &self,
        store: &Store<P, Self>,
        slot: Slot,
    ) -> Result<Option<H256>> {
        if let Some(chain_link) = store.chain_link_before_or_at(slot) {
            let slot_matches = chain_link.slot() == slot;
            return Ok(slot_matches.then_some(chain_link.block_root));
        }

        self.block_root_by_slot(slot)
    }

    pub(crate) fn block_root_before_or_at_slot(&self, slot: Slot) -> Result<Option<H256>> {
        let results = self
            .database
            .iterator_descending(..=BlockRootBySlot(slot).to_string())?;

        itertools::process_results(results, |pairs| {
            pairs
                .take_while(|(key_bytes, _)| BlockRootBySlot::has_prefix(key_bytes))
                .map(|(_, value_bytes)| H256::from_ssz_default(value_bytes))
                .next()
                .transpose()
        })?
        .map_err(Into::into)
    }

    pub(crate) fn finalized_block_by_slot(
        &self,
        slot: Slot,
    ) -> Result<Option<(Arc<SignedBeaconBlock<P>>, H256)>> {
        let Some(block_root) = self.block_root_by_slot(slot)? else {
            return Ok(None);
        };

        let Some(block) = self.finalized_block_by_root(block_root)? else {
            return Ok(None);
        };

        Ok(Some((block, block_root)))
    }

    #[tracing::instrument(
        skip_all,
        fields(slot = %slot, base_slot = field::Empty, blocks_replayed = field::Empty),
    )]
    pub(crate) fn stored_state(
        &self,
        slot: Slot,
        finalized_validators: Option<&Validators<P>>,
    ) -> Result<Option<Arc<BeaconState<P>>>> {
        // Loading the base state reconstructs a referential frame and replays the stored diffs onto
        // it (see `state_with_key_by_block_root`); this is the dominant cost of the first call after
        // crossing into a new frame.
        // The base state's root is seeded once, when its referential frame enters the cache
        // (see `state_with_key_by_block_root`), so the block here is no longer needed to set it.
        // It is intentionally not re-set on this path: the frame is shared via the cache and its
        // root lives in a write-once cell, so a second `set_cached_root` on a cache hit would
        // panic. Any diffs replayed below clone the state and invalidate the cached root anyway.
        let (mut state, _state_block, blocks) = {
            let _span = span!(Level::INFO, "load_base_state", %slot).entered();

            match self.load_state_by_iteration(slot, finalized_validators)? {
                OptionalStateStorage::None | OptionalStateStorage::UnfinalizedOnly(_) => {
                    return Ok(None);
                }
                OptionalStateStorage::Full(state_storage) => state_storage,
            }
        };

        Span::current().record("base_slot", state.slot());

        // The base state was reconstructed from diffs, so its Merkle cache is cold. The first
        // `process_slot` of the replay below would otherwise rebuild the entire state tree on one
        // thread (dominated by the per-validator lists). Warm the largest fields' caches in
        // parallel first; `make_mut` preserves the cached roots when the replay clones the state,
        // so the first `hash_tree_root` only has to recombine already-hashed subtrees.
        {
            let _span = span!(Level::INFO, "prewarm_state_hash_caches", %slot).entered();
            prewarm_state_hash_caches(&state);
        }

        // State may be persisted only once in several epochs.
        // `blocks` here are needed to transition state closer to `slot`.
        let mut blocks_replayed = 0_u64;

        for result in blocks.rev() {
            let block = result?;
            let block_slot = block.message().slot();
            let is_epoch_start = misc::is_epoch_start::<P>(block_slot);
            let _span =
                span!(Level::INFO, "state_transition", %block_slot, is_epoch_start).entered();

            let state = {
                // The first replayed block forces a deep copy-on-write clone of the loaded state,
                // which can dominate this span for large states.
                let _span = span!(Level::DEBUG, "state_make_mut", %block_slot).entered();
                state.make_mut()
            };

            combined::trusted_state_transition(&self.config, &self.pubkey_cache, state, &block)?;

            blocks_replayed += 1;
        }

        Span::current().record("blocks_replayed", blocks_replayed);

        if state.slot() < slot {
            let _span =
                span!(Level::INFO, "process_empty", from = %state.slot(), to = %slot).entered();
            combined::process_slots(&self.config, &self.pubkey_cache, state.make_mut(), slot)?;
        }

        Ok(Some(state))
    }

    pub(crate) fn state_post_block(
        &self,
        mut block_root: H256,
        finalized_validators: &Validators<P>,
    ) -> Result<Option<Arc<BeaconState<P>>>> {
        let mut blocks = vec![];

        let mut state = loop {
            if let Some(state) = self.state_by_block_root(block_root, Some(finalized_validators))? {
                let slot = state.slot();

                ensure!(
                    misc::is_epoch_start::<P>(slot),
                    Error::PersistedSlotCannotContainAnchor { slot },
                );

                break state;
            }

            if let Some(block) = self.finalized_block_by_root(block_root)? {
                block_root = block.message().parent_root();
                blocks.push(block);
                continue;
            }

            if let Some(block) = self.unfinalized_block_by_root(block_root)? {
                block_root = block.message().parent_root();
                blocks.push(block);
                continue;
            }

            return Ok(None);
        };

        for block in blocks.into_iter().rev() {
            combined::trusted_state_transition(
                &self.config,
                &self.pubkey_cache,
                state.make_mut(),
                &block,
            )?;
        }

        Ok(Some(state))
    }

    pub(crate) fn stored_state_by_state_root(
        &self,
        state_root: H256,
        finalized_validators: &Validators<P>,
    ) -> Result<Option<Arc<BeaconState<P>>>> {
        if let Some(state_slot) = self.slot_by_state_root(state_root)? {
            return self.stored_state(state_slot, Some(finalized_validators));
        }

        Ok(None)
    }

    pub(crate) fn dependent_root(
        &self,
        store: &Store<P, Self>,
        state: &BeaconState<P>,
        epoch: Epoch,
    ) -> Result<H256> {
        let start_slot = misc::compute_start_slot_at_epoch::<P>(epoch);

        match start_slot.checked_sub(1) {
            Some(root_slot) => accessors::get_block_root_at_slot(state, root_slot),
            None => self.genesis_block_root(store),
        }
        .context(Error::DependentRootLookupFailed)
    }

    fn load_state_and_blocks_from_checkpoint(
        &self,
        finalized_validators: Option<&Validators<P>>,
    ) -> Result<Option<StateStorage<'_, P>>> {
        if let Some(checkpoint) = self.load_state_checkpoint(finalized_validators)? {
            let StateCheckpoint {
                block_root, state, ..
            } = checkpoint;

            let block = if let Some(block_checkpoint) = self.load_block_checkpoint()? {
                let BlockCheckpoint { block } = block_checkpoint;
                let requested = block_root;
                let computed = block.message().hash_tree_root();

                ensure!(
                    requested == computed,
                    Error::CheckpointBlockRootMismatch {
                        requested,
                        computed,
                    },
                );

                block
            } else {
                self.finalized_block_by_root(block_root)?
                    .ok_or(Error::BlockNotFound { block_root })?
            };

            ensure!(
                misc::is_epoch_start::<P>(state.slot()),
                Error::PersistedSlotCannotContainAnchor { slot: state.slot() },
            );

            let results = self.database.iterator_ascending(
                BlockRootBySlot(state.slot().saturating_add(1)).to_string()..,
            )?;

            let block_roots = itertools::process_results(results, |pairs| {
                pairs
                    .take_while(|(key_bytes, _)| BlockRootBySlot::has_prefix(key_bytes))
                    .map(|(_, value_bytes)| H256::from_ssz_default(value_bytes))
                    .try_collect()
            })??;

            let blocks = self.blocks_by_roots(block_roots);

            return Ok(Some((state, block, blocks)));
        }

        Ok(None)
    }

    #[tracing::instrument(skip_all, fields(start_from_slot = %start_from_slot))]
    fn load_state_by_iteration(
        &self,
        start_from_slot: Slot,
        finalized_validators: Option<&Validators<P>>,
    ) -> Result<OptionalStateStorage<'_, P>> {
        let results = self
            .database
            .iterator_descending(..=BlockRootBySlot(start_from_slot).to_string())?;

        let mut block_roots = vec![];

        // Walk descending from `start_from_slot` only until the first persisted snapshot is found.
        // The iterator is lazy, so breaking out here avoids reading and deserializing every block
        // root all the way down to genesis: the base state is usually within one epoch, so only a
        // handful of entries are touched instead of the entire `BlockRootBySlot` keyspace.
        for result in results {
            let (key_bytes, value_bytes) = result?;

            // The descending iterator keeps going past the `BlockRootBySlot` keyspace into other
            // prefixes; stop once we leave it (equivalent to the old `take_while`).
            if !BlockRootBySlot::has_prefix(&key_bytes) {
                break;
            }

            let block_root = H256::from_ssz_default(value_bytes)?;

            if self.contains_prefixed_key(StateByBlockRoot::snapshot(block_root))? {
                let Some(block) = self.finalized_block_by_root(block_root)? else {
                    // States are also persisted from unfinalized chain
                    continue;
                };

                if let Some(state) = self.state_by_block_root(block_root, finalized_validators)? {
                    let slot = state.slot();

                    ensure!(
                        misc::is_epoch_start::<P>(slot),
                        Error::PersistedSlotCannotContainAnchor { slot },
                    );

                    let blocks = self.blocks_by_roots(block_roots);

                    return Ok(OptionalStateStorage::Full((state, block, blocks)));
                }
            }

            block_roots.push(block_root);
        }

        if block_roots.is_empty() {
            return Ok(OptionalStateStorage::None);
        }

        Ok(OptionalStateStorage::UnfinalizedOnly(
            self.blocks_by_roots(block_roots),
        ))
    }

    fn load_block_checkpoint(&self) -> Result<Option<BlockCheckpoint<P>>> {
        self.get(BlockCheckpoint::<P>::KEY)
    }

    fn load_state_checkpoint(
        &self,
        finalized_validators: Option<&Validators<P>>,
    ) -> Result<Option<StateCheckpoint<P>>> {
        let Some(mut checkpoint) = self.get::<StateCheckpoint<P>>(StateCheckpoint::<P>::KEY)?
        else {
            return Ok(None);
        };

        // Restore validators if they were removed
        self.restore_validators_to_state(checkpoint.state.make_mut(), finalized_validators)?;

        Ok(Some(checkpoint))
    }

    fn contains_prefixed_key(&self, key: impl Display) -> Result<bool> {
        let key_string = key.to_string();

        self.database.contains_prefixed_key(key_string)
    }

    fn contains_key(&self, key: impl Display) -> Result<bool> {
        let key_string = key.to_string();

        self.database.contains_key(key_string)
    }

    fn get<V: SszRead<Config>>(&self, key: impl Display) -> Result<Option<V>> {
        let key_string = key.to_string();

        if let Some(value_bytes) = self.database.get(key_string)? {
            let value = V::from_ssz(&self.config, value_bytes)?;
            return Ok(Some(value));
        }

        Ok(None)
    }

    fn blocks_by_roots(&self, block_roots: Vec<H256>) -> UnfinalizedBlocks<'_, P> {
        Box::new(block_roots.into_iter().map(|block_root| {
            if let Some(block) = self.finalized_block_by_root(block_root)? {
                return Ok(block);
            }

            if let Some(block) = self.unfinalized_block_by_root(block_root)? {
                return Ok(block);
            }

            bail!(Error::BlockNotFound { block_root })
        }))
    }

    pub(crate) fn epoch_at_slot(slot: Slot) -> Epoch {
        misc::compute_epoch_at_slot::<P>(slot)
    }

    #[tracing::instrument(skip_all)]
    fn restore_validators_to_state(
        &self,
        state: &mut BeaconState<P>,
        finalized_validators: Option<&Validators<P>>,
    ) -> Result<()> {
        match finalized_validators {
            Some(validators) => {
                state
                    .validators_mut()
                    .set_pubkeys(validators.pubkeys())
                    .context("invalid finalized validators list")?;
            }
            None => {
                info_with_peers!("loading validators from disk");

                let Some(pubkeys) =
                    self.get::<PubkeyList<P::ValidatorRegistryLimit>>(FinalizedValidators)?
                else {
                    bail!(
                        "unable to restore validators into state - no saved validators on disk found."
                    );
                };

                state
                    .validators_mut()
                    .set_pubkeys(&pubkeys)
                    .context("invalid finalized validators list loaded from disk")?;
            }
        }

        Ok(())
    }

    fn append_finalized_validator_pubkeys_to_batch(
        &self,
        batch: &mut Vec<(String, Vec<u8>)>,
        validators: &Validators<P>,
    ) -> Result<()> {
        let current_validator_count = self.get::<u64>(FinalizedValidatorCount)?.unwrap_or(0);

        if validators.len_u64() <= current_validator_count {
            return Ok(());
        }

        batch.extend_from_slice(&[
            serialize(FinalizedValidatorCount, validators.len_u64())?,
            serialize(FinalizedValidators, validators.pubkeys())?,
        ]);

        Ok(())
    }
}

#[cfg(test)]
impl<P: Preset> Storage<P> {
    pub fn block_root_by_slot_count(&self) -> Result<usize> {
        let results = self
            .database
            .iterator_ascending(BlockRootBySlot(0).to_string()..)?;

        itertools::process_results(results, |pairs| {
            pairs
                .take_while(|(key_bytes, _)| BlockRootBySlot::has_prefix(key_bytes))
                .count()
        })
    }

    pub fn finalized_block_count(&self) -> Result<usize> {
        let results = self
            .database
            .iterator_ascending(FinalizedBlockByRoot(H256::zero()).to_string()..)?;

        itertools::process_results(results, |pairs| {
            pairs
                .take_while(|(key_bytes, _)| FinalizedBlockByRoot::has_prefix(key_bytes))
                .filter(|(key_bytes, _)| !UnfinalizedBlockByRoot::has_prefix(key_bytes))
                .count()
        })
    }

    pub fn unfinalized_block_count(&self) -> Result<usize> {
        let results = self
            .database
            .iterator_ascending(UnfinalizedBlockByRoot(H256::zero()).to_string()..)?;

        itertools::process_results(results, |pairs| {
            pairs
                .take_while(|(key_bytes, _)| UnfinalizedBlockByRoot::has_prefix(key_bytes))
                .count()
        })
    }

    pub fn slot_by_state_root_count(&self) -> Result<usize> {
        let results = self
            .database
            .iterator_ascending(SlotByStateRoot(H256::zero()).to_string()..)?;

        itertools::process_results(results, |pairs| {
            pairs
                .take_while(|(key_bytes, _)| SlotByStateRoot::has_prefix(key_bytes))
                .count()
        })
    }

    pub fn slot_by_blob_id_count(&self) -> Result<usize> {
        let results = self
            .database
            .iterator_ascending(SlotBlobId(0, H256::zero(), 0).to_string()..)?;

        itertools::process_results(results, |pairs| {
            pairs
                .take_while(|(key_bytes, _)| SlotBlobId::has_prefix(key_bytes))
                .count()
        })
    }

    pub fn state_count(&self) -> Result<usize> {
        let results = self
            .database
            .iterator_ascending(StateByBlockRoot::snapshot(H256::zero()).to_string()..)?;

        let frame_count = itertools::process_results(results, |pairs| {
            pairs
                .take_while(|(key_bytes, _)| StateByBlockRoot::has_prefix(key_bytes))
                .count()
        })?;

        Ok(frame_count)
    }

    pub fn blob_sidecar_by_blob_id_count(&self) -> Result<usize> {
        let results = self
            .database
            .iterator_ascending(BlobSidecarByBlobId(H256::zero(), 0).to_string()..)?;

        itertools::process_results(results, |pairs| {
            pairs
                .take_while(|(key_bytes, _)| BlobSidecarByBlobId::has_prefix(key_bytes))
                .count()
        })
    }
}

impl<P: Preset> fork_choice_store::Storage<P> for Storage<P> {
    fn storage_mode(&self) -> StorageMode {
        self.storage_mode
    }

    fn stored_state_by_block_root(
        &self,
        block_root: H256,
        finalized_validators: Option<&Validators<P>>,
    ) -> Result<Option<Arc<BeaconState<P>>>> {
        self.state_by_block_root(block_root, finalized_validators)
    }
}

#[derive(Default, Debug)]
pub struct AppendedBlockSlots {
    pub finalized: Vec<Slot>,
    pub unfinalized: Vec<Slot>,
}

type UnfinalizedBlocks<'storage, P> =
    Box<dyn DoubleEndedIterator<Item = Result<Arc<SignedBeaconBlock<P>>>> + Send + 'storage>;

// Internal type for state storage that can be missing or have missing elements.
// E.g. non-finalized storage that has only unfinalized blocks stored.
enum OptionalStateStorage<'storage, P: Preset> {
    None,
    UnfinalizedOnly(UnfinalizedBlocks<'storage, P>),
    Full(StateStorage<'storage, P>),
}

impl<P: Preset> OptionalStateStorage<'_, P> {
    const fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }
}

type StateStorage<'storage, P> = (
    Arc<BeaconState<P>>,
    Arc<SignedBeaconBlock<P>>,
    UnfinalizedBlocks<'storage, P>,
);

#[derive(Ssz)]
// A `bound_for_read` attribute like this must be added when deriving `SszRead` for any type that
// contains a block or state. The name of the `C` type parameter is hardcoded in `ssz_derive`.
#[ssz(bound_for_read = "BeaconState<P>: SszRead<C>", derive_hash = false)]
pub struct StateCheckpoint<P: Preset> {
    block_root: H256,
    head_slot: Slot,
    state: Arc<BeaconState<P>>,
}

impl<P: Preset> PrefixableKey for StateCheckpoint<P> {
    const PREFIX: &'static str = Self::KEY;
}

impl<P: Preset> StateCheckpoint<P> {
    // This was renamed from `cstate` for compatibility with old schema versions.
    const KEY: &'static str = "cstate2";
}

#[derive(Ssz)]
// A `bound_for_read` attribute like this must be added when deriving `SszRead` for any type that
// contains a block or state. The name of the `C` type parameter is hardcoded in `ssz_derive`.
#[ssz(
    bound_for_read = "SignedBeaconBlock<P>: SszRead<C>",
    derive_hash = false,
    transparent
)]
pub struct BlockCheckpoint<P: Preset> {
    block: Arc<SignedBeaconBlock<P>>,
}

impl<P: Preset> PrefixableKey for BlockCheckpoint<P> {
    const PREFIX: &'static str = Self::KEY;
}

impl<P: Preset> BlockCheckpoint<P> {
    const KEY: &'static str = "cblock";
}

#[derive(Display)]
#[display("{}{_0:020}", Self::PREFIX)]
pub struct BlockRootBySlot(pub Slot);

impl TryFrom<Cow<'_, [u8]>> for BlockRootBySlot {
    type Error = AnyhowError;

    fn try_from(bytes: Cow<[u8]>) -> Result<Self> {
        let payload =
            bytes
                .strip_prefix(Self::PREFIX.as_bytes())
                .ok_or_else(|| Error::IncorrectPrefix {
                    bytes: bytes.to_vec(),
                })?;

        let string = core::str::from_utf8(payload)?;
        let slot = string.parse()?;

        Ok(Self(slot))
    }
}

impl PrefixableKey for BlockRootBySlot {
    const PREFIX: &'static str = "r";
}

#[derive(Display)]
#[display("{}{_0:x}", Self::PREFIX)]
pub struct FinalizedBlockByRoot(pub H256);

impl PrefixableKey for FinalizedBlockByRoot {
    const PREFIX: &'static str = "b";
}

#[derive(Display)]
#[display("{}{_0:x}", Self::PREFIX)]
pub struct UnfinalizedBlockByRoot(pub H256);

impl PrefixableKey for UnfinalizedBlockByRoot {
    const PREFIX: &'static str = "b_nf";
}

#[derive(Debug)]
pub struct StateByBlockRoot {
    block_root: H256,
    parents: Vec<H256>,
}

impl PrefixableKey for StateByBlockRoot {
    const PREFIX: &'static str = "s";
}

impl Display for StateByBlockRoot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{:x}", Self::PREFIX, self.block_root)?;
        for i in &self.parents {
            write!(f, "{:x}", i)?;
        }
        Ok(())
    }
}

impl StateByBlockRoot {
    fn snapshot(block_root: H256) -> Self {
        Self {
            block_root,
            parents: Vec::new(),
        }
    }

    fn extend_chain(mut self, block_root: H256) -> Self {
        self.parents.insert(0, self.block_root);

        Self {
            block_root,
            parents: self.parents,
        }
    }
}

#[derive(Display)]
#[display("{}{_0:x}", Self::PREFIX)]
pub struct SlotByStateRoot(pub H256);

impl PrefixableKey for SlotByStateRoot {
    const PREFIX: &'static str = "t";
}

#[derive(Display)]
#[display("{}", Self::PREFIX)]
pub struct FinalizedValidators;

impl FinalizedValidators {
    const KEY: &'static str = "finalized_validators";
}

impl PrefixableKey for FinalizedValidators {
    const PREFIX: &'static str = Self::KEY;
}

#[derive(Display)]
#[display("{}", Self::PREFIX)]
pub struct FinalizedValidatorCount;

impl FinalizedValidatorCount {
    const KEY: &'static str = "finalized_validator_count";
}

impl PrefixableKey for FinalizedValidatorCount {
    const PREFIX: &'static str = Self::KEY;
}

#[derive(Display)]
#[display("{}{_0:x}{_1}", Self::PREFIX)]
pub struct BlobSidecarByBlobId(pub H256, pub BlobIndex);

impl PrefixableKey for BlobSidecarByBlobId {
    const PREFIX: &'static str = "o";

    #[cfg(test)]
    fn has_prefix(bytes: &[u8]) -> bool {
        bytes.starts_with(Self::PREFIX.as_bytes())
    }
}

#[derive(Display)]
#[display("{}{_0:020}{_1:x}{_2}", Self::PREFIX)]
pub struct SlotBlobId(pub Slot, pub H256, pub BlobIndex);

impl PrefixableKey for SlotBlobId {
    const PREFIX: &'static str = "i";
}

#[derive(Display)]
#[display("{}{_0:x}{_1}", Self::PREFIX)]
pub struct DataColumnSidecarByColumnId(pub H256, pub ColumnIndex);

impl PrefixableKey for DataColumnSidecarByColumnId {
    const PREFIX: &'static str = "d";

    #[cfg(test)]
    fn has_prefix(bytes: &[u8]) -> bool {
        bytes.starts_with(Self::PREFIX.as_bytes())
    }
}

#[derive(Display)]
#[display("{}{_0:020}{_1:x}{_2}", Self::PREFIX)]
pub struct SlotColumnId(pub Slot, pub H256, pub ColumnIndex);

impl PrefixableKey for SlotColumnId {
    const PREFIX: &'static str = "c";
}

#[derive(Display)]
#[display("{}", Self::PREFIX)]
pub struct StateAnchorKey;

impl PrefixableKey for StateAnchorKey {
    const PREFIX: &'static str = "anchor";
}

#[derive(Debug, Ssz)]
#[ssz(derive_hash = false)]
pub struct StateAnchor {
    slot: Slot,
    block_root: H256,
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("checkpoint sync failed")]
    CheckpointSyncFailed,
    #[error("failed to look up dependent root")]
    DependentRootLookupFailed,
    #[error("genesis block root not found in storage")]
    GenesisBlockRootNotFound,
    #[error("block not found in storage: {block_root:?}")]
    BlockNotFound { block_root: H256 },
    #[error("state not found in storage: {state_slot}")]
    StateNotFound { state_slot: Slot },
    #[error(
        "checkpoint block root does not match state checkpoint \
         (requested: {requested:?}, computed: {computed:?})"
    )]
    CheckpointBlockRootMismatch { requested: H256, computed: H256 },
    #[error("persisted slot cannot contain anchor: {slot}")]
    PersistedSlotCannotContainAnchor { slot: Slot },
    #[error("storage key has incorrect prefix: {bytes:?}")]
    IncorrectPrefix { bytes: Vec<u8> },
}

pub fn save(database: &Database, key: impl Display, value: impl SszWrite) -> Result<()> {
    database.put(serialize_key(key), serialize_value(value)?)
}

pub fn get<V: SszReadDefault>(database: &Database, key: impl Display) -> Result<Option<V>> {
    database
        .get(serialize_key(key))?
        .map(V::from_ssz_default)
        .transpose()
        .map_err(Into::into)
}

/// Populates the Merkle caches of a state's largest fields in parallel.
///
/// Used right after reconstructing a base state from diffs (which leaves every cache cold) so that
/// the first `hash_tree_root` during replay recombines already-hashed subtrees instead of rebuilding
/// the whole tree on one thread. Each list caches its node roots through interior mutability, so
/// only shared access is required. The closures borrow `state` (which is `Sync`); the `post_altair`
/// trait object is created and used inside the worker thread, never sent across threads.
fn prewarm_state_hash_caches<P: Preset>(state: &BeaconState<P>) {
    // Only the large per-validator `PersistentList` fields are warmed here. They dominate the cold
    // hash and cache their node roots through interior mutability with no panic path (an empty list
    // hashes to the zero root). The validator registry is excluded: it is already kept warm via
    // structural sharing, and its `hash_tree_root` asserts the presence of a cache.
    rayon::scope(|scope| {
        scope.spawn(|_| {
            state.balances().par_warm_hash();
        });
        scope.spawn(|_| {
            if let Some(state) = state.post_altair() {
                state.previous_epoch_participation().par_warm_hash();
            }
        });
        scope.spawn(|_| {
            if let Some(state) = state.post_altair() {
                state.current_epoch_participation().par_warm_hash();
            }
        });
        scope.spawn(|_| {
            if let Some(state) = state.post_altair() {
                state.inactivity_scores().par_warm_hash();
            }
        });
    });
}

fn serialize_key(key: impl Display) -> String {
    key.to_string()
}

fn serialize_value(value: impl SszWrite) -> Result<Vec<u8>> {
    value.to_ssz().map_err(Into::into)
}

pub fn serialize(key: impl Display, value: impl SszWrite) -> Result<(String, Vec<u8>)> {
    Ok((serialize_key(key), serialize_value(value)?))
}

/// Returns `true` if `key` addresses a hierarchical state *diff* (a [`BeaconStatePatch`]) rather
/// than a referential frame snapshot.
///
/// `StateByBlockRoot` snapshot keys are `PREFIX` followed by a single 32-byte root encoded as 64
/// hex characters. Diff keys additionally encode one or more parent roots (64 hex characters each),
/// so they are strictly longer. Only diffs are stored as patches, and those are what benefit from
/// zstd compression.
#[must_use]
pub fn is_state_diff_key(key: &[u8]) -> bool {
    // A 32-byte `H256` root is rendered as 64 hex characters by the `Display` impl.
    const ROOT_HEX_LEN: usize = 64;

    key.starts_with(StateByBlockRoot::PREFIX.as_bytes())
        && key.len() > StateByBlockRoot::PREFIX.len() + ROOT_HEX_LEN
}

/// Builds the [`CompressionSelector`] for the beacon fork choice database.
///
/// State diffs are compressed with zstd at `zstd_level`; every other key keeps using Snappy, which
/// preserves the on-disk format of databases created before per-key compression existed. The level
/// only affects writes — decompression is level-independent — so the same selector works for
/// read-only consumers regardless of the level passed.
#[must_use]
pub fn beacon_state_compression_selector(zstd_level: i32) -> CompressionSelector {
    Arc::new(move |key: &[u8]| {
        if is_state_diff_key(key) {
            Compression::Zstd(zstd_level)
        } else {
            Compression::Snappy
        }
    })
}

// Add more info when needed
pub fn print_beacon_database_info(database: &Database) -> Result<()> {
    info!("beacon_fork_choice database info:");

    match database
        .iterator_ascending(SlotColumnId(0, H256::zero(), 0).to_string()..)?
        .next()
        .transpose()?
    {
        Some((key_bytes, value_bytes)) if SlotColumnId::has_prefix(&key_bytes) => {
            info!(
                "oldest data column entry: {:?}",
                DataColumnIdentifier::from_ssz_default(value_bytes)?,
            );
        }
        _ => info!("no data column entries found"),
    }

    Ok(())
}

fn prepare_state<P: Preset>(
    mut state: Arc<BeaconState<P>>,
    finalized_validator_list_len: usize,
) -> Arc<BeaconState<P>> {
    let state_mut = state.make_mut();

    // pubkeys never change, so they can be restored later from the finalized
    // validator list; zero out the leading (finalized) prefix to shrink the
    // serialized state.
    state_mut
        .validators_mut()
        .clear_pubkeys(finalized_validator_list_len);

    state
}

// #[cfg(test)]
// mod tests {
//     use bytesize::ByteSize;
//     use database::DatabaseMode;
//     use tempfile::TempDir;
//     use types::{
//         phase0::containers::{
//             BeaconBlock as Phase0BeaconBlock, SignedBeaconBlock as Phase0SignedBeaconBlock,
//         },
//         preset::Mainnet,
//     };

//     use super::*;

//     fn block_with_slot(slot: Slot) -> SignedBeaconBlock<Mainnet> {
//         SignedBeaconBlock::<Mainnet>::Phase0(Phase0SignedBeaconBlock {
//             message: Phase0BeaconBlock {
//                 slot,
//                 ..Phase0BeaconBlock::default()
//             }
//             .into(),
//             ..Phase0SignedBeaconBlock::default()
//         })
//     }

//     #[test]
//     fn test_prune_unfinalized_blocks() -> Result<()> {
//         let database = Database::persistent(
//             "test_db",
//             TempDir::new()?,
//             ByteSize::mib(10),
//             DatabaseMode::ReadWrite,
//             None,
//         )?;

//         let block_1 = block_with_slot(1);
//         let block_3 = block_with_slot(3);
//         let block_5 = block_with_slot(5);
//         let block_6 = block_with_slot(6);
//         let block_10 = block_with_slot(10);

//         database.put_batch(vec![
//             // Slot 1
//             serialize(BlockRootBySlot(1), H256::repeat_byte(1))?,
//             serialize(FinalizedBlockByRoot(H256::repeat_byte(1)), &block_1)?,
//             serialize(SlotByStateRoot(H256::repeat_byte(1)), 1_u64)?,
//             serialize(StateByBlockRoot(H256::repeat_byte(1)), 1_u64)?,
//             // Slot 3
//             serialize(BlockRootBySlot(3), H256::repeat_byte(3))?,
//             serialize(FinalizedBlockByRoot(H256::repeat_byte(3)), &block_3)?,
//             // Slot 5
//             serialize(BlockRootBySlot(5), H256::repeat_byte(5))?,
//             serialize(UnfinalizedBlockByRoot(H256::repeat_byte(5)), &block_5)?,
//             //Slot 6
//             serialize(BlockRootBySlot(6), H256::repeat_byte(6))?,
//             serialize(FinalizedBlockByRoot(H256::repeat_byte(6)), &block_6)?,
//             serialize(UnfinalizedBlockByRoot(H256::repeat_byte(6)), &block_6)?,
//             serialize(SlotByStateRoot(H256::repeat_byte(6)), 6_u64)?,
//             serialize(StateByBlockRoot(H256::repeat_byte(6)), 6_u64)?,
//             // Slot 10, test case that "10" < "3" is not true
//             serialize(BlockRootBySlot(10), H256::repeat_byte(10))?,
//             serialize(UnfinalizedBlockByRoot(H256::repeat_byte(10)), &block_10)?,
//             serialize(SlotByStateRoot(H256::repeat_byte(10)), 10_u64)?,
//             serialize(StateByBlockRoot(H256::repeat_byte(10)), 10_u64)?,
//         ])?;

//         let storage = Storage::<Mainnet>::new(
//             Arc::new(Config::mainnet()),
//             Arc::new(PubkeyCache::default()),
//             database,
//             nonzero!(64_u64),
//             StorageMode::default(),
//         );

//         // slots 1, 3, 10
//         assert_eq!(storage.finalized_block_count()?, 3);
//         // slots 1, 3, 5, 6, 10
//         assert_eq!(storage.unfinalized_block_count()?, 3);
//         assert_eq!(storage.block_root_by_slot_count()?, 5);
//         assert_eq!(storage.slot_by_state_root_count()?, 3);
//         assert_eq!(storage.state_count()?, 3);

//         storage.prune_unfinalized_blocks(6)?;

//         // slots 1, 3, 10
//         assert_eq!(storage.finalized_block_count()?, 3);
//         // slots 10
//         assert_eq!(storage.unfinalized_block_count()?, 1);
//         assert_eq!(storage.block_root_by_slot_count()?, 4);
//         assert_eq!(storage.slot_by_state_root_count()?, 3);
//         assert_eq!(storage.state_count()?, 3);

//         Ok(())
//     }

//     #[test]
//     fn test_prune_old_blocks_and_states() -> Result<()> {
//         let database = Database::persistent(
//             "test_db",
//             TempDir::new()?,
//             ByteSize::mib(10),
//             DatabaseMode::ReadWrite,
//             None,
//         )?;

//         let block = SignedBeaconBlock::<Mainnet>::Phase0(Phase0SignedBeaconBlock::default());

//         database.put_batch(vec![
//             // Slot 1
//             serialize(BlockRootBySlot(1), H256::repeat_byte(1))?,
//             serialize(FinalizedBlockByRoot(H256::repeat_byte(1)), &block)?,
//             serialize(SlotByStateRoot(H256::repeat_byte(1)), 1_u64)?,
//             serialize(StateByBlockRoot(H256::repeat_byte(1)), 1_u64)?,
//             // Slot 3
//             serialize(BlockRootBySlot(3), H256::repeat_byte(3))?,
//             serialize(FinalizedBlockByRoot(H256::repeat_byte(3)), &block)?,
//             // Slot 5
//             serialize(BlockRootBySlot(5), H256::repeat_byte(5))?,
//             serialize(UnfinalizedBlockByRoot(H256::repeat_byte(5)), &block)?,
//             //Slot 6
//             serialize(BlockRootBySlot(6), H256::repeat_byte(6))?,
//             serialize(UnfinalizedBlockByRoot(H256::repeat_byte(6)), &block)?,
//             serialize(SlotByStateRoot(H256::repeat_byte(6)), 6_u64)?,
//             serialize(StateByBlockRoot(H256::repeat_byte(6)), 6_u64)?,
//             // Slot 10, test case that "10" < "3" is not true
//             serialize(BlockRootBySlot(10), H256::repeat_byte(10))?,
//             serialize(UnfinalizedBlockByRoot(H256::repeat_byte(10)), &block)?,
//             serialize(SlotByStateRoot(H256::repeat_byte(10)), 10_u64)?,
//             serialize(StateByBlockRoot(H256::repeat_byte(10)), 10_u64)?,
//         ])?;

//         let storage = Storage::<Mainnet>::new(
//             Arc::new(Config::mainnet()),
//             Arc::new(PubkeyCache::default()),
//             database,
//             Hierarchy::new([6]).unwrap(),
//             StorageMode::default(),
//         );

//         assert_eq!(storage.finalized_block_count()?, 2);
//         assert_eq!(storage.unfinalized_block_count()?, 3);
//         assert_eq!(storage.block_root_by_slot_count()?, 5);
//         assert_eq!(storage.slot_by_state_root_count()?, 3);
//         assert_eq!(storage.state_count()?, 3);

//         storage.prune_old_blocks_and_states(5)?;

//         assert_eq!(storage.finalized_block_count()?, 0);
//         assert_eq!(storage.unfinalized_block_count()?, 3);
//         assert_eq!(storage.block_root_by_slot_count()?, 3);
//         assert_eq!(storage.slot_by_state_root_count()?, 3);
//         assert_eq!(storage.state_count()?, 2);

//         storage.prune_old_state_roots(5)?;

//         assert_eq!(storage.slot_by_state_root_count()?, 2);

//         Ok(())
//     }

//     #[test]
//     #[expect(clippy::similar_names)]
//     fn test_prune_old_blob_sidecars() -> Result<()> {
//         let database = Database::persistent(
//             "test_db",
//             TempDir::new()?,
//             ByteSize::mib(10),
//             DatabaseMode::ReadWrite,
//             None,
//         )?;

//         let storage = Storage::<Mainnet>::new(
//             Arc::new(Config::mainnet()),
//             Arc::new(PubkeyCache::default()),
//             database,
//             Hierarchy::new([6]).unwrap(),
//             StorageMode::default(),
//         );

//         let blob_id_0 = BlobIdentifier {
//             block_root: H256::zero(),
//             index: 0,
//         };

//         // slot 5
//         let blob_id_5 = BlobIdentifier {
//             block_root: H256::zero(),
//             index: 1,
//         };

//         let mut blob_sidecar_5 = BlobSidecar::default();
//         blob_sidecar_5.signed_block_header.message.slot = 5;

//         // slot 10
//         let blob_id_10 = BlobIdentifier {
//             block_root: H256::zero(),
//             index: 2,
//         };

//         let mut blob_sidecar_10 = BlobSidecar::default();
//         blob_sidecar_10.signed_block_header.message.slot = 10;

//         let blob_sidecars = vec![
//             BlobSidecarWithId {
//                 blob_sidecar: Arc::new(BlobSidecar::default()),
//                 blob_id: blob_id_0,
//             },
//             BlobSidecarWithId {
//                 blob_sidecar: Arc::new(blob_sidecar_5),
//                 blob_id: blob_id_5,
//             },
//             BlobSidecarWithId {
//                 blob_sidecar: Arc::new(blob_sidecar_10),
//                 blob_id: blob_id_10,
//             },
//         ];

//         let persisted = storage.append_blob_sidecars(blob_sidecars)?;

//         assert_eq!(persisted, vec![blob_id_0, blob_id_5, blob_id_10]);
//         assert_eq!(storage.slot_by_blob_id_count()?, 3);
//         assert_eq!(storage.blob_sidecar_by_blob_id_count()?, 3);

//         storage.prune_old_blob_sidecars(6)?;

//         assert_eq!(storage.slot_by_blob_id_count()?, 1);
//         assert_eq!(storage.blob_sidecar_by_blob_id_count()?, 1);

//         Ok(())
//     }

//     #[test]
//     fn test_block_root_before_or_at_slot() -> Result<()> {
//         let database = Database::in_memory();

//         database.put_batch(vec![
//             serialize(BlockRootBySlot(2), H256::repeat_byte(2))?,
//             serialize(BlockRootBySlot(6), H256::repeat_byte(6))?,
//         ])?;

//         let storage = Storage::<Mainnet>::new(
//             Arc::new(Config::mainnet()),
//             Arc::new(PubkeyCache::default()),
//             database,
//             Hierarchy::new([6]).unwrap(),
//             StorageMode::default(),
//         );

//         assert_eq!(storage.block_root_before_or_at_slot(1)?, None);
//         assert_eq!(
//             storage.block_root_before_or_at_slot(2)?,
//             Some(H256::repeat_byte(2)),
//         );
//         assert_eq!(
//             storage.block_root_before_or_at_slot(3)?,
//             Some(H256::repeat_byte(2)),
//         );
//         assert_eq!(
//             storage.block_root_before_or_at_slot(6)?,
//             Some(H256::repeat_byte(6)),
//         );
//         assert_eq!(
//             storage.block_root_before_or_at_slot(9)?,
//             Some(H256::repeat_byte(6)),
//         );

//         Ok(())
//     }
// }
