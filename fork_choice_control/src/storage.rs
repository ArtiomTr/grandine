use core::{cell::OnceCell, marker::PhantomData, num::NonZeroU64};
use std::{borrow::Cow, sync::Arc};

use anyhow::{Context as _, Error as AnyhowError, Result, bail, ensure};
use database::{Database, PrefixableKey, decompress};
use derive_more::Display;
use fork_choice_store::{ChainLink, Store};
use genesis::AnchorCheckpointProvider;
use helper_functions::{
    accessors,
    error::SignatureKind,
    misc,
    signing::SignForSingleFork as _,
    slot_report::NullSlotReport,
    verifier::{SingleVerifier, Verifier as _},
};
use itertools::Itertools as _;
use logging::{debug_with_peers, info_with_peers, warn_with_peers};
use nonzero_ext::nonzero;
use pubkey_cache::PubkeyCache;
use reqwest::Client;
use ssz::{Ssz, SszHash as _, SszRead, SszReadDefault, SszWrite};
use std_ext::ArcExt as _;
use thiserror::Error;
use tracing::info;
use transition_functions::{combined, unphased::StateRootPolicy};
use typenum::Unsigned as _;
use types::{
    combined::{BeaconState, DataColumnSidecar, SignedBeaconBlock, SignedBlindedBeaconBlock},
    config::Config,
    deneb::{
        containers::{BlobIdentifier, BlobSidecar},
        primitives::BlobIndex,
    },
    fulu::{containers::DataColumnIdentifier, primitives::ColumnIndex},
    gloas::containers::SignedExecutionPayloadEnvelope,
    nonstandard::{
        BlobSidecarWithId, DataColumnSidecarWithId, FinalizedCheckpoint, Phase, PubkeyList,
        StorageMode,
    },
    phase0::{
        consts::GENESIS_SLOT,
        containers::SignedBeaconBlockHeader,
        primitives::{Epoch, H256, Slot},
    },
    preset::Preset,
    redacting_url::RedactingUrl,
    traits::{BeaconBlock, BeaconState as _, SignedBeaconBlock as _, SszValidatorList},
};

use crate::checkpoint_sync;

pub const DEFAULT_ARCHIVAL_EPOCH_INTERVAL: NonZeroU64 = nonzero!(32_u64);
pub const MAX_DATA_COLUMN_EPOCHS_TO_PRUNE: usize = 100;

/// Suffix distinguishing a blinded finalized block from a full one.
const BLINDED_BLOCK_SUFFIX: &str = "bl";

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
pub struct Storage<P> {
    config: Arc<Config>,
    pub(crate) database: Arc<Database>,
    pub(crate) archival_epoch_interval: NonZeroU64,
    storage_mode: StorageMode,
    pub(crate) pubkey_cache: Arc<PubkeyCache>,
    store_payloads: bool,
    phantom: PhantomData<P>,
}

impl<P: Preset> Storage<P> {
    #[must_use]
    pub fn new(
        config: Arc<Config>,
        pubkey_cache: Arc<PubkeyCache>,
        database: Database,
        archival_epoch_interval: NonZeroU64,
        storage_mode: StorageMode,
        store_payloads: bool,
    ) -> Self {
        Self {
            config,
            pubkey_cache,
            database: Arc::new(database),
            archival_epoch_interval,
            storage_mode,
            store_payloads,
            phantom: PhantomData,
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

    #[must_use]
    pub const fn payload_storage_enabled(&self) -> bool {
        self.store_payloads
    }

    #[expect(clippy::too_many_lines)]
    pub async fn load(
        &self,
        client: &Client,
        state_load_strategy: StateLoadStrategy<P>,
    ) -> Result<(LoadedStateStorage<'_, P>, bool)> {
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
                let mut local_state_storage = match state_slot {
                    Some(slot) => self.load_state_by_iteration(slot, None)?,
                    None => self.load_latest_state(None)?,
                };

                // The anchor block seeds the fork choice store, which needs its real
                // `hash_tree_root` and therefore the full block. Only the checkpoint record keeps
                // a block whole when payload storage is off, and iteration bypasses that record,
                // so `--state-slot` may find a state it cannot use. The latest checkpoint is the
                // only usable replacement; it may be newer than the requested slot, which does not
                // honor the rewind, but the alternative is falling back to the anchor checkpoint
                // and resyncing from it.
                if state_slot.is_some()
                    && let OptionalStateStorage::Full((state, block, _)) = &local_state_storage
                    && block.is_blinded()
                {
                    warn_with_peers!(
                        "block of the stored state in slot {} is missing its execution payload",
                        state.slot(),
                    );

                    let latest_state_storage = self.load_latest_state(None)?;

                    if let OptionalStateStorage::Full((latest_state, latest_block, _)) =
                        &latest_state_storage
                        && !latest_block.is_blinded()
                    {
                        warn_with_peers!(
                            "falling back to the stored checkpoint in slot {}; \
                            pass --store-payloads to make --state-slot reach past it",
                            latest_state.slot(),
                        );

                        local_state_storage = latest_state_storage;
                    }
                }

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
                    // The anchor block seeds the fork choice store, which needs its real
                    // `hash_tree_root` and therefore the full block. Blocks written as
                    // checkpoints are always stored full, so this only rejects anchors found
                    // by iteration on a database whose checkpoint record is missing.
                    OptionalStateStorage::Full((state, block, blocks)) if !block.is_blinded() => {
                        anchor_state = state;
                        anchor_block = block.into_full()?;
                        unfinalized_blocks = blocks;
                    }
                    OptionalStateStorage::Full((_, _, local_unfinalized_blocks)) => {
                        warn_with_peers!(
                            "stored anchor block is missing its execution payload; \
                             falling back to the anchor checkpoint",
                        );

                        let FinalizedCheckpoint { block, state } =
                            anchor_checkpoint_provider.checkpoint().value;

                        anchor_block = block;
                        anchor_state = state;
                        unfinalized_blocks = local_unfinalized_blocks;
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
            serialize(FinalizedBlockByRoot::full(anchor_block_root), &anchor_block)?,
            serialize(BlockRootBySlot(anchor_slot), anchor_block_root)?,
            serialize(SlotByStateRoot(anchor_state_root), anchor_slot)?,
            serialize(
                StateByBlockRoot(anchor_block_root),
                prepare_state(anchor_state.clone_arc(), anchor_validators.len_usize()),
            )?,
        ];

        self.append_finalized_validator_pubkeys_to_batch(&mut batch, anchor_validators)?;

        self.database.put_batch(batch)?;

        let state_storage = (anchor_state, anchor_block, unfinalized_blocks);

        Ok((state_storage, loaded_from_remote))
    }

    fn load_latest_state(
        &self,
        finalized_validators: Option<&dyn SszValidatorList>,
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
        let mut archival_state_appended = false;
        let mut batch = vec![];

        let finalized_validators = store.finalized_validators();

        let unfinalized = unfinalized.zip(core::iter::repeat(false));
        let finalized = finalized.rev().zip(core::iter::repeat(true));

        let mut chain = unfinalized
            .chain(finalized)
            .filter(|(chain_link, is_finalized)| *is_finalized || chain_link.is_valid())
            .peekable();

        if let Some(StateCheckpoint { head_slot, .. }) =
            self.load_state_checkpoint(Some(&*finalized_validators))?
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
                    batch.push(self.serialize_finalized_block(block_root, block)?);
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
                let state_epoch = Self::epoch_at_slot(state_slot);
                let is_epoch_start = misc::is_epoch_start::<P>(state_slot);
                let is_archival_epoch_start = is_epoch_start
                    && state_epoch.is_multiple_of(self.archival_epoch_interval.into());

                if !checkpoint_state_appended
                    && ((store.is_forward_synced() && is_epoch_start) || is_archival_epoch_start)
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

                if !archival_state_appended
                    && !self.prune_storage_enabled()
                    && is_archival_epoch_start
                {
                    info_with_peers!("saving state in slot {state_slot}");

                    batch.push(serialize(
                        StateByBlockRoot(block_root),
                        prepare_state(
                            state.get_or_init(|| chain_link.state(store)).clone_arc(),
                            finalized_validators.len_usize(),
                        ),
                    )?);

                    archival_state_appended = true;
                    update_finalized_validators = true;
                }
            }
        }

        if update_finalized_validators {
            self.append_finalized_validator_pubkeys_to_batch(&mut batch, &*finalized_validators)?;
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
        finalized_validators: &dyn SszValidatorList,
    ) -> Result<Vec<Slot>> {
        let mut slots = vec![];
        let mut batch = vec![];
        let mut update_finalized_validators = false;

        for (state, block_root) in states_with_block_roots {
            if !self.contains_key(StateByBlockRoot(block_root))? {
                let archival_state = state.clone_arc();

                slots.push(state.slot());
                batch.push(serialize(
                    StateByBlockRoot(block_root),
                    prepare_state(archival_state, finalized_validators.len_usize()),
                )?);

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
                    .unzip()
            })?;

        for block_root_bytes in block_roots_to_remove {
            let block_root = H256::from_ssz_default(block_root_bytes)?;

            // The block may be stored either full or blinded, so delete whichever key exists.
            let block_prefix = FinalizedBlockByRoot::prefix(block_root);
            let block_keys = self.database.keys_ascending(block_prefix.as_bytes()..)?;

            keys_to_remove.extend(itertools::process_results(block_keys, |keys| {
                keys.take_while(|key| key.starts_with(block_prefix.as_bytes()))
                    .collect::<Vec<_>>()
            })?);

            keys_to_remove.push(StateByBlockRoot(block_root).to_string().into());
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

    pub(crate) fn append_execution_payload_envelopes(
        &self,
        envelopes: impl IntoIterator<Item = Arc<SignedExecutionPayloadEnvelope<P>>>,
    ) -> Result<Vec<H256>> {
        let mut batch = vec![];
        let mut persisted_block_roots = vec![];

        for envelope in envelopes {
            let block_root = envelope.block_root();
            let slot = envelope.slot();

            batch.push(serialize(EnvelopeByBlockRoot(block_root), envelope)?);
            batch.push(serialize(EnvelopeRootBySlot(slot, block_root), block_root)?);

            persisted_block_roots.push(block_root);
        }

        self.database.put_batch(batch)?;

        Ok(persisted_block_roots)
    }

    pub(crate) fn data_column_sidecar_by_id(
        &self,
        data_column_id: DataColumnIdentifier,
    ) -> Result<Option<Arc<DataColumnSidecar<P>>>> {
        let DataColumnIdentifier { block_root, index } = data_column_id;

        self.get(DataColumnSidecarByColumnId(block_root, index))
    }

    pub(crate) fn execution_payload_envelope_by_root(
        &self,
        block_root: H256,
    ) -> Result<Option<Arc<SignedExecutionPayloadEnvelope<P>>>> {
        self.get(EnvelopeByBlockRoot(block_root))
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
        finalized_validators: &dyn SszValidatorList,
    ) -> Result<Option<Slot>> {
        if let Some(StateCheckpoint { head_slot, .. }) =
            self.load_state_checkpoint(Some(finalized_validators))?
        {
            return Ok(Some(head_slot));
        }

        Ok(None)
    }

    pub(crate) fn prune_old_execution_payload_envelopes(&self, up_to_slot: Slot) -> Result<()> {
        let results = self
            .database
            .iterator_descending(..=EnvelopeRootBySlot(up_to_slot, H256::zero()).to_string())?;

        let (mut keys_to_remove, envelopes_to_remove): (Vec<_>, Vec<_>) =
            itertools::process_results(results, |iter| {
                iter.take_while(|(key_bytes, _)| EnvelopeRootBySlot::has_prefix(key_bytes))
                    .unzip()
            })?;

        for value_bytes in envelopes_to_remove {
            let block_root = H256::from_ssz_default(value_bytes)?;

            keys_to_remove.push(EnvelopeByBlockRoot(block_root).to_string().into());
        }

        self.database.delete_batch(keys_to_remove)
    }

    pub(crate) fn genesis_block_root(&self, store: &Store<P, Self>) -> Result<H256> {
        self.block_root_by_slot_with_store(store, GENESIS_SLOT)?
            .ok_or(Error::GenesisBlockRootNotFound)
            .map_err(Into::into)
    }

    /// Serialize a finalized block for storage, blinding it when payload storage is off.
    ///
    /// Pre-Bellatrix and Gloas blocks are always stored as they are: the former have no
    /// execution payload and the latter keep theirs in separate envelope records. Pre-Merge
    /// Bellatrix blocks are stored as they are too, because blinded block processing assumes every
    /// blinded block is post-Merge and rejects the default payload header they would be blinded to.
    pub(crate) fn serialize_finalized_block(
        &self,
        block_root: H256,
        block: &Arc<SignedBeaconBlock<P>>,
    ) -> Result<(String, Vec<u8>)> {
        let blindable = block.has_blindable_payload()
            && block
                .execution_block_hash()
                .is_some_and(|block_hash| !block_hash.is_zero());

        if self.store_payloads || !blindable {
            return serialize(FinalizedBlockByRoot::full(block_root), block);
        }

        let blinded = SignedBlindedBeaconBlock::try_from(block.as_ref().clone())?;

        serialize(FinalizedBlockByRoot::blinded(block_root), &blinded)
    }

    pub(crate) fn contains_finalized_block(&self, block_root: H256) -> Result<bool> {
        self.contains_prefixed_key(FinalizedBlockByRoot::prefix(block_root))
    }

    pub(crate) fn contains_unfinalized_block(&self, block_root: H256) -> Result<bool> {
        self.contains_key(UnfinalizedBlockByRoot(block_root))
    }

    pub(crate) fn finalized_block_by_root(
        &self,
        block_root: H256,
    ) -> Result<Option<StoredBlock<P>>> {
        // The full key carries the payload variant as a suffix, so a single lookup for the
        // first key at or after `b{block_root}` resolves both existence and variant.
        let prefix = FinalizedBlockByRoot::prefix(block_root);

        let Some((full_key, raw_value)) = self.database.next_raw(&prefix)? else {
            return Ok(None);
        };

        // The key we received is only known to be lexicographically greater than or equal to
        // the prefix, so it may belong to an unrelated block.
        if !full_key.starts_with(prefix.as_bytes()) {
            return Ok(None);
        }

        let key = FinalizedBlockByRoot::try_from(full_key.as_slice())?;
        let value_bytes = decompress(&raw_value)?;

        let block = match key.payload {
            StoredPayload::Full => StoredBlock::Full(Arc::from_ssz(&*self.config, value_bytes)?),
            StoredPayload::Blinded => {
                StoredBlock::Blinded(Arc::from_ssz(&*self.config, value_bytes)?)
            }
        };

        Ok(Some(block))
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

    fn state_by_block_root(
        &self,
        block_root: H256,
        finalized_validators: Option<&dyn SszValidatorList>,
    ) -> Result<Option<Arc<BeaconState<P>>>> {
        let Some(mut state) = self.get::<Arc<BeaconState<P>>>(StateByBlockRoot(block_root))? else {
            return Ok(None);
        };

        // Restore validators if they were removed
        self.restore_validators_to_state(state.make_mut(), finalized_validators)?;

        Ok(Some(state))
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
    ) -> Result<Option<(StoredBlock<P>, H256)>> {
        let Some(block_root) = self.block_root_by_slot(slot)? else {
            return Ok(None);
        };

        let Some(block) = self.finalized_block_by_root(block_root)? else {
            return Ok(None);
        };

        Ok(Some((block, block_root)))
    }

    pub(crate) fn stored_state(
        &self,
        slot: Slot,
        finalized_validators: Option<&dyn SszValidatorList>,
    ) -> Result<Option<Arc<BeaconState<P>>>> {
        let (mut state, state_block, blocks) =
            match self.load_state_by_iteration(slot, finalized_validators)? {
                OptionalStateStorage::None | OptionalStateStorage::UnfinalizedOnly(_) => {
                    return Ok(None);
                }
                OptionalStateStorage::Full(state_storage) => state_storage,
            };

        state.set_cached_root(state_block.message().state_root());

        // State may be persisted only once in several epochs.
        // `blocks` here are needed to transition state closer to `slot`.
        for result in blocks.rev() {
            self.replay_block(state.make_mut(), result?, StateRootPolicy::Trust)?;
        }

        if state.slot() < slot {
            combined::process_slots(&self.config, &self.pubkey_cache, state.make_mut(), slot)?;
        }

        Ok(Some(state))
    }

    pub(crate) fn state_post_block(
        &self,
        mut block_root: H256,
        finalized_validators: &dyn SszValidatorList,
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
                blocks.push(StoredBlock::Full(block));
                continue;
            }

            return Ok(None);
        };

        for block in blocks.into_iter().rev() {
            self.replay_block(state.make_mut(), block, StateRootPolicy::Trust)?;
        }

        Ok(Some(state))
    }

    /// Apply a stored block to `state`.
    ///
    /// Blinded blocks go through blinded block processing, which produces the same post-state
    /// as full block processing, so historical state replay never needs the execution payload.
    pub(crate) fn replay_block(
        &self,
        state: &mut BeaconState<P>,
        block: StoredBlock<P>,
        state_root_policy: StateRootPolicy,
    ) -> Result<()> {
        let block = match block {
            StoredBlock::Full(block) => {
                return match state_root_policy {
                    StateRootPolicy::Trust => combined::trusted_state_transition(
                        &self.config,
                        &self.pubkey_cache,
                        state,
                        &block,
                    ),
                    StateRootPolicy::Verify => combined::untrusted_state_transition(
                        &self.config,
                        &self.pubkey_cache,
                        state,
                        &block,
                    ),
                };
            }
            StoredBlock::Blinded(block) => block,
        };

        let slot = block.message().slot();
        let in_block = block.message().state_root();
        let (message, signature) = Arc::unwrap_or_clone(block).split();

        combined::process_slots(&self.config, &self.pubkey_cache, state, slot)?;

        match state_root_policy {
            StateRootPolicy::Trust => {
                combined::process_trusted_blinded_block(
                    &self.config,
                    &self.pubkey_cache,
                    state,
                    &message,
                    NullSlotReport,
                )?;

                state.set_cached_root(in_block);
            }
            StateRootPolicy::Verify => {
                // Blinded block processing only sees the message, so the proposer signature has to
                // be verified separately to match what full block processing does.
                let pubkey = accessors::public_key(state, message.proposer_index())?;

                SingleVerifier.verify_singular(
                    message.signing_root(&self.config, state),
                    signature,
                    self.pubkey_cache.get_or_insert(*pubkey)?,
                    SignatureKind::Block,
                )?;

                combined::process_untrusted_blinded_block(
                    &self.config,
                    &self.pubkey_cache,
                    state,
                    &message,
                    NullSlotReport,
                    false,
                )?;

                let computed = state.hash_tree_root();

                ensure!(
                    computed == in_block,
                    Error::StateRootMismatch { computed, in_block },
                );
            }
        }

        Ok(())
    }

    pub(crate) fn stored_state_by_state_root(
        &self,
        state_root: H256,
        finalized_validators: &dyn SszValidatorList,
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
        finalized_validators: Option<&dyn SszValidatorList>,
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

                StoredBlock::Full(block)
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

    fn load_state_by_iteration(
        &self,
        start_from_slot: Slot,
        finalized_validators: Option<&dyn SszValidatorList>,
    ) -> Result<OptionalStateStorage<'_, P>> {
        let results = self
            .database
            .iterator_descending(..=BlockRootBySlot(start_from_slot).to_string())?;

        let results = itertools::process_results(results, |iter| {
            iter.take_while(|(key_bytes, _)| BlockRootBySlot::has_prefix(key_bytes))
                .map(|(_, v)| v)
                .collect::<Vec<_>>()
        })?;

        let mut block_roots = vec![];

        for value_bytes in results {
            let block_root = H256::from_ssz_default(value_bytes)?;

            if self.contains_key(StateByBlockRoot(block_root))? {
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
        finalized_validators: Option<&dyn SszValidatorList>,
    ) -> Result<Option<StateCheckpoint<P>>> {
        let Some(mut checkpoint) = self.get::<StateCheckpoint<P>>(StateCheckpoint::<P>::KEY)?
        else {
            return Ok(None);
        };

        // Restore validators if they were removed
        self.restore_validators_to_state(checkpoint.state.make_mut(), finalized_validators)?;

        Ok(Some(checkpoint))
    }

    fn contains_key(&self, key: impl core::fmt::Display) -> Result<bool> {
        let key_string = key.to_string();

        self.database.contains_key(key_string)
    }

    fn contains_prefixed_key(&self, key: impl core::fmt::Display) -> Result<bool> {
        let key_string = key.to_string();

        self.database.contains_prefixed_key(key_string)
    }

    fn get<V: SszRead<Config>>(&self, key: impl core::fmt::Display) -> Result<Option<V>> {
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
                return Ok(StoredBlock::Full(block));
            }

            bail!(Error::BlockNotFound { block_root })
        }))
    }

    pub(crate) fn epoch_at_slot(slot: Slot) -> Epoch {
        misc::compute_epoch_at_slot::<P>(slot)
    }

    fn restore_validators_to_state(
        &self,
        state: &mut BeaconState<P>,
        finalized_validators: Option<&dyn SszValidatorList>,
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

                let Some(pubkeys) = self.get::<PubkeyList>(FinalizedValidators)? else {
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
        validators: &dyn SszValidatorList,
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
            .iterator_ascending_raw(FinalizedBlockByRoot::full(H256::zero()).to_string()..)?;

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
            .iterator_ascending(StateByBlockRoot(H256::zero()).to_string()..)?;

        itertools::process_results(results, |pairs| {
            pairs
                .take_while(|(key_bytes, _)| StateByBlockRoot::has_prefix(key_bytes))
                .count()
        })
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
        finalized_validators: Option<&dyn SszValidatorList>,
    ) -> Result<Option<Arc<BeaconState<P>>>> {
        self.state_by_block_root(block_root, finalized_validators)
    }
}

#[derive(Default, Debug)]
pub struct AppendedBlockSlots {
    pub finalized: Vec<Slot>,
    pub unfinalized: Vec<Slot>,
}

/// A finalized block as it is held in the database.
///
/// Blocks are stored blinded when payload storage is off, so every read path has to be
/// prepared for either form. Everything that does not live in the execution payload is
/// reachable through [`StoredBlock::message`]; callers that genuinely need the payload have
/// to reconstruct it from the execution client, which `fork_choice_control` cannot do.
#[derive(Clone, Debug)]
pub enum StoredBlock<P: Preset> {
    Full(Arc<SignedBeaconBlock<P>>),
    Blinded(Arc<SignedBlindedBeaconBlock<P>>),
}

impl<P: Preset> StoredBlock<P> {
    #[must_use]
    pub fn message(&self) -> &dyn BeaconBlock<P> {
        match self {
            Self::Full(block) => block.message(),
            Self::Blinded(block) => block.message(),
        }
    }

    /// The block header, which never depends on the execution payload.
    #[must_use]
    pub fn to_header(&self) -> SignedBeaconBlockHeader {
        match self {
            Self::Full(block) => block.to_header(),
            Self::Blinded(block) => block
                .message()
                .to_header()
                .with_signature(block.signature()),
        }
    }

    #[must_use]
    pub fn phase(&self) -> Phase {
        match self {
            Self::Full(block) => block.phase(),
            Self::Blinded(block) => block.phase(),
        }
    }

    #[must_use]
    pub const fn is_blinded(&self) -> bool {
        matches!(self, Self::Blinded(_))
    }

    pub fn into_full(self) -> Result<Arc<SignedBeaconBlock<P>>> {
        match self {
            Self::Full(block) => Ok(block),
            Self::Blinded(block) => bail!(Error::BlockPayloadNotStored {
                block_root: block.message().hash_tree_root(),
            }),
        }
    }
}

type UnfinalizedBlocks<'storage, P> =
    Box<dyn DoubleEndedIterator<Item = Result<StoredBlock<P>>> + Send + 'storage>;

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
    StoredBlock<P>,
    UnfinalizedBlocks<'storage, P>,
);

/// Anchor as handed to the fork choice store.
///
/// The anchor block is always full, but the blocks that follow it may be stored blinded, so the
/// caller has to reconstruct them before the fork choice store can re-validate them.
type LoadedStateStorage<'storage, P> = (
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

/// Whether a stored finalized block carries its execution payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoredPayload {
    Full,
    Blinded,
}

/// Key of a finalized block.
///
/// Full blocks keep the bare `b{block_root}` key they have always had, so databases written
/// by older versions load unchanged. Blinded blocks append [`BLINDED_BLOCK_SUFFIX`], which
/// lets a single `next_key` over `b{block_root}` resolve both existence and variant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FinalizedBlockByRoot {
    pub block_root: H256,
    pub payload: StoredPayload,
}

impl PrefixableKey for FinalizedBlockByRoot {
    const PREFIX: &'static str = "b";
}

impl core::fmt::Display for FinalizedBlockByRoot {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}{:x}", Self::PREFIX, self.block_root)?;

        if matches!(self.payload, StoredPayload::Blinded) {
            write!(f, "{BLINDED_BLOCK_SUFFIX}")?;
        }

        Ok(())
    }
}

impl TryFrom<&'_ [u8]> for FinalizedBlockByRoot {
    type Error = AnyhowError;

    fn try_from(value: &'_ [u8]) -> Result<Self> {
        let Some(stripped) = value.strip_prefix(Self::PREFIX.as_bytes()) else {
            bail!("invalid prefix");
        };

        let (stripped, payload) = match stripped.strip_suffix(BLINDED_BLOCK_SUFFIX.as_bytes()) {
            Some(stripped) => (stripped, StoredPayload::Blinded),
            None => (stripped, StoredPayload::Full),
        };

        let mut block_root = H256::default();

        hex::decode_to_slice(str::from_utf8(stripped)?, &mut block_root.0)?;

        Ok(Self {
            block_root,
            payload,
        })
    }
}

impl FinalizedBlockByRoot {
    #[must_use]
    pub const fn full(block_root: H256) -> Self {
        Self {
            block_root,
            payload: StoredPayload::Full,
        }
    }

    #[must_use]
    pub const fn blinded(block_root: H256) -> Self {
        Self {
            block_root,
            payload: StoredPayload::Blinded,
        }
    }

    pub(crate) fn prefix(block_root: H256) -> String {
        format!("{}{:x}", Self::PREFIX, block_root)
    }
}

#[derive(Display)]
#[display("{}{_0:x}", Self::PREFIX)]
pub struct UnfinalizedBlockByRoot(pub H256);

impl PrefixableKey for UnfinalizedBlockByRoot {
    const PREFIX: &'static str = "b_nf";
}

#[derive(Display)]
#[display("{}{_0:x}", Self::PREFIX)]
pub struct StateByBlockRoot(pub H256);

impl PrefixableKey for StateByBlockRoot {
    const PREFIX: &'static str = "s";
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
#[display("{}{_0:x}", Self::PREFIX)]
pub struct EnvelopeByBlockRoot(pub H256);

impl PrefixableKey for EnvelopeByBlockRoot {
    const PREFIX: &'static str = "e";

    #[cfg(test)]
    fn has_prefix(bytes: &[u8]) -> bool {
        bytes.starts_with(Self::PREFIX.as_bytes())
    }
}

#[derive(Display)]
#[display("{}{_0:020}{_1:x}", Self::PREFIX)]
pub struct EnvelopeRootBySlot(pub Slot, pub H256);

impl PrefixableKey for EnvelopeRootBySlot {
    const PREFIX: &'static str = "v";

    #[cfg(test)]
    fn has_prefix(bytes: &[u8]) -> bool {
        bytes.starts_with(Self::PREFIX.as_bytes())
    }
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
    #[error("block {block_root:?} is stored without its execution payload")]
    BlockPayloadNotStored { block_root: H256 },
    #[error("state root mismatch (computed: {computed:?}, in block: {in_block:?})")]
    StateRootMismatch { computed: H256, in_block: H256 },
}

pub fn save(database: &Database, key: impl core::fmt::Display, value: impl SszWrite) -> Result<()> {
    database.put(serialize_key(key), serialize_value(value)?)
}

pub fn get<V: SszReadDefault>(
    database: &Database,
    key: impl core::fmt::Display,
) -> Result<Option<V>> {
    database
        .get(serialize_key(key))?
        .map(V::from_ssz_default)
        .transpose()
        .map_err(Into::into)
}

fn serialize_key(key: impl core::fmt::Display) -> String {
    key.to_string()
}

fn serialize_value(value: impl SszWrite) -> Result<Vec<u8>> {
    value.to_ssz().map_err(Into::into)
}

pub fn serialize(key: impl core::fmt::Display, value: impl SszWrite) -> Result<(String, Vec<u8>)> {
    Ok((serialize_key(key), serialize_value(value)?))
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

#[cfg(test)]
mod tests {
    use bls::SignatureBytes;
    use bytesize::ByteSize;
    use database::DatabaseMode;
    use ssz::SszHash as _;
    use tempfile::TempDir;
    use types::{
        altair::containers::BeaconBlock as AltairBeaconBlock,
        bellatrix::containers::{
            BeaconBlock as BellatrixBeaconBlock, BeaconBlockBody as BellatrixBeaconBlockBody,
            ExecutionPayload as BellatrixExecutionPayload,
        },
        capella::containers::{
            BeaconBlock as CapellaBeaconBlock, BeaconBlockBody as CapellaBeaconBlockBody,
            ExecutionPayload as CapellaExecutionPayload,
        },
        combined::BeaconBlock,
        deneb::containers::{
            BeaconBlock as DenebBeaconBlock, BeaconBlockBody as DenebBeaconBlockBody,
            ExecutionPayload as DenebExecutionPayload,
        },
        electra::containers::{
            BeaconBlock as ElectraBeaconBlock, BeaconBlockBody as ElectraBeaconBlockBody,
        },
        fulu::containers::{
            BeaconBlock as FuluBeaconBlock, BeaconBlockBody as FuluBeaconBlockBody,
        },
        gloas::containers::BeaconBlock as GloasBeaconBlock,
        nonstandard::Phase,
        phase0::{
            containers::{
                BeaconBlock as Phase0BeaconBlock, SignedBeaconBlock as Phase0SignedBeaconBlock,
            },
            primitives::ExecutionBlockHash,
        },
        preset::{Mainnet, Minimal},
    };

    use super::*;

    const BLINDABLE_PHASES: [Phase; 5] = [
        Phase::Bellatrix,
        Phase::Capella,
        Phase::Deneb,
        Phase::Electra,
        Phase::Fulu,
    ];

    const UNBLINDABLE_PHASES: [Phase; 3] = [Phase::Phase0, Phase::Altair, Phase::Gloas];

    /// Builds a block at `phase` whose execution payload, if any, is post-Merge.
    ///
    /// Only post-Merge blocks are stored blinded, so a default payload would defeat the point of
    /// most of the tests below.
    fn block_at_phase(phase: Phase) -> Arc<SignedBeaconBlock<Mainnet>> {
        let block_hash = ExecutionBlockHash::repeat_byte(1);

        let block = match phase {
            Phase::Phase0 => BeaconBlock::Phase0(Phase0BeaconBlock::default().into()),
            Phase::Altair => BeaconBlock::Altair(AltairBeaconBlock::default().into()),
            Phase::Bellatrix => BeaconBlock::Bellatrix(
                BellatrixBeaconBlock {
                    body: BellatrixBeaconBlockBody {
                        execution_payload: BellatrixExecutionPayload {
                            block_hash,
                            ..BellatrixExecutionPayload::default()
                        },
                        ..BellatrixBeaconBlockBody::default()
                    },
                    ..BellatrixBeaconBlock::default()
                }
                .into(),
            ),
            Phase::Capella => BeaconBlock::Capella(
                CapellaBeaconBlock {
                    body: CapellaBeaconBlockBody {
                        execution_payload: CapellaExecutionPayload {
                            block_hash,
                            ..CapellaExecutionPayload::default()
                        },
                        ..CapellaBeaconBlockBody::default()
                    },
                    ..CapellaBeaconBlock::default()
                }
                .into(),
            ),
            Phase::Deneb => BeaconBlock::Deneb(
                DenebBeaconBlock {
                    body: DenebBeaconBlockBody {
                        execution_payload: DenebExecutionPayload {
                            block_hash,
                            ..DenebExecutionPayload::default()
                        },
                        ..DenebBeaconBlockBody::default()
                    },
                    ..DenebBeaconBlock::default()
                }
                .into(),
            ),
            Phase::Electra => BeaconBlock::Electra(
                ElectraBeaconBlock {
                    body: ElectraBeaconBlockBody {
                        execution_payload: DenebExecutionPayload {
                            block_hash,
                            ..DenebExecutionPayload::default()
                        },
                        ..ElectraBeaconBlockBody::default()
                    },
                    ..ElectraBeaconBlock::default()
                }
                .into(),
            ),
            Phase::Fulu => BeaconBlock::Fulu(
                FuluBeaconBlock {
                    body: FuluBeaconBlockBody {
                        execution_payload: DenebExecutionPayload {
                            block_hash,
                            ..DenebExecutionPayload::default()
                        },
                        ..FuluBeaconBlockBody::default()
                    },
                    ..FuluBeaconBlock::default()
                }
                .into(),
            ),
            Phase::Gloas => BeaconBlock::Gloas(GloasBeaconBlock::default().into()),
        };

        Arc::new(block.with_signature(SignatureBytes::default()))
    }

    fn storage_with_config(config: Config, store_payloads: bool) -> Storage<Mainnet> {
        Storage::<Mainnet>::new(
            Arc::new(config),
            Arc::new(PubkeyCache::default()),
            Database::in_memory(),
            DEFAULT_ARCHIVAL_EPOCH_INTERVAL,
            StorageMode::default(),
            store_payloads,
        )
    }

    fn storage_with_payload_setting(store_payloads: bool) -> Storage<Mainnet> {
        storage_with_config(Config::mainnet(), store_payloads)
    }

    fn block_with_slot(slot: Slot) -> SignedBeaconBlock<Mainnet> {
        SignedBeaconBlock::<Mainnet>::Phase0(Phase0SignedBeaconBlock {
            message: Phase0BeaconBlock {
                slot,
                ..Phase0BeaconBlock::default()
            }
            .into(),
            ..Phase0SignedBeaconBlock::default()
        })
    }

    #[test]
    fn test_prune_unfinalized_blocks() -> Result<()> {
        let database = Database::persistent(
            "test_db",
            TempDir::new()?,
            ByteSize::mib(10),
            DatabaseMode::ReadWrite,
            None,
        )?;

        let block_1 = block_with_slot(1);
        let block_3 = block_with_slot(3);
        let block_5 = block_with_slot(5);
        let block_6 = block_with_slot(6);
        let block_10 = block_with_slot(10);

        database.put_batch(vec![
            // Slot 1
            serialize(BlockRootBySlot(1), H256::repeat_byte(1))?,
            serialize(FinalizedBlockByRoot::full(H256::repeat_byte(1)), &block_1)?,
            serialize(SlotByStateRoot(H256::repeat_byte(1)), 1_u64)?,
            serialize(StateByBlockRoot(H256::repeat_byte(1)), 1_u64)?,
            // Slot 3
            serialize(BlockRootBySlot(3), H256::repeat_byte(3))?,
            serialize(FinalizedBlockByRoot::full(H256::repeat_byte(3)), &block_3)?,
            // Slot 5
            serialize(BlockRootBySlot(5), H256::repeat_byte(5))?,
            serialize(UnfinalizedBlockByRoot(H256::repeat_byte(5)), &block_5)?,
            //Slot 6
            serialize(BlockRootBySlot(6), H256::repeat_byte(6))?,
            serialize(FinalizedBlockByRoot::full(H256::repeat_byte(6)), &block_6)?,
            serialize(UnfinalizedBlockByRoot(H256::repeat_byte(6)), &block_6)?,
            serialize(SlotByStateRoot(H256::repeat_byte(6)), 6_u64)?,
            serialize(StateByBlockRoot(H256::repeat_byte(6)), 6_u64)?,
            // Slot 10, test case that "10" < "3" is not true
            serialize(BlockRootBySlot(10), H256::repeat_byte(10))?,
            serialize(UnfinalizedBlockByRoot(H256::repeat_byte(10)), &block_10)?,
            serialize(SlotByStateRoot(H256::repeat_byte(10)), 10_u64)?,
            serialize(StateByBlockRoot(H256::repeat_byte(10)), 10_u64)?,
        ])?;

        let storage = Storage::<Mainnet>::new(
            Arc::new(Config::mainnet()),
            Arc::new(PubkeyCache::default()),
            database,
            nonzero!(64_u64),
            StorageMode::default(),
            true,
        );

        // slots 1, 3, 10
        assert_eq!(storage.finalized_block_count()?, 3);
        // slots 1, 3, 5, 6, 10
        assert_eq!(storage.unfinalized_block_count()?, 3);
        assert_eq!(storage.block_root_by_slot_count()?, 5);
        assert_eq!(storage.slot_by_state_root_count()?, 3);
        assert_eq!(storage.state_count()?, 3);

        storage.prune_unfinalized_blocks(6)?;

        // slots 1, 3, 10
        assert_eq!(storage.finalized_block_count()?, 3);
        // slots 10
        assert_eq!(storage.unfinalized_block_count()?, 1);
        assert_eq!(storage.block_root_by_slot_count()?, 4);
        assert_eq!(storage.slot_by_state_root_count()?, 3);
        assert_eq!(storage.state_count()?, 3);

        Ok(())
    }

    #[test]
    fn test_prune_old_blocks_and_states() -> Result<()> {
        let database = Database::persistent(
            "test_db",
            TempDir::new()?,
            ByteSize::mib(10),
            DatabaseMode::ReadWrite,
            None,
        )?;

        let block = SignedBeaconBlock::<Mainnet>::Phase0(Phase0SignedBeaconBlock::default());

        database.put_batch(vec![
            // Slot 1
            serialize(BlockRootBySlot(1), H256::repeat_byte(1))?,
            serialize(FinalizedBlockByRoot::full(H256::repeat_byte(1)), &block)?,
            serialize(SlotByStateRoot(H256::repeat_byte(1)), 1_u64)?,
            serialize(StateByBlockRoot(H256::repeat_byte(1)), 1_u64)?,
            // Slot 3
            serialize(BlockRootBySlot(3), H256::repeat_byte(3))?,
            serialize(FinalizedBlockByRoot::full(H256::repeat_byte(3)), &block)?,
            // Slot 5
            serialize(BlockRootBySlot(5), H256::repeat_byte(5))?,
            serialize(UnfinalizedBlockByRoot(H256::repeat_byte(5)), &block)?,
            //Slot 6
            serialize(BlockRootBySlot(6), H256::repeat_byte(6))?,
            serialize(UnfinalizedBlockByRoot(H256::repeat_byte(6)), &block)?,
            serialize(SlotByStateRoot(H256::repeat_byte(6)), 6_u64)?,
            serialize(StateByBlockRoot(H256::repeat_byte(6)), 6_u64)?,
            // Slot 10, test case that "10" < "3" is not true
            serialize(BlockRootBySlot(10), H256::repeat_byte(10))?,
            serialize(UnfinalizedBlockByRoot(H256::repeat_byte(10)), &block)?,
            serialize(SlotByStateRoot(H256::repeat_byte(10)), 10_u64)?,
            serialize(StateByBlockRoot(H256::repeat_byte(10)), 10_u64)?,
        ])?;

        let storage = Storage::<Mainnet>::new(
            Arc::new(Config::mainnet()),
            Arc::new(PubkeyCache::default()),
            database,
            nonzero!(64_u64),
            StorageMode::default(),
            true,
        );

        assert_eq!(storage.finalized_block_count()?, 2);
        assert_eq!(storage.unfinalized_block_count()?, 3);
        assert_eq!(storage.block_root_by_slot_count()?, 5);
        assert_eq!(storage.slot_by_state_root_count()?, 3);
        assert_eq!(storage.state_count()?, 3);

        storage.prune_old_blocks_and_states(5)?;

        assert_eq!(storage.finalized_block_count()?, 0);
        assert_eq!(storage.unfinalized_block_count()?, 3);
        assert_eq!(storage.block_root_by_slot_count()?, 3);
        assert_eq!(storage.slot_by_state_root_count()?, 3);
        assert_eq!(storage.state_count()?, 2);

        storage.prune_old_state_roots(5)?;

        assert_eq!(storage.slot_by_state_root_count()?, 2);

        Ok(())
    }

    #[test]
    #[expect(clippy::similar_names)]
    fn test_prune_old_blob_sidecars() -> Result<()> {
        let database = Database::persistent(
            "test_db",
            TempDir::new()?,
            ByteSize::mib(10),
            DatabaseMode::ReadWrite,
            None,
        )?;

        let storage = Storage::<Mainnet>::new(
            Arc::new(Config::mainnet()),
            Arc::new(PubkeyCache::default()),
            database,
            nonzero!(64_u64),
            StorageMode::default(),
            true,
        );

        let blob_id_0 = BlobIdentifier {
            block_root: H256::zero(),
            index: 0,
        };

        // slot 5
        let blob_id_5 = BlobIdentifier {
            block_root: H256::zero(),
            index: 1,
        };

        let mut blob_sidecar_5 = BlobSidecar::default();
        blob_sidecar_5.signed_block_header.message.slot = 5;

        // slot 10
        let blob_id_10 = BlobIdentifier {
            block_root: H256::zero(),
            index: 2,
        };

        let mut blob_sidecar_10 = BlobSidecar::default();
        blob_sidecar_10.signed_block_header.message.slot = 10;

        let blob_sidecars = vec![
            BlobSidecarWithId {
                blob_sidecar: Arc::new(BlobSidecar::default()),
                blob_id: blob_id_0,
            },
            BlobSidecarWithId {
                blob_sidecar: Arc::new(blob_sidecar_5),
                blob_id: blob_id_5,
            },
            BlobSidecarWithId {
                blob_sidecar: Arc::new(blob_sidecar_10),
                blob_id: blob_id_10,
            },
        ];

        let persisted = storage.append_blob_sidecars(blob_sidecars)?;

        assert_eq!(persisted, vec![blob_id_0, blob_id_5, blob_id_10]);
        assert_eq!(storage.slot_by_blob_id_count()?, 3);
        assert_eq!(storage.blob_sidecar_by_blob_id_count()?, 3);

        storage.prune_old_blob_sidecars(6)?;

        assert_eq!(storage.slot_by_blob_id_count()?, 1);
        assert_eq!(storage.blob_sidecar_by_blob_id_count()?, 1);

        Ok(())
    }

    #[test]
    fn test_block_root_before_or_at_slot() -> Result<()> {
        let database = Database::in_memory();

        database.put_batch(vec![
            serialize(BlockRootBySlot(2), H256::repeat_byte(2))?,
            serialize(BlockRootBySlot(6), H256::repeat_byte(6))?,
        ])?;

        let storage = Storage::<Mainnet>::new(
            Arc::new(Config::mainnet()),
            Arc::new(PubkeyCache::default()),
            database,
            nonzero!(64_u64),
            StorageMode::default(),
            true,
        );

        assert_eq!(storage.block_root_before_or_at_slot(1)?, None);
        assert_eq!(
            storage.block_root_before_or_at_slot(2)?,
            Some(H256::repeat_byte(2)),
        );
        assert_eq!(
            storage.block_root_before_or_at_slot(3)?,
            Some(H256::repeat_byte(2)),
        );
        assert_eq!(
            storage.block_root_before_or_at_slot(6)?,
            Some(H256::repeat_byte(6)),
        );
        assert_eq!(
            storage.block_root_before_or_at_slot(9)?,
            Some(H256::repeat_byte(6)),
        );

        Ok(())
    }

    #[test]
    fn finalized_block_key_round_trips() -> Result<()> {
        let block_root = H256::repeat_byte(7);

        for key in [
            FinalizedBlockByRoot::full(block_root),
            FinalizedBlockByRoot::blinded(block_root),
        ] {
            let string = key.to_string();
            let parsed = FinalizedBlockByRoot::try_from(string.as_bytes())?;

            assert_eq!(parsed, key);
        }

        assert_eq!(
            FinalizedBlockByRoot::blinded(block_root).to_string(),
            format!(
                "{}{BLINDED_BLOCK_SUFFIX}",
                FinalizedBlockByRoot::full(block_root)
            ),
        );

        FinalizedBlockByRoot::try_from(UnfinalizedBlockByRoot(block_root).to_string().as_bytes())
            .expect_err("an unfinalized block key is not a finalized block key");

        Ok(())
    }

    #[test]
    fn post_merge_finalized_blocks_are_stored_blinded_without_payload_storage() -> Result<()> {
        let storage = storage_with_payload_setting(false);

        for phase in BLINDABLE_PHASES {
            let block = block_at_phase(phase);
            let block_root = block.message().hash_tree_root();
            let blinded = SignedBlindedBeaconBlock::try_from(block.as_ref().clone())?;

            let (key, _) = storage.serialize_finalized_block(block_root, &block)?;

            assert_eq!(key, FinalizedBlockByRoot::blinded(block_root).to_string());

            storage
                .database
                .put_batch(vec![storage.serialize_finalized_block(block_root, &block)?])?;

            assert!(storage.contains_finalized_block(block_root)?);
            assert_eq!(
                storage
                    .database
                    .get(FinalizedBlockByRoot::full(block_root).to_string())?,
                None,
            );

            let stored = storage
                .database
                .get(key)?
                .expect("blinded block was just stored");

            assert_eq!(stored, blinded.to_ssz()?);
            assert_eq!(
                SignedBlindedBeaconBlock::<Mainnet>::from_ssz(&phase, stored.as_slice())?
                    .message()
                    .hash_tree_root(),
                block.message().hash_tree_root(),
            );
        }

        Ok(())
    }

    #[test]
    fn finalized_blocks_are_stored_whole_with_payload_storage() -> Result<()> {
        let storage = storage_with_payload_setting(true);

        for phase in BLINDABLE_PHASES.into_iter().chain(UNBLINDABLE_PHASES) {
            let block = block_at_phase(phase);
            let block_root = block.message().hash_tree_root();

            assert_eq!(
                storage.serialize_finalized_block(block_root, &block)?,
                serialize(FinalizedBlockByRoot::full(block_root), &block)?,
            );
        }

        Ok(())
    }

    #[test]
    fn blocks_without_blindable_payload_are_stored_whole() -> Result<()> {
        let storage = storage_with_payload_setting(false);

        for phase in UNBLINDABLE_PHASES {
            let block = block_at_phase(phase);
            let block_root = block.message().hash_tree_root();

            assert_eq!(
                storage.serialize_finalized_block(block_root, &block)?,
                serialize(FinalizedBlockByRoot::full(block_root), &block)?,
            );
        }

        Ok(())
    }

    // Blinded block processing assumes every blinded block is post-Merge, so a pre-Merge Bellatrix
    // block that got blinded could never be replayed again.
    #[test]
    fn pre_merge_blocks_are_stored_whole() -> Result<()> {
        let storage = storage_with_payload_setting(false);
        let block = Arc::new(
            BeaconBlock::<Mainnet>::Bellatrix(BellatrixBeaconBlock::default().into())
                .with_signature(SignatureBytes::default()),
        );
        let block_root = block.message().hash_tree_root();

        assert_eq!(
            storage.serialize_finalized_block(block_root, &block)?,
            serialize(FinalizedBlockByRoot::full(block_root), &block)?,
        );

        Ok(())
    }

    #[test]
    fn test_prune_old_blocks_and_states_removes_blinded_blocks() -> Result<()> {
        let storage = storage_with_payload_setting(false);
        let block = block_at_phase(Phase::Deneb);
        let blinded = SignedBlindedBeaconBlock::try_from(block.as_ref().clone())?;

        storage.database.put_batch(vec![
            serialize(BlockRootBySlot(1), H256::repeat_byte(1))?,
            serialize(
                FinalizedBlockByRoot::blinded(H256::repeat_byte(1)),
                &blinded,
            )?,
            serialize(BlockRootBySlot(3), H256::repeat_byte(3))?,
            serialize(FinalizedBlockByRoot::full(H256::repeat_byte(3)), &block)?,
            serialize(BlockRootBySlot(9), H256::repeat_byte(9))?,
            serialize(
                FinalizedBlockByRoot::blinded(H256::repeat_byte(9)),
                &blinded,
            )?,
        ])?;

        assert_eq!(storage.finalized_block_count()?, 3);
        assert!(storage.contains_finalized_block(H256::repeat_byte(1))?);

        storage.prune_old_blocks_and_states(5)?;

        // Blinded and full blocks alike are pruned; the block in slot 9 is newer than the
        // pruning threshold and stays.
        assert_eq!(storage.finalized_block_count()?, 1);
        assert!(!storage.contains_finalized_block(H256::repeat_byte(1))?);
        assert!(!storage.contains_finalized_block(H256::repeat_byte(3))?);
        assert!(storage.contains_finalized_block(H256::repeat_byte(9))?);

        Ok(())
    }

    // Every graceful shutdown persists the finalized chain up to `last_finalized`, while the
    // checkpoint state is written at the newest finalized epoch start, so finalized blocks
    // routinely sit above the checkpoint state and are stored blinded. `load` has to hand them to
    // the caller as they are instead of demanding their payloads.
    #[tokio::test]
    async fn load_hands_out_blinded_blocks_persisted_after_the_checkpoint_state() -> Result<()> {
        let config = Arc::new(Config::minimal().start_and_stay_in(Phase::Bellatrix));
        let pubkey_cache = Arc::new(PubkeyCache::default());
        let (genesis_state, _) = factory::min_genesis_state::<Minimal>(&config, &pubkey_cache)?;
        let anchor_checkpoint_provider =
            AnchorCheckpointProvider::custom_from_genesis(genesis_state.clone_arc());
        let genesis_block = anchor_checkpoint_provider.checkpoint().value.block;
        let genesis_block_root = genesis_block.message().hash_tree_root();

        let payload = factory::execution_payload(
            &config,
            &genesis_state,
            1,
            ExecutionBlockHash::repeat_byte(1),
        )?;

        let (block, _) = factory::block_with_payload(
            &config,
            &pubkey_cache,
            genesis_state.clone_arc(),
            1,
            H256::zero(),
            payload,
        )?;

        let block_root = block.message().hash_tree_root();

        let storage = Storage::<Minimal>::new(
            config,
            pubkey_cache,
            Database::in_memory(),
            DEFAULT_ARCHIVAL_EPOCH_INTERVAL,
            StorageMode::default(),
            false,
        );

        let mut batch = vec![
            serialize(
                StateCheckpoint::<Minimal>::KEY,
                StateCheckpoint {
                    block_root: genesis_block_root,
                    head_slot: 1,
                    state: genesis_state.clone_arc(),
                },
            )?,
            serialize(
                BlockCheckpoint::<Minimal>::KEY,
                BlockCheckpoint {
                    block: genesis_block,
                },
            )?,
            serialize(BlockRootBySlot(1), block_root)?,
            storage.serialize_finalized_block(block_root, &block)?,
        ];

        storage
            .append_finalized_validator_pubkeys_to_batch(&mut batch, genesis_state.validators())?;

        storage.database.put_batch(batch)?;

        let ((_, anchor_block, blocks), loaded_from_remote) = storage
            .load(
                &Client::new(),
                StateLoadStrategy::Auto {
                    state_slot: None,
                    checkpoint_sync_url: None,
                    anchor_checkpoint_provider,
                },
            )
            .await?;

        assert!(!loaded_from_remote);
        assert_eq!(anchor_block.message().hash_tree_root(), genesis_block_root);

        let blinded_flags = blocks
            .map(|result| result.map(|block| block.is_blinded()))
            .collect::<Result<Vec<_>>>()?;

        assert_eq!(blinded_flags, [true]);

        Ok(())
    }

    fn blinded_block_at_phase(phase: Phase) -> Arc<SignedBlindedBeaconBlock<Mainnet>> {
        let block = block_at_phase(phase);

        Arc::new(
            SignedBlindedBeaconBlock::try_from(block.as_ref().clone())
                .expect("phase has a blindable payload"),
        )
    }

    #[test]
    fn finalized_blocks_are_read_back_in_the_form_they_were_stored() -> Result<()> {
        let storage = storage_with_config(Config::mainnet().start_and_stay_in(Phase::Deneb), false);

        let full_root = H256::repeat_byte(1);
        let blinded_root = H256::repeat_byte(2);
        let full = block_at_phase(Phase::Deneb);
        let blinded = blinded_block_at_phase(Phase::Deneb);

        storage.database.put_batch(vec![
            // A database written by an older version stores blocks under the bare key.
            serialize(FinalizedBlockByRoot::full(full_root), &full)?,
            serialize(FinalizedBlockByRoot::blinded(blinded_root), &blinded)?,
        ])?;

        let stored = storage
            .finalized_block_by_root(full_root)?
            .expect("full block was just stored");

        assert!(!stored.is_blinded());
        assert_eq!(stored.into_full()?.to_ssz()?, full.to_ssz()?);

        let stored = storage
            .finalized_block_by_root(blinded_root)?
            .expect("blinded block was just stored");

        assert!(stored.is_blinded());
        assert_eq!(
            stored.message().hash_tree_root(),
            blinded.message().hash_tree_root(),
        );
        stored
            .into_full()
            .expect_err("a blinded block has no payload to hand out");

        assert!(
            storage
                .finalized_block_by_root(H256::repeat_byte(3))?
                .is_none()
        );

        Ok(())
    }

    #[test]
    fn unfinalized_blocks_are_not_mistaken_for_finalized_ones() -> Result<()> {
        let storage = storage_with_config(Config::mainnet().start_and_stay_in(Phase::Deneb), false);

        let block_root = H256::repeat_byte(0xff);
        let block = block_at_phase(Phase::Deneb);

        storage
            .database
            .put_batch(vec![serialize(UnfinalizedBlockByRoot(block_root), &block)?])?;

        assert!(storage.finalized_block_by_root(block_root)?.is_none());

        Ok(())
    }

    #[test]
    fn finalized_block_by_slot_resolves_blinded_blocks() -> Result<()> {
        let storage =
            storage_with_config(Config::mainnet().start_and_stay_in(Phase::Capella), false);

        let block_root = H256::repeat_byte(4);
        let blinded = blinded_block_at_phase(Phase::Capella);

        storage.database.put_batch(vec![
            serialize(BlockRootBySlot(7), block_root)?,
            serialize(FinalizedBlockByRoot::blinded(block_root), &blinded)?,
        ])?;

        let (stored, root) = storage
            .finalized_block_by_slot(7)?
            .expect("blinded block was just stored");

        assert_eq!(root, block_root);
        assert!(stored.is_blinded());

        Ok(())
    }

    #[test]
    fn blocks_by_roots_carries_both_stored_forms() -> Result<()> {
        let storage =
            storage_with_config(Config::mainnet().start_and_stay_in(Phase::Electra), false);

        let blinded_root = H256::repeat_byte(5);
        let full_root = H256::repeat_byte(6);
        let unfinalized_root = H256::repeat_byte(7);
        let block = block_at_phase(Phase::Electra);
        let blinded = blinded_block_at_phase(Phase::Electra);

        storage.database.put_batch(vec![
            serialize(FinalizedBlockByRoot::blinded(blinded_root), &blinded)?,
            serialize(FinalizedBlockByRoot::full(full_root), &block)?,
            serialize(UnfinalizedBlockByRoot(unfinalized_root), &block)?,
        ])?;

        let blinded_flags = storage
            .blocks_by_roots(vec![blinded_root, full_root, unfinalized_root])
            .map(|result| result.map(|block| block.is_blinded()))
            .collect::<Result<Vec<_>>>()?;

        assert_eq!(blinded_flags, [true, false, false]);

        storage
            .blocks_by_roots(vec![H256::repeat_byte(8)])
            .next()
            .expect("iterator yields one item per requested root")
            .expect_err("an unknown block root has no stored block");

        Ok(())
    }

    #[test]
    fn replaying_a_blinded_block_yields_the_same_state_as_the_full_one() -> Result<()> {
        let config = Arc::new(Config::minimal().start_and_stay_in(Phase::Bellatrix));
        let pubkey_cache = Arc::new(PubkeyCache::default());
        let (genesis_state, _) = factory::min_genesis_state::<Minimal>(&config, &pubkey_cache)?;

        let payload = factory::execution_payload(
            &config,
            &genesis_state,
            1,
            ExecutionBlockHash::repeat_byte(1),
        )?;

        let (block, post_state) = factory::block_with_payload(
            &config,
            &pubkey_cache,
            genesis_state.clone_arc(),
            1,
            H256::zero(),
            payload,
        )?;

        let storage = Storage::<Minimal>::new(
            config,
            pubkey_cache,
            Database::in_memory(),
            DEFAULT_ARCHIVAL_EPOCH_INTERVAL,
            StorageMode::default(),
            false,
        );

        let blinded = Arc::new(SignedBlindedBeaconBlock::try_from(block.as_ref().clone())?);

        let mut replayed_from_full = genesis_state.clone_arc();
        storage.replay_block(
            replayed_from_full.make_mut(),
            StoredBlock::Full(block.clone_arc()),
            StateRootPolicy::Trust,
        )?;

        let mut replayed_from_blinded = genesis_state;
        storage.replay_block(
            replayed_from_blinded.make_mut(),
            StoredBlock::Blinded(blinded),
            StateRootPolicy::Trust,
        )?;

        assert_eq!(replayed_from_full.to_ssz()?, post_state.to_ssz()?);
        assert_eq!(replayed_from_blinded.to_ssz()?, post_state.to_ssz()?);
        assert_eq!(
            replayed_from_blinded.hash_tree_root(),
            replayed_from_full.hash_tree_root(),
        );

        Ok(())
    }

    // Back-sync archiving replays stored blocks with `StateRootPolicy::Verify`, so blinded blocks
    // have to produce a state whose root matches the one they carry.
    #[test]
    fn verified_replay_of_a_blinded_block_checks_the_state_root() -> Result<()> {
        let config = Arc::new(Config::minimal().start_and_stay_in(Phase::Bellatrix));
        let pubkey_cache = Arc::new(PubkeyCache::default());
        let (genesis_state, _) = factory::min_genesis_state::<Minimal>(&config, &pubkey_cache)?;

        let payload = factory::execution_payload(
            &config,
            &genesis_state,
            1,
            ExecutionBlockHash::repeat_byte(1),
        )?;

        let (block, post_state) = factory::block_with_payload(
            &config,
            &pubkey_cache,
            genesis_state.clone_arc(),
            1,
            H256::zero(),
            payload,
        )?;

        let storage = Storage::<Minimal>::new(
            config,
            pubkey_cache,
            Database::in_memory(),
            DEFAULT_ARCHIVAL_EPOCH_INTERVAL,
            StorageMode::default(),
            false,
        );

        let blinded = SignedBlindedBeaconBlock::try_from(block.as_ref().clone())?;

        let mut replayed = genesis_state.clone_arc();
        storage.replay_block(
            replayed.make_mut(),
            StoredBlock::Blinded(Arc::new(blinded.clone())),
            StateRootPolicy::Verify,
        )?;

        assert_eq!(replayed.to_ssz()?, post_state.to_ssz()?);

        let mut tampered = blinded;

        match &mut tampered {
            SignedBlindedBeaconBlock::Bellatrix(block) => {
                block.message.state_root = H256::repeat_byte(0xff);
            }
            _ => unreachable!("block was built in Bellatrix"),
        }

        let mut replayed = genesis_state;

        storage
            .replay_block(
                replayed.make_mut(),
                StoredBlock::Blinded(Arc::new(tampered)),
                StateRootPolicy::Verify,
            )
            .expect_err("the tampered state root should not verify");

        Ok(())
    }

    // Blinded block processing never sees the signature, so `StateRootPolicy::Verify` has to check
    // the proposer signature separately to match what full block processing does.
    #[test]
    fn verified_replay_of_a_blinded_block_checks_the_block_signature() -> Result<()> {
        let config = Arc::new(Config::minimal().start_and_stay_in(Phase::Bellatrix));
        let pubkey_cache = Arc::new(PubkeyCache::default());
        let (genesis_state, _) = factory::min_genesis_state::<Minimal>(&config, &pubkey_cache)?;

        let payload = factory::execution_payload(
            &config,
            &genesis_state,
            1,
            ExecutionBlockHash::repeat_byte(1),
        )?;

        let (block, _) = factory::block_with_payload(
            &config,
            &pubkey_cache,
            genesis_state.clone_arc(),
            1,
            H256::zero(),
            payload,
        )?;

        let storage = Storage::<Minimal>::new(
            config,
            pubkey_cache,
            Database::in_memory(),
            DEFAULT_ARCHIVAL_EPOCH_INTERVAL,
            StorageMode::default(),
            false,
        );

        let mut tampered = SignedBlindedBeaconBlock::try_from(block.as_ref().clone())?;

        match &mut tampered {
            SignedBlindedBeaconBlock::Bellatrix(block) => {
                block.signature = SignatureBytes::default();
            }
            _ => unreachable!("block was built in Bellatrix"),
        }

        let mut replayed = genesis_state;

        storage
            .replay_block(
                replayed.make_mut(),
                StoredBlock::Blinded(Arc::new(tampered)),
                StateRootPolicy::Verify,
            )
            .expect_err("the tampered block signature should not verify");

        Ok(())
    }

    #[test]
    fn back_synced_blocks_are_stored_in_the_configured_form() -> Result<()> {
        for store_payloads in [false, true] {
            let storage = storage_with_config(
                Config::mainnet().start_and_stay_in(Phase::Deneb),
                store_payloads,
            );

            let block = block_at_phase(Phase::Deneb);
            let block_root = block.message().hash_tree_root();
            let slot = block.message().slot();

            storage.store_back_sync_blocks(core::iter::once(block.clone_arc()))?;

            assert_eq!(storage.block_root_by_slot(slot)?, Some(block_root));

            let stored = storage
                .finalized_block_by_root(block_root)?
                .expect("back-synced block was just stored");

            assert_eq!(stored.is_blinded(), !store_payloads);
            assert_eq!(
                stored.message().hash_tree_root(),
                block.message().hash_tree_root(),
            );
        }

        Ok(())
    }

    // Turning `--store-payloads` back on must not strand the blocks written while it was off.
    #[test]
    fn blinded_blocks_stay_readable_after_payload_storage_is_re_enabled() -> Result<()> {
        let config = Config::mainnet().start_and_stay_in(Phase::Deneb);
        let blinded_root = H256::repeat_byte(1);
        let full_root = H256::repeat_byte(2);
        let blinded = blinded_block_at_phase(Phase::Deneb);
        let full = block_at_phase(Phase::Deneb);
        let database = Database::in_memory();

        database.put_batch(vec![
            serialize(BlockRootBySlot(3), blinded_root)?,
            serialize(FinalizedBlockByRoot::blinded(blinded_root), &blinded)?,
        ])?;

        let storage = Storage::<Mainnet>::new(
            Arc::new(config),
            Arc::new(PubkeyCache::default()),
            database,
            DEFAULT_ARCHIVAL_EPOCH_INTERVAL,
            StorageMode::default(),
            true,
        );

        // Blocks appended after the restart are stored whole, next to the blinded ones.
        storage
            .database
            .put_batch(vec![storage.serialize_finalized_block(full_root, &full)?])?;

        let stored = storage
            .finalized_block_by_root(blinded_root)?
            .expect("blinded block was stored before payload storage was re-enabled");

        assert!(stored.is_blinded());
        assert_eq!(
            stored.message().hash_tree_root(),
            blinded.message().hash_tree_root(),
        );

        let (stored, root) = storage
            .finalized_block_by_slot(3)?
            .expect("blinded block was stored before payload storage was re-enabled");

        assert_eq!(root, blinded_root);
        assert!(stored.is_blinded());

        let blinded_flags = storage
            .blocks_by_roots(vec![blinded_root, full_root])
            .map(|result| result.map(|block| block.is_blinded()))
            .collect::<Result<Vec<_>>>()?;

        assert_eq!(blinded_flags, [true, false]);

        Ok(())
    }
}
