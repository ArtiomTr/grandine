use std::sync::Arc;

use anyhow::Error as AnyhowError;
use eth1_api::{ApiController, Eth1Api, PayloadReconstructionError, reconstruct_stored_blocks};
use fork_choice_control::{StoredBlock, Wait};
use genesis::AnchorCheckpointProvider;
use http_api_utils::BlockId;
use tracing::instrument;
use types::{
    combined::{SignedBeaconBlock, SignedBlindedBeaconBlock},
    nonstandard::WithStatus,
    phase0::primitives::H256,
    preset::Preset,
    traits::SignedBeaconBlock as _,
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

#[instrument(level = "debug", skip_all, fields(%block_id))]
pub async fn block<P: Preset, W: Wait>(
    block_id: BlockId,
    controller: &ApiController<P, W>,
    anchor_checkpoint_provider: &AnchorCheckpointProvider<P>,
    eth1_api: &Eth1Api,
) -> Result<WithStatus<Arc<SignedBeaconBlock<P>>>, Error> {
    let WithStatus {
        value,
        status,
        finalized,
    } = stored_block(block_id, controller, anchor_checkpoint_provider)?;

    Ok(WithStatus {
        value: reconstruct(eth1_api, value).await?,
        status,
        finalized,
    })
}

/// Rebuilds the execution payload of a blinded stored block from the execution client.
///
/// A body the execution client no longer has is reported as a temporary failure rather than as an
/// internal error: the block is intact on our side and the operator can restore the response by
/// pointing us at an execution client that retains bodies.
pub async fn reconstruct<P: Preset>(
    eth1_api: &Eth1Api,
    block: StoredBlock<P>,
) -> Result<Arc<SignedBeaconBlock<P>>, Error> {
    if let StoredBlock::Full(block) = block {
        return Ok(block);
    }

    let blocks = reconstruct_stored_blocks(eth1_api, [block])
        .await
        .map_err(
            |error| match error.downcast_ref::<PayloadReconstructionError>() {
                Some(PayloadReconstructionError::BodyMissing { .. }) => {
                    Error::PayloadBodyUnavailable
                }
                _ => Error::Internal(error),
            },
        )?;

    blocks.into_iter().next().ok_or(Error::BlockNotFound)
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

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use axum::response::IntoResponse as _;
    use bls::SignatureBytes;
    use httpmock::{Method, MockServer};
    use reqwest::{Client, StatusCode};
    use serde_json::{Value, json};
    use std_ext::ArcExt as _;
    use types::{
        combined::BeaconBlock,
        config::Config,
        deneb::containers::{
            BeaconBlock as DenebBeaconBlock, BeaconBlockBody as DenebBeaconBlockBody,
            ExecutionPayload as DenebExecutionPayload,
        },
        phase0::primitives::ExecutionBlockNumber,
        preset::Mainnet,
    };

    use super::*;

    const BLOCK_NUMBER: ExecutionBlockNumber = 17_000_000;

    #[tokio::test]
    async fn reconstructed_block_is_identical_to_the_stored_full_one() -> Result<()> {
        let block = full_block();

        let (_server, eth1_api) = eth1_api_serving(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": [{ "transactions": [], "withdrawals": [] }],
        }));

        let reconstructed = reconstruct(&eth1_api, blinded(&block)?).await?;

        assert_eq!(reconstructed, block);

        Ok(())
    }

    #[tokio::test]
    async fn reconstruction_of_a_full_block_does_not_call_the_execution_client() -> Result<()> {
        let block = full_block();

        let server = MockServer::start();

        let mock = server.mock(|when, then| {
            when.method(Method::POST).path("/");
            then.status(500);
        });

        let eth1_api = eth1_api_connected_to(&server);
        let reconstructed = reconstruct(&eth1_api, StoredBlock::Full(block.clone_arc())).await?;

        assert_eq!(reconstructed, block);
        assert_eq!(mock.calls(), 0);

        Ok(())
    }

    #[tokio::test]
    async fn blinding_a_stored_block_never_needs_the_execution_client() -> Result<()> {
        let block = full_block();
        let stored = blinded(&block)?;

        let StoredBlock::Blinded(expected) = stored.clone() else {
            panic!("stored block should be blinded");
        };

        let expected = expected.message().hash_tree_root();

        assert_eq!(into_blinded(stored)?.message().hash_tree_root(), expected);
        assert_eq!(
            into_blinded(StoredBlock::Full(block))?
                .message()
                .hash_tree_root(),
            expected,
        );

        Ok(())
    }

    #[tokio::test]
    async fn missing_payload_body_is_reported_as_unavailable() -> Result<()> {
        let block = full_block();

        let (_server, eth1_api) = eth1_api_serving(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": [null],
        }));

        let error = reconstruct(&eth1_api, blinded(&block)?)
            .await
            .expect_err("reconstruction should fail without a payload body");

        assert!(matches!(error, Error::PayloadBodyUnavailable));

        assert_eq!(
            error.into_response().status(),
            StatusCode::SERVICE_UNAVAILABLE,
        );

        Ok(())
    }

    fn full_block() -> Arc<SignedBeaconBlock<Mainnet>> {
        let block = BeaconBlock::Deneb(
            DenebBeaconBlock {
                body: DenebBeaconBlockBody {
                    execution_payload: DenebExecutionPayload {
                        block_number: BLOCK_NUMBER,
                        block_hash: H256::from_low_u64_be(BLOCK_NUMBER),
                        gas_limit: 30_000_000,
                        ..DenebExecutionPayload::default()
                    },
                    ..DenebBeaconBlockBody::default()
                },
                ..DenebBeaconBlock::default()
            }
            .into(),
        );

        Arc::new(block.with_signature(SignatureBytes::default()))
    }

    fn blinded(block: &Arc<SignedBeaconBlock<Mainnet>>) -> Result<StoredBlock<Mainnet>> {
        let blinded = SignedBlindedBeaconBlock::try_from(block.as_ref().clone())?;

        Ok(StoredBlock::Blinded(Arc::new(blinded)))
    }

    // The `MockServer` is returned along with the API because dropping it returns the server to
    // `httpmock`'s pool, where another test can claim it and replace the mocks this one relies on.
    fn eth1_api_serving(body: &Value) -> (MockServer, Eth1Api) {
        let server = MockServer::start();

        server.mock(|when, then| {
            when.method(Method::POST).path("/");
            then.status(200).body(body.to_string());
        });

        let eth1_api = eth1_api_connected_to(&server);

        (server, eth1_api)
    }

    fn eth1_api_connected_to(server: &MockServer) -> Eth1Api {
        Eth1Api::new(
            Arc::new(Config::mainnet()),
            Client::new(),
            Arc::default(),
            vec![server.url("/").parse().expect("mock server URL is valid")],
            None,
            None,
        )
    }
}
