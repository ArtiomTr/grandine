use std::sync::Arc;

use anyhow::Error as AnyhowError;
use eth1_api::ApiController;
use fork_choice_control::{StoredBlock, Wait};
use genesis::AnchorCheckpointProvider;
use http_api_utils::BlockId;
use tracing::instrument;
use types::{
    combined::SignedBlindedBeaconBlock, nonstandard::WithStatus, phase0::primitives::H256,
    preset::Preset, traits::SignedBeaconBlock as _,
};

use crate::error::Error;

/// The block as it is stored, which may be blinded and therefore carry no execution payload.
///
/// Endpoints that only need block metadata should use this to avoid an execution client round trip.
#[instrument(level = "debug", skip_all, fields(%block_id))]
pub fn stored_block<P: Preset, W: Wait>(
    block_id: BlockId,
    controller: &ApiController<P, W>,
    anchor_checkpoint_provider: &AnchorCheckpointProvider<P>,
) -> Result<WithStatus<StoredBlock<P>>, Error> {
    match block_id {
        BlockId::Head => Some(controller.head_block().map(StoredBlock::Full)),
        BlockId::Genesis => anchor_checkpoint_provider
            .checkpoint()
            .genesis()
            .map(|checkpoint| StoredBlock::Full(checkpoint.block))
            .map(WithStatus::valid_and_finalized),
        BlockId::Finalized => Some(controller.last_finalized_block().map(StoredBlock::Full)),
        BlockId::Slot(slot) => controller
            .block_by_slot(slot)?
            .map(|with_status| with_status.map(|block_with_root| block_with_root.block)),
        BlockId::Root(root) => controller.block_by_root(root)?,
    }
    .ok_or(Error::BlockNotFound)
}

/// Blinds a stored block, which is free when it is already stored blinded.
pub fn into_blinded<P: Preset>(
    block: StoredBlock<P>,
) -> Result<SignedBlindedBeaconBlock<P>, Error> {
    match block {
        StoredBlock::Full(block) => Arc::unwrap_or_clone(block)
            .try_into()
            .map_err(AnyhowError::new)
            .map_err(Error::Internal),
        StoredBlock::Blinded(block) => Ok(Arc::unwrap_or_clone(block)),
    }
}

#[instrument(level = "debug", skip_all, fields(%block_id))]
pub fn block_root<P: Preset, W: Wait>(
    block_id: BlockId,
    controller: &ApiController<P, W>,
    anchor_checkpoint_provider: &AnchorCheckpointProvider<P>,
) -> Result<WithStatus<H256>, Error> {
    match block_id {
        BlockId::Head => Some(controller.head_block_root()),
        BlockId::Genesis => anchor_checkpoint_provider
            .checkpoint()
            .genesis()
            .map(|checkpoint| checkpoint.block.message().hash_tree_root())
            .map(WithStatus::valid_and_finalized),
        BlockId::Finalized => Some(controller.last_finalized_block_root()),
        BlockId::Slot(slot) => controller
            .block_by_slot(slot)?
            .map(|with_status| with_status.map(|with_status| with_status.root)),
        BlockId::Root(root) => controller.check_block_root(root)?,
    }
    .ok_or(Error::BlockNotFound)
}
