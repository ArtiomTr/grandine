use core::{
    ops::RangeInclusive,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    time::Duration,
};
use std::{collections::BTreeMap, sync::Arc};

use anyhow::{Error as AnyhowError, Result, bail, ensure};
use either::Either;
use enum_iterator::Sequence as _;
use ethereum_types::H64;
use execution_engine::{
    BlobAndProofV1, BlobAndProofV2, EngineGetPayloadV1Response, EngineGetPayloadV2Response,
    EngineGetPayloadV3Response, EngineGetPayloadV4Response, EngineGetPayloadV5Response,
    EngineGetPayloadV6Response, ExecutionPayloadBodyV1, ExecutionPayloadV1, ExecutionPayloadV2,
    ExecutionPayloadV3, ExecutionPayloadV4, ForkChoiceStateV1, ForkChoiceUpdatedResponse,
    PayloadAttributes, PayloadId, PayloadStatusV1, RawExecutionRequests,
};
use futures::{Future, channel::mpsc::UnboundedSender};
use logging::{debug_with_peers, warn_with_peers};
use prometheus_metrics::Metrics;
use reqwest::{Client, header::HeaderMap};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::Value;
use static_assertions::const_assert_eq;
use std_ext::CopyExt;
use thiserror::Error;
use types::{
    combined::{ExecutionPayload, ExecutionPayloadParams},
    config::Config,
    deneb::primitives::VersionedHash,
    nonstandard::{Phase, WithBlobsAndMev},
    phase0::primitives::{ExecutionBlockHash, ExecutionBlockNumber},
    preset::Preset,
    redacting_url::RedactingUrl,
};
use web3::{
    Error as Web3Error, Transport as _, Web3,
    api::{Eth, Namespace as _},
    helpers::CallFuture,
    transports::Http,
    types::{BlockId, BlockNumber, FilterBuilder, U64},
};

use crate::{
    Eth1ApiToMetrics, Eth1ConnectionData, WithClientVersions,
    auth::Auth,
    deposit_event::DepositEvent,
    endpoints::{ClientVersions, Endpoint, Endpoints},
    eth1_api::{
        ENGINE_FORKCHOICE_UPDATED_V1, ENGINE_FORKCHOICE_UPDATED_V2, ENGINE_FORKCHOICE_UPDATED_V3,
        ENGINE_FORKCHOICE_UPDATED_V4, ENGINE_GET_EL_BLOBS_V1, ENGINE_GET_EL_BLOBS_V2,
        ENGINE_GET_PAYLOAD_BODIES_BY_HASH_V1, ENGINE_GET_PAYLOAD_BODIES_BY_RANGE_V1,
        ENGINE_GET_PAYLOAD_V1, ENGINE_GET_PAYLOAD_V2, ENGINE_GET_PAYLOAD_V3, ENGINE_GET_PAYLOAD_V4,
        ENGINE_GET_PAYLOAD_V5, ENGINE_GET_PAYLOAD_V6, ENGINE_NEW_PAYLOAD_V1, ENGINE_NEW_PAYLOAD_V2,
        ENGINE_NEW_PAYLOAD_V3, ENGINE_NEW_PAYLOAD_V4, ENGINE_NEW_PAYLOAD_V5,
    },
    eth1_block::Eth1Block,
};

const ENGINE_FORKCHOICE_UPDATED_TIMEOUT: Duration = Duration::from_secs(8);
// In some of our setups 1 second is not enough to get blobs from the execution client
const ENGINE_GET_BLOBS_TIMEOUT: Duration = Duration::from_secs(2);
const ENGINE_GET_PAYLOAD_TIMEOUT: Duration = Duration::from_secs(1);
const ENGINE_NEW_PAYLOAD_TIMEOUT: Duration = Duration::from_secs(8);
const ENGINE_GET_PAYLOAD_BODIES_TIMEOUT: Duration = Duration::from_secs(10);

/// Execution clients cap the number of payload bodies they return per request but offer no way to
/// query the cap, so start optimistically and shrink on rejection.
const PAYLOAD_BODIES_MAX_BATCH_SIZE: usize = 1024;

/// The Engine API requires execution clients to serve at least this many payload bodies per
/// request, so shrinking below it would only add round trips.
const PAYLOAD_BODIES_MIN_BATCH_SIZE: usize = 32;

/// [`Too large request`](https://github.com/ethereum/execution-apis/blob/b7c5d3420e00648f456744d121ffbd929862924d/src/engine/shanghai.md#specification-3)
const TOO_LARGE_REQUEST_ERROR_CODE: i64 = -38004;

#[expect(clippy::struct_field_names)]
pub struct Eth1Api {
    config: Arc<Config>,
    client: Client,
    pub(crate) auth: Arc<Auth>,
    pub(crate) endpoints: Endpoints,
    eth1_api_to_metrics_tx: Option<UnboundedSender<Eth1ApiToMetrics>>,
    pub(crate) metrics: Option<Arc<Metrics>>,
    payload_bodies_batch_size: AtomicUsize,
    payload_bodies_failure_reported: AtomicBool,
}

impl Eth1Api {
    #[must_use]
    pub fn new(
        config: Arc<Config>,
        client: Client,
        auth: Arc<Auth>,
        eth1_rpc_urls: Vec<RedactingUrl>,
        eth1_api_to_metrics_tx: Option<UnboundedSender<Eth1ApiToMetrics>>,
        metrics: Option<Arc<Metrics>>,
    ) -> Self {
        Self {
            config,
            client,
            auth,
            endpoints: Endpoints::new(eth1_rpc_urls),
            eth1_api_to_metrics_tx,
            metrics,
            payload_bodies_batch_size: AtomicUsize::new(PAYLOAD_BODIES_MAX_BATCH_SIZE),
            payload_bodies_failure_reported: AtomicBool::new(false),
        }
    }

    pub fn client_versions(&self) -> impl Iterator<Item = Arc<ClientVersions>> {
        self.endpoints.client_versions()
    }

    pub async fn current_head_number(&self) -> Result<ExecutionBlockNumber> {
        Ok(self
            .request_with_fallback(|(api, headers)| Ok(api.block_number(headers)), None, false)
            .await?
            .result
            .as_u64())
    }

    pub async fn get_block(&self, block_id: BlockId) -> Result<Option<Eth1Block>> {
        self.request_with_fallback(
            |(api, headers)| Ok(api.block(block_id, headers)),
            None,
            false,
        )
        .await?
        .result
        .map(Eth1Block::try_from)
        .transpose()
    }

    pub async fn get_block_by_number(
        &self,
        block_number: ExecutionBlockNumber,
    ) -> Result<Option<Eth1Block>> {
        self.get_block(U64::from(block_number).into()).await
    }

    pub async fn get_block_by_hash(
        &self,
        block_hash: ExecutionBlockHash,
    ) -> Result<Option<Eth1Block>> {
        self.get_block(block_hash.into()).await
    }

    pub async fn get_first_deposit_contract_block_number(
        &self,
    ) -> Result<Option<ExecutionBlockNumber>> {
        // `BlockNumber::Earliest` is necessary to get all logs.
        // `BlockNumber::Latest` is the default (in the JSON RPC, not in `web3`). See:
        // - <https://github.com/ethereum/wiki/wiki/JSON-RPC/b729c267fd71d9ba92ce6b90023caabc486ca5ae#eth_getlogs>
        // - <https://github.com/paritytech/wiki/blob/bc0952d26528de087993049fc72e4f6f003e688f/JSONRPC-eth-module.md#eth_newfilter>
        let filter = FilterBuilder::default()
            .from_block(BlockNumber::Earliest)
            .address(vec![self.config.deposit_contract_address])
            .limit(1)
            .build();

        let logs = self
            .request_with_fallback(
                |(api, headers)| Ok(api.logs(filter.clone(), headers)),
                None,
                false,
            )
            .await?
            .result;

        if let Some(log) = logs.first()
            && let Some(block_number) = log.block_number
        {
            return Ok(Some(block_number.as_u64()));
        }

        Ok(None)
    }

    pub(crate) async fn get_blobs_v1<P: Preset>(
        &self,
        versioned_hashes: Vec<VersionedHash>,
    ) -> Result<Vec<Option<BlobAndProofV1<P>>>> {
        let params = vec![serde_json::to_value(versioned_hashes)?];

        self.execute(
            ENGINE_GET_EL_BLOBS_V1,
            params,
            Some(ENGINE_GET_BLOBS_TIMEOUT),
            Some(ENGINE_GET_EL_BLOBS_V1),
        )
        .await
        .map(WithClientVersions::result)
    }

    pub(crate) async fn get_blobs_v2<P: Preset>(
        &self,
        versioned_hashes: Vec<VersionedHash>,
    ) -> Result<Option<Vec<BlobAndProofV2<P>>>> {
        let params = vec![serde_json::to_value(versioned_hashes)?];

        self.execute(
            ENGINE_GET_EL_BLOBS_V2,
            params,
            Some(ENGINE_GET_BLOBS_TIMEOUT),
            Some(ENGINE_GET_EL_BLOBS_V2),
        )
        .await
        .map(WithClientVersions::result)
    }

    /// Calls [`engine_getPayloadBodiesByHashV1`].
    ///
    /// The response has one entry per requested hash, `None` for blocks the execution client does
    /// not have.
    ///
    /// [`engine_getPayloadBodiesByHashV1`]: https://github.com/ethereum/execution-apis/blob/b7c5d3420e00648f456744d121ffbd929862924d/src/engine/shanghai.md#engine_getpayloadbodiesbyhashv1
    pub async fn get_payload_bodies_by_hash<P: Preset>(
        &self,
        block_hashes: &[ExecutionBlockHash],
    ) -> Result<Vec<Option<ExecutionPayloadBodyV1<P>>>> {
        let mut bodies = Vec::with_capacity(block_hashes.len());
        let mut remaining = block_hashes;

        while !remaining.is_empty() {
            let batch_size = self.payload_bodies_batch_size().min(remaining.len());
            let (batch, rest) = remaining.split_at(batch_size);
            let params = vec![serde_json::to_value(batch)?];

            let returned = match self
                .execute::<Vec<Option<ExecutionPayloadBodyV1<P>>>>(
                    ENGINE_GET_PAYLOAD_BODIES_BY_HASH_V1,
                    params,
                    Some(ENGINE_GET_PAYLOAD_BODIES_TIMEOUT),
                    // Deliberately unfiltered by capability, unlike `get_blobs`: blocks stored
                    // blinded cannot be served at all without these methods, and capabilities are
                    // only known after the first exchange, which happens after the blocks
                    // persisted by the previous run are reconstructed.
                    None,
                )
                .await
            {
                Ok(returned) => returned.result,
                Err(error) => {
                    if self.shrink_payload_bodies_batch_size(&error, batch_size) {
                        continue;
                    }

                    return Err(error);
                }
            };

            ensure!(
                returned.len() == batch.len(),
                Error::PayloadBodyCountMismatch {
                    requested: batch.len(),
                    returned: returned.len(),
                },
            );

            bodies.extend(returned);
            remaining = rest;
        }

        Ok(bodies)
    }

    /// Calls [`engine_getPayloadBodiesByRangeV1`].
    ///
    /// The response may be shorter than `count` if the range extends past the latest block known to
    /// the execution client.
    ///
    /// [`engine_getPayloadBodiesByRangeV1`]: https://github.com/ethereum/execution-apis/blob/b7c5d3420e00648f456744d121ffbd929862924d/src/engine/shanghai.md#engine_getpayloadbodiesbyrangev1
    pub async fn get_payload_bodies_by_range<P: Preset>(
        &self,
        start: ExecutionBlockNumber,
        count: u64,
    ) -> Result<Vec<Option<ExecutionPayloadBodyV1<P>>>> {
        let mut bodies = vec![];
        let mut next = start;
        let mut remaining = count;

        while remaining > 0 {
            let batch_size = u64::try_from(self.payload_bodies_batch_size())?.min(remaining);
            let params = vec![
                format!("{next:#x}").into(),
                format!("{batch_size:#x}").into(),
            ];

            let returned = match self
                .execute::<Vec<Option<ExecutionPayloadBodyV1<P>>>>(
                    ENGINE_GET_PAYLOAD_BODIES_BY_RANGE_V1,
                    params,
                    Some(ENGINE_GET_PAYLOAD_BODIES_TIMEOUT),
                    None,
                )
                .await
            {
                Ok(returned) => returned.result,
                Err(error) => {
                    if self.shrink_payload_bodies_batch_size(&error, usize::try_from(batch_size)?) {
                        continue;
                    }

                    return Err(error);
                }
            };

            let returned_count = u64::try_from(returned.len())?;

            ensure!(
                returned_count <= batch_size,
                Error::PayloadBodyCountMismatch {
                    requested: usize::try_from(batch_size)?,
                    returned: returned.len(),
                },
            );

            bodies.extend(returned);

            // A short response means the execution client has no more blocks to serve.
            if returned_count < batch_size {
                break;
            }

            next = next.saturating_add(returned_count);
            remaining = remaining.saturating_sub(returned_count);
        }

        Ok(bodies)
    }

    fn payload_bodies_batch_size(&self) -> usize {
        self.payload_bodies_batch_size.load(Ordering::Relaxed)
    }

    /// Halves the cached batch size if `error` says the request was too large.
    ///
    /// Returns `true` if the failed request should be retried with the smaller size.
    fn shrink_payload_bodies_batch_size(&self, error: &AnyhowError, attempted: usize) -> bool {
        let too_large = error.chain().any(|cause| {
            matches!(
                cause.downcast_ref::<Web3Error>(),
                Some(Web3Error::Rpc(rpc_error))
                    if rpc_error.code.code() == TOO_LARGE_REQUEST_ERROR_CODE,
            )
        });

        if !too_large || attempted <= PAYLOAD_BODIES_MIN_BATCH_SIZE {
            return false;
        }

        let reduced = (attempted / 2).max(PAYLOAD_BODIES_MIN_BATCH_SIZE);

        self.payload_bodies_batch_size
            .store(reduced, Ordering::Relaxed);

        true
    }

    pub async fn get_blocks(
        &self,
        block_number_range: RangeInclusive<ExecutionBlockNumber>,
    ) -> Result<Vec<Eth1Block>> {
        let mut deposit_data = self.get_deposit_events(block_number_range.clone()).await?;
        let mut blocks = vec![];

        for block_number in block_number_range {
            if let Some(block) = self.get_block_by_number(block_number).await? {
                let deposit_events = deposit_data.remove(&block_number).unwrap_or_default();
                let eth1_block = Eth1Block {
                    deposit_events: deposit_events.try_into()?,
                    ..block
                };
                blocks.push(eth1_block);
            }
        }

        Ok(blocks)
    }

    pub async fn get_deposit_events(
        &self,
        block_number_range: RangeInclusive<ExecutionBlockNumber>,
    ) -> Result<BTreeMap<ExecutionBlockNumber, Vec<DepositEvent>>> {
        // Sepolia uses a custom contract that emits events other than `DepositEvent`. See:
        // - <https://github.com/ethereum/pm/issues/526>
        // - <https://github.com/protolambda/testnet-dep-contract/blob/8df70175dca186b74197ec830450c4b988861746/deposit_contract.sol>
        // - <https://notes.ethereum.org/zvkfSmYnT0-uxwwEegbCqg>
        // - <https://sepolia.etherscan.io/address/0x7f02C3E3c98b133055B8B348B2Ac625669Ed295D#events>
        // - <https://sepolia.etherscan.io/token/0x7f02C3E3c98b133055B8B348B2Ac625669Ed295D>
        let filter = FilterBuilder::default()
            .from_block(block_number_range.start().copy().into())
            .to_block(block_number_range.end().copy().into())
            .address(vec![self.config.deposit_contract_address])
            .topics(Some(vec![DepositEvent::TOPIC]), None, None, None)
            .build();

        let mut deposit_events = BTreeMap::<_, Vec<_>>::new();

        for log in self
            .request_with_fallback(
                |(api, headers)| Ok(api.logs(filter.clone(), headers)),
                None,
                false,
            )
            .await?
            .result
        {
            let block_number = match log.block_number {
                Some(block_number) => block_number.as_u64(),
                None => continue,
            };

            let deposit_event = DepositEvent::try_from(log)?;

            deposit_events
                .entry(block_number)
                .or_default()
                .push(deposit_event);
        }

        Ok(deposit_events)
    }

    /// Calls [`engine_newPayloadV1`] or [`engine_newPayloadV2`] or [`engine_newPayloadV3`] or [`engine_newPayloadV4`] or [`engine_newPayloadV5`] depending on `payload`.
    ///
    /// Later versions of `engine_newPayload` accept parameters of all prior versions,
    /// but using the earlier versions allows the application to work with old execution clients.
    ///
    /// [`engine_newPayloadV1`]: https://github.com/ethereum/execution-apis/blob/b7c5d3420e00648f456744d121ffbd929862924d/src/engine/paris.md#engine_newpayloadv1
    /// [`engine_newPayloadV2`]: https://github.com/ethereum/execution-apis/blob/b7c5d3420e00648f456744d121ffbd929862924d/src/engine/shanghai.md#engine_newpayloadv2
    /// [`engine_newPayloadV3`]: https://github.com/ethereum/execution-apis/blob/a0d03086564ab1838b462befbc083f873dcf0c0f/src/engine/cancun.md#engine_newpayloadv3
    /// [`engine_newPayloadV4`]: https://github.com/ethereum/execution-apis/blob/4140e528360fea53c34a766d86a000c6c039100e/src/engine/prague.md#engine_newpayloadv4
    /// [`engine_newPayloadV5`]: https://github.com/ethereum/execution-apis/blob/4db2ff91a1811f40aa7c23547eef9d2bc789d27e/src/engine/amsterdam.md#engine_newpayloadv5
    #[expect(clippy::too_many_lines)]
    pub async fn new_payload<P: Preset>(
        &self,
        payload: ExecutionPayload<P>,
        params: Option<ExecutionPayloadParams<P>>,
    ) -> Result<PayloadStatusV1> {
        match (payload, params) {
            (ExecutionPayload::Bellatrix(payload), None) => {
                let payload_v1 = ExecutionPayloadV1::from(payload);
                let params = vec![serde_json::to_value(payload_v1)?];
                self.execute(
                    ENGINE_NEW_PAYLOAD_V1,
                    params,
                    Some(ENGINE_NEW_PAYLOAD_TIMEOUT),
                    None,
                )
                .await
                .map(WithClientVersions::result)
            }
            (ExecutionPayload::Capella(payload), None) => {
                let payload_v2 = ExecutionPayloadV2::from(payload);
                let params = vec![serde_json::to_value(payload_v2)?];
                self.execute(
                    ENGINE_NEW_PAYLOAD_V2,
                    params,
                    Some(ENGINE_NEW_PAYLOAD_TIMEOUT),
                    None,
                )
                .await
                .map(WithClientVersions::result)
            }
            (
                ExecutionPayload::Deneb(payload),
                Some(ExecutionPayloadParams::Deneb {
                    versioned_hashes,
                    parent_beacon_block_root,
                }),
            ) => {
                let payload_v3 = ExecutionPayloadV3::from(payload);
                let params = vec![
                    serde_json::to_value(payload_v3)?,
                    serde_json::to_value(versioned_hashes)?,
                    serde_json::to_value(parent_beacon_block_root)?,
                ];
                self.execute(
                    ENGINE_NEW_PAYLOAD_V3,
                    params,
                    Some(ENGINE_NEW_PAYLOAD_TIMEOUT),
                    None,
                )
                .await
                .map(WithClientVersions::result)
            }
            (
                ExecutionPayload::Deneb(payload),
                Some(ExecutionPayloadParams::Electra {
                    versioned_hashes,
                    parent_beacon_block_root,
                    execution_requests,
                }),
            ) => {
                let payload_v3 = ExecutionPayloadV3::from(payload);
                let raw_execution_requests = RawExecutionRequests::from(execution_requests);

                let params = vec![
                    serde_json::to_value(payload_v3)?,
                    serde_json::to_value(versioned_hashes)?,
                    serde_json::to_value(parent_beacon_block_root)?,
                    serde_json::to_value(raw_execution_requests)?,
                ];

                self.execute(
                    ENGINE_NEW_PAYLOAD_V4,
                    params,
                    Some(ENGINE_NEW_PAYLOAD_TIMEOUT),
                    None,
                )
                .await
                .map(WithClientVersions::result)
            }
            (
                ExecutionPayload::Gloas(payload),
                Some(ExecutionPayloadParams::Gloas {
                    versioned_hashes,
                    parent_beacon_block_root,
                    execution_requests,
                }),
            ) => {
                let payload_v4 = ExecutionPayloadV4::try_from(payload)?;
                let raw_execution_requests = RawExecutionRequests::try_from(execution_requests)?;

                let params = vec![
                    serde_json::to_value(payload_v4)?,
                    serde_json::to_value(versioned_hashes)?,
                    serde_json::to_value(parent_beacon_block_root)?,
                    serde_json::to_value(raw_execution_requests)?,
                ];

                self.execute(
                    ENGINE_NEW_PAYLOAD_V5,
                    params,
                    Some(ENGINE_NEW_PAYLOAD_TIMEOUT),
                    None,
                )
                .await
                .map(WithClientVersions::result)
            }
            _ => bail!(Error::InvalidParameters),
        }
    }

    /// Calls [`engine_forkchoiceUpdatedV1`] or [`engine_forkchoiceUpdatedV2`] or [`engine_forkchoiceUpdatedV3`] or [`engine_forkchoiceUpdatedV4`] depending on `payload_attributes`.
    ///
    /// Later versions of `engine_forkchoiceUpdated` accept parameters of all prior versions,
    /// but using the earlier versions allows the application to work with old execution clients.
    ///
    /// [`engine_forkchoiceUpdatedV1`]: https://github.com/ethereum/execution-apis/blob/b7c5d3420e00648f456744d121ffbd929862924d/src/engine/paris.md#engine_forkchoiceupdatedv1
    /// [`engine_forkchoiceUpdatedV2`]: https://github.com/ethereum/execution-apis/blob/b7c5d3420e00648f456744d121ffbd929862924d/src/engine/shanghai.md#engine_forkchoiceupdatedv2
    /// [`engine_forkchoiceUpdatedV3`]: https://github.com/ethereum/execution-apis/blob/a0d03086564ab1838b462befbc083f873dcf0c0f/src/engine/cancun.md#engine_forkchoiceupdatedv3
    /// [`engine_forkchoiceUpdatedV4`]: https://github.com/ethereum/execution-apis/blob/ffe6c839567f931ece3276d8242963744f09bf67/src/engine/amsterdam.md#engine_forkchoiceupdatedv4
    pub async fn forkchoice_updated<P: Preset>(
        &self,
        head_block_hash: ExecutionBlockHash,
        safe_block_hash: ExecutionBlockHash,
        finalized_block_hash: ExecutionBlockHash,
        payload_attributes: Either<Phase, PayloadAttributes<P>>,
    ) -> Result<ForkChoiceUpdatedResponse> {
        let fork_choice_state = ForkChoiceStateV1 {
            head_block_hash,
            safe_block_hash,
            finalized_block_hash,
        };

        let phase = payload_attributes
            .as_ref()
            .either(CopyExt::copy, PayloadAttributes::phase);

        let payload_attributes = payload_attributes.right();

        let params = vec![
            serde_json::to_value(fork_choice_state)?,
            serde_json::to_value(payload_attributes)?,
        ];

        let RawForkChoiceUpdatedResponse {
            payload_id,
            payload_status,
        } = match phase {
            Phase::Bellatrix => {
                self.execute(
                    ENGINE_FORKCHOICE_UPDATED_V1,
                    params,
                    Some(ENGINE_FORKCHOICE_UPDATED_TIMEOUT),
                    None,
                )
                .await?
                .result
            }
            Phase::Capella => {
                self.execute(
                    ENGINE_FORKCHOICE_UPDATED_V2,
                    params,
                    Some(ENGINE_FORKCHOICE_UPDATED_TIMEOUT),
                    None,
                )
                .await?
                .result
            }
            Phase::Deneb | Phase::Electra | Phase::Fulu => {
                self.execute(
                    ENGINE_FORKCHOICE_UPDATED_V3,
                    params,
                    Some(ENGINE_FORKCHOICE_UPDATED_TIMEOUT),
                    None,
                )
                .await?
                .result
            }
            Phase::Gloas => {
                self.execute(
                    ENGINE_FORKCHOICE_UPDATED_V4,
                    params,
                    Some(ENGINE_FORKCHOICE_UPDATED_TIMEOUT),
                    None,
                )
                .await?
                .result
            }
            _ => {
                // This match arm will silently match any new phases.
                // Cause a compilation error if a new phase is added.
                const_assert_eq!(Phase::CARDINALITY, 8);

                bail!(Error::PhasePreBellatrix)
            }
        };

        let payload_id = match phase {
            Phase::Bellatrix => payload_id.map(PayloadId::Bellatrix),
            Phase::Capella => payload_id.map(PayloadId::Capella),
            Phase::Deneb => payload_id.map(PayloadId::Deneb),
            Phase::Electra => payload_id.map(PayloadId::Electra),
            Phase::Fulu => payload_id.map(PayloadId::Fulu),
            Phase::Gloas => payload_id.map(PayloadId::Gloas),
            _ => {
                // This match arm will silently match any new phases.
                // Cause a compilation error if a new phase is added.
                const_assert_eq!(Phase::CARDINALITY, 8);

                bail!(Error::PhasePreBellatrix)
            }
        };

        Ok(ForkChoiceUpdatedResponse {
            payload_status,
            payload_id,
        })
    }

    /// Calls [`engine_getPayloadV1`] or [`engine_getPayloadV2`] or [`engine_getPayloadV3`] or [`engine_getPayloadV4`] or [`engine_getPayloadV5`] or [`engine_getPayloadV6`] depending on `payload_id`.
    ///
    /// Newer versions of the method may be used to request payloads from all prior versions,
    /// but using the old methods allows the application to work with old execution clients.
    ///
    /// [`engine_getPayloadV1`]: https://github.com/ethereum/execution-apis/blob/b7c5d3420e00648f456744d121ffbd929862924d/src/engine/paris.md#engine_getpayloadv1
    /// [`engine_getPayloadV2`]: https://github.com/ethereum/execution-apis/blob/b7c5d3420e00648f456744d121ffbd929862924d/src/engine/shanghai.md#engine_getpayloadv2
    /// [`engine_getPayloadV3`]: https://github.com/ethereum/execution-apis/blob/a0d03086564ab1838b462befbc083f873dcf0c0f/src/engine/cancun.md#engine_getpayloadv3
    /// [`engine_getPayloadV4`]: https://github.com/ethereum/execution-apis/blob/4140e528360fea53c34a766d86a000c6c039100e/src/engine/prague.md#engine_getpayloadv4
    /// [`engine_getPayloadV5`]: https://github.com/ethereum/execution-apis/blob/5d634063ccfd897a6974ea589c00e2c1d889abc9/src/engine/osaka.md#engine_getpayloadv5
    /// [`engine_getPayloadV6`]: https://github.com/ethereum/execution-apis/blob/4db2ff91a1811f40aa7c23547eef9d2bc789d27e/src/engine/amsterdam.md#engine_getpayloadv6
    pub async fn get_payload<P: Preset>(
        &self,
        payload_id: PayloadId,
    ) -> Result<WithClientVersions<WithBlobsAndMev<ExecutionPayload<P>, P>>> {
        match payload_id {
            PayloadId::Bellatrix(payload_id) => {
                let params = vec![serde_json::to_value(payload_id)?];

                self.execute::<EngineGetPayloadV1Response<P>>(
                    ENGINE_GET_PAYLOAD_V1,
                    params,
                    Some(ENGINE_GET_PAYLOAD_TIMEOUT),
                    None,
                )
                .await
                .map(|with_client_info| with_client_info.map(Into::into))
            }
            PayloadId::Capella(payload_id) => {
                let params = vec![serde_json::to_value(payload_id)?];

                self.execute::<EngineGetPayloadV2Response<P>>(
                    ENGINE_GET_PAYLOAD_V2,
                    params,
                    Some(ENGINE_GET_PAYLOAD_TIMEOUT),
                    None,
                )
                .await
                .map(|with_client_info| with_client_info.map(Into::into))
            }
            PayloadId::Deneb(payload_id) => {
                let params = vec![serde_json::to_value(payload_id)?];

                self.execute::<EngineGetPayloadV3Response<P>>(
                    ENGINE_GET_PAYLOAD_V3,
                    params,
                    Some(ENGINE_GET_PAYLOAD_TIMEOUT),
                    None,
                )
                .await
                .map(|with_client_info| with_client_info.map(Into::into))
            }
            PayloadId::Electra(payload_id) => {
                let params = vec![serde_json::to_value(payload_id)?];

                self.execute::<EngineGetPayloadV4Response<P>>(
                    ENGINE_GET_PAYLOAD_V4,
                    params,
                    Some(ENGINE_GET_PAYLOAD_TIMEOUT),
                    None,
                )
                .await
                .map(|with_client_info| with_client_info.map(Into::into))
            }
            PayloadId::Fulu(payload_id) => {
                let params = vec![serde_json::to_value(payload_id)?];

                self.execute::<EngineGetPayloadV5Response<P>>(
                    ENGINE_GET_PAYLOAD_V5,
                    params,
                    Some(ENGINE_GET_PAYLOAD_TIMEOUT),
                    None,
                )
                .await
                .map(|with_client_info| with_client_info.map(Into::into))
            }
            PayloadId::Gloas(payload_id) => {
                let params = vec![serde_json::to_value(payload_id)?];

                self.execute::<EngineGetPayloadV6Response<P>>(
                    ENGINE_GET_PAYLOAD_V6,
                    params,
                    Some(ENGINE_GET_PAYLOAD_TIMEOUT),
                    None,
                )
                .await
                .map(|with_client_info| with_client_info.map(Into::into))
            }
        }
    }

    async fn execute<T: DeserializeOwned + Send>(
        &self,
        method: &str,
        params: Vec<Value>,
        timeout: Option<Duration>,
        capability: Option<&str>,
    ) -> Result<WithClientVersions<T>> {
        let _timer = self.metrics.as_ref().map(|metrics| {
            prometheus_metrics::start_timer_vec(&metrics.eth1_api_request_times, method)
        });

        // Payload body reconstruction runs on behalf of peers, deliberately skips the capability
        // filter and probes for the batch size limit. Whether these two methods succeed says
        // nothing about the ability of the endpoint to serve the methods the node itself depends
        // on, and peers can make them fail at will, so their outcome must not move the endpoint
        // online status in either direction: a failure must not mask a working endpoint, and a
        // success must not revive an endpoint that `engine_newPayload` found broken.
        let tolerate_errors = matches!(
            method,
            ENGINE_GET_PAYLOAD_BODIES_BY_HASH_V1 | ENGINE_GET_PAYLOAD_BODIES_BY_RANGE_V1,
        );

        self.request_with_fallback(
            |(api, headers)| {
                Ok(CallFuture::new(api.transport().execute_with_headers(
                    method,
                    params.clone(),
                    headers,
                    timeout,
                )))
            },
            capability,
            tolerate_errors,
        )
        .await
    }

    #[must_use]
    pub fn el_offline(&self) -> bool {
        self.endpoints.el_offline()
    }

    /// Calls `request_from_api` on each endpoint in turn until one of them succeeds.
    ///
    /// `tolerate_errors` keeps requests made on behalf of peers out of the endpoint health
    /// tracking entirely: neither their failures nor their successes change the online status or
    /// the error metrics, and their failures are reported once per outage instead of per request.
    async fn request_with_fallback<R, O, F>(
        &self,
        request_from_api: R,
        capability: Option<&str>,
        tolerate_errors: bool,
    ) -> Result<WithClientVersions<O>>
    where
        R: Fn((Eth<Http>, Option<HeaderMap>)) -> Result<CallFuture<O, F>> + Sync + Send,
        O: DeserializeOwned + Send,
        F: Future<Output = Result<Value, Web3Error>> + Send,
    {
        let mut endpoints_for_request = self.endpoints.endpoints_for_request(capability).peekable();
        let mut last_error = None;

        while let Some(endpoint) = endpoints_for_request.next() {
            let api = self.build_api_for_request(endpoint);
            let query = request_from_api((api, self.auth.headers()?))?.await;

            match query {
                Ok(result) => {
                    if tolerate_errors {
                        self.payload_bodies_failure_reported
                            .store(false, Ordering::Relaxed);
                    } else {
                        self.on_ok_response(endpoint);
                    }

                    return Ok(WithClientVersions {
                        client_versions: Some(endpoint.get_client_versions()),
                        result,
                    });
                }
                Err(error) => {
                    if tolerate_errors {
                        debug_with_peers!(
                            "Eth1 RPC endpoint {} could not serve a request made on behalf of a \
                             peer: {error}",
                            endpoint.url(),
                        );
                    } else {
                        self.on_error_response(endpoint);

                        match endpoints_for_request.peek() {
                            Some(next_endpoint) => warn_with_peers!(
                                "Eth1 RPC endpoint {} returned an error: {error}; switching to {}",
                                endpoint.url(),
                                next_endpoint.url(),
                            ),
                            None => warn_with_peers!(
                                "last available Eth1 RPC endpoint {} returned an error: {error}",
                                endpoint.url(),
                            ),
                        }
                    }

                    last_error = Some(error);
                }
            }
        }

        if tolerate_errors {
            // Peers request historical blocks continuously, so warning on every failure would
            // flood the log. Warning on the transition into failure still tells the operator that
            // the node stopped serving blocks it holds, which nothing else in the system reports.
            if !self
                .payload_bodies_failure_reported
                .swap(true, Ordering::Relaxed)
            {
                warn_with_peers!(
                    "no Eth1 RPC endpoint could serve execution payload bodies; \
                     blocks stored without their execution payloads cannot be served \
                     to peers or over the HTTP API until one can. \
                     Pass --store-payloads to keep payloads in the beacon database instead",
                );
            }
        } else if let Some(metrics) = self.metrics.as_ref() {
            metrics.eth1_api_reset_count.inc();
        }

        // Checking this in `Eth1Api::new` would be unnecessarily strict.
        // Syncing a predefined network without proposing blocks does not require an Eth1 RPC
        // (except during the Merge transition).
        ensure!(!self.endpoints.is_empty(), Error::NoEndpointsProvided);

        match last_error {
            Some(error) => Err(AnyhowError::new(error).context(Error::EndpointsExhausted)),
            None => bail!(Error::EndpointsExhausted),
        }
    }

    pub(crate) fn build_api_for_request(&self, endpoint: &Endpoint) -> Eth<Http> {
        let http = Http::with_client(self.client.clone(), endpoint.url().clone().into_url());
        Web3::new(http).eth()
    }

    pub(crate) fn on_ok_response(&self, endpoint: &Endpoint) {
        endpoint.set_online_status(true);

        if let Some(metrics_tx) = self.eth1_api_to_metrics_tx.as_ref() {
            Eth1ApiToMetrics::Eth1Connection(Eth1ConnectionData {
                sync_eth1_connected: true,
                sync_eth1_fallback_connected: endpoint.is_fallback,
            })
            .send(metrics_tx);
        }
    }

    pub(crate) fn on_error_response(&self, endpoint: &Endpoint) {
        endpoint.set_online_status(false);

        if let Some(metrics) = self.metrics.as_ref() {
            metrics.eth1_api_errors_count.inc();
        }

        if let Some(metrics_tx) = self.eth1_api_to_metrics_tx.as_ref() {
            Eth1ApiToMetrics::Eth1Connection(Eth1ConnectionData::default()).send(metrics_tx);
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawForkChoiceUpdatedResponse {
    payload_status: PayloadStatusV1,
    payload_id: Option<H64>,
}

#[derive(Debug, Error)]
#[cfg_attr(test, derive(PartialEq, Eq))]
enum Error {
    #[error("all Eth1 RPC endpoints exhausted")]
    EndpointsExhausted,
    #[error("attempted to call Eth1 RPC endpoint with misconfigured parameters")]
    InvalidParameters,
    #[error("attempted to call Eth1 RPC endpoint but none were provided")]
    NoEndpointsProvided,
    #[error("execution client returned {returned} payload bodies for a request of {requested}")]
    PayloadBodyCountMismatch { requested: usize, returned: usize },
    #[error("pre-Bellatrix phase passed to Eth1Api::forkchoice_updated")]
    PhasePreBellatrix,
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use execution_engine::PayloadValidationStatus;
    use hex_literal::hex;
    use httpmock::{HttpMockRequest, Method, MockServer};
    use serde_json::json;
    use ssz::ContiguousList;
    use types::{
        bellatrix::containers::ExecutionPayload as BellatrixExecutionPayload,
        electra::containers::{DepositRequest, ExecutionRequests},
        phase0::primitives::H256,
        preset::Mainnet,
    };

    use super::*;

    /// [`Method not found`](https://www.jsonrpc.org/specification#error_object)
    const METHOD_NOT_FOUND_ERROR_CODE: i64 = -32601;

    #[tokio::test]
    async fn test_eth1_endpoints_error_with_no_endpoints() -> Result<()> {
        let config = Arc::new(Config::mainnet());
        let auth = Arc::default();

        let eth1_api = Arc::new(Eth1Api::new(
            config,
            Client::new(),
            auth,
            vec![],
            None,
            None,
        ));

        assert!(eth1_api.el_offline());

        assert_eq!(
            eth1_api
                .current_head_number()
                .await
                .expect_err("Eth1Api with no endpoints should return an error")
                .downcast::<Error>()?,
            Error::NoEndpointsProvided,
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_eth1_endpoints_error_with_single_endpoint() -> Result<()> {
        let server = MockServer::start();

        server.mock(|when, then| {
            when.method(Method::POST).path("/");
            then.status(500).body("{}");
        });

        let config = Arc::new(Config::mainnet());
        let auth = Arc::default();
        let server_url = server.url("/").parse()?;

        let eth1_api = Arc::new(Eth1Api::new(
            config,
            Client::new(),
            auth,
            vec![server_url],
            None,
            None,
        ));

        assert!(!eth1_api.el_offline());
        assert_eq!(
            eth1_api
                .current_head_number()
                .await
                .expect_err("500 response should be a an error")
                .downcast::<Error>()?,
            Error::EndpointsExhausted,
        );

        // Despite the endpoint returning an error, it remains the only available option
        assert!(eth1_api.el_offline());
        assert_eq!(
            eth1_api
                .current_head_number()
                .await
                .expect_err("500 response should be a an error")
                .downcast::<Error>()?,
            Error::EndpointsExhausted,
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_eth1_endpoints_error_with_multiple_endpoints() -> Result<()> {
        let server = MockServer::start();

        server.mock(|when, then| {
            when.method(Method::POST).path("/");
            then.status(500).body("{}");
        });

        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": "0x1d243",
        });

        server.mock(|when, then| {
            when.method(Method::POST).path("/next");
            then.status(200).body(body.to_string());
        });

        let config = Arc::new(Config::mainnet());
        let auth = Arc::default();
        let server_url = server.url("/").parse()?;
        let next_server_url = server.url("/next").parse()?;

        let eth1_api = Arc::new(Eth1Api::new(
            config,
            Client::new(),
            auth,
            vec![server_url, next_server_url],
            None,
            None,
        ));

        assert!(!eth1_api.el_offline());

        // Expect to use the fallback endpoint when the primary endpoint returns an error
        assert_eq!(
            eth1_api
                .current_head_number()
                .await
                .expect("the fallback endpoint should be working"),
            119_363,
        );

        // Even though the primary endpoint is offline, eth1_api itself is not offline
        assert!(!eth1_api.el_offline());

        Ok(())
    }

    #[tokio::test]
    async fn test_bellatrix_payload_deserialization_with_real_response() -> Result<()> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "parentHash": "0x2c7776c6c6c4a3fa2fbfc4886930e681fe4658e23e988b7ce27d4f355269b4a4",
                "feeRecipient": "0x0000000000000000000000000000000000000000",
                "stateRoot": "0xdeb98cee0497b499dc1a6a2323f990d350e80301fbbb0e778b62b5037fce5bf6",
                "receiptsRoot": "0x06215fe5ec9a1b418434561323471cc1c8cfc6ae121aaf03825596268581e098",
                "logsBloom": "0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
                "prevRandao": "0x9ed233232634a35bbcb77b50cf357defc8f61ad5c74d5bba528e1a260f8d2f7f",
                "blockNumber": "0x7130a",
                "gasLimit": "0xf78798",
                "gasUsed": "0x14820",
                "timestamp": "0x621cc4f8",
                "extraData": "0xd883010a11846765746888676f312e31372e38856c696e7578",
                "baseFeePerGas": "0x7",
                "blockHash": "0xedd9cf26b9a0455a67e9abefe926796356ca6564d02463e229097c61ced696db",
                "transactions": [
                    "0xf86e078459682f0782520894419f2d6c3f5fe8bf43f91923ba21e996032897298894a1739b5e1d49c8808328d2f0a069dffffc6f9b20157bd17872d326de8ed088de3e24f2801dd9375ddbecd013f0a041aab6f5dff83fdd2595cc55725b28128b8902f12f3db598dce9f9183f989300",
                    "0x02f87883146966830516988459682f008459682f078252089432960b83199ae0f78756dbcf016a8e88e4dd7a748894a19041886f000080c001a0f916421115b1dc667b959fe32fa01cc9ba07942078b9e28435fd0a55c1cbf2dba076da1b6e79fa9a3b6b77e1601546fa194652a3f9a73919c470254833dfae68f8",
                    "0x02f87883146966830516998459682f008459682f0782520894b467d5ec9f6db8b1c156d40e65ebf88b2596ab198894a19041886f000080c001a0cc9ddcece6913c48e3aaaab25fb4f98da8540f1ffac58b010c9d3d0c60e01edba073cbf451658aa60dac89b62a463a2a95cdba3c73e5d258f80eedd6d465ab0772",
                    "0x02f878831469668305169a8459682f008459682f078252089477831a3a5552ad92848d7134a1e467c1089fb04a8894a19041886f000080c001a0f3687841790c73693a44710dfc83d02f7044ea821cca5a66b92b283c2c346d62a013695d07f9e62132a8c3c423dc4b74b914b5fa758f88fcf2ed10242aaa68ca6a",
                ],
            },
        });

        let server = MockServer::start();

        server.mock(|when, then| {
            when.method(Method::POST).path("/");
            then.status(200).body(body.to_string());
        });

        // The block seems to be from the Kintsugi testnet. There's no block explorer still serving
        // Kintsugi blocks to confirm it, but the block number and timestamp suggest that execution
        // layer genesis happened around 2021-12-15, just before the `MIN_GENESIS_TIME` of Kintsugi.
        let config = Arc::new(Config::mainnet());
        let auth = Arc::default();
        let server_url = server.url("/").parse()?;

        let eth1_api = Arc::new(Eth1Api::new(
            config,
            Client::new(),
            auth,
            vec![server_url],
            None,
            None,
        ));

        let payload_id = PayloadId::Bellatrix(H64(hex!("a5f7426cdca69a73")));

        eth1_api.get_payload::<Mainnet>(payload_id).await?;

        Ok(())
    }

    #[tokio::test]
    async fn test_capella_payload_deserialization_with_full_response() -> Result<()> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 0,
            "result": {
                "executionPayload": {
                    "parentHash": "0x98eff2712c5546167a22d9d3ab340005d8f736d49e8867ab2e67400526dc5d2c",
                    "feeRecipient": "0xe7cf7c3ba875dd3884ed6a9082d342cb4fbb1f1b",
                    "stateRoot": "0x54874eaadc381f61c2999a93c59c36e564a42062d64955e057991534fc166504",
                    "receiptsRoot": "0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421",
                    "logsBloom": "0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
                    "prevRandao": "0x883fbdbbc4a4c75747422bc271c43bf6370f570c43cccd81f80cae71f54ad3da",
                    "blockNumber": "0x21b0",
                    "gasLimit": "0x1c9c380",
                    "gasUsed": "0x0",
                    "timestamp": "0x63d2af38",
                    "extraData": "0xd883010b00846765746888676f312e31392e35856c696e7578",
                    "baseFeePerGas": "0x7",
                    "blockHash": "0x1587569314611d9f06aac37c64c87b180313056d1a968e6b8290ce64c519859f",
                    "transactions": [
                        "0xf86e078459682f0782520894419f2d6c3f5fe8bf43f91923ba21e996032897298894a1739b5e1d49c8808328d2f0a069dffffc6f9b20157bd17872d326de8ed088de3e24f2801dd9375ddbecd013f0a041aab6f5dff83fdd2595cc55725b28128b8902f12f3db598dce9f9183f989300",
                        "0x02f87883146966830516988459682f008459682f078252089432960b83199ae0f78756dbcf016a8e88e4dd7a748894a19041886f000080c001a0f916421115b1dc667b959fe32fa01cc9ba07942078b9e28435fd0a55c1cbf2dba076da1b6e79fa9a3b6b77e1601546fa194652a3f9a73919c470254833dfae68f8",
                        "0x02f87883146966830516998459682f008459682f0782520894b467d5ec9f6db8b1c156d40e65ebf88b2596ab198894a19041886f000080c001a0cc9ddcece6913c48e3aaaab25fb4f98da8540f1ffac58b010c9d3d0c60e01edba073cbf451658aa60dac89b62a463a2a95cdba3c73e5d258f80eedd6d465ab0772",
                        "0x02f878831469668305169a8459682f008459682f078252089477831a3a5552ad92848d7134a1e467c1089fb04a8894a19041886f000080c001a0f3687841790c73693a44710dfc83d02f7044ea821cca5a66b92b283c2c346d62a013695d07f9e62132a8c3c423dc4b74b914b5fa758f88fcf2ed10242aaa68ca6a",
                    ],
                    "withdrawals": [
                        {
                            "index": "0x18561",
                            "validatorIndex": "0x7c2e8",
                            "address": "0xf97e180c050e5ab072211ad2c213eb5aee4df134",
                            "amount": "0x18111",
                        },
                        {
                            "index": "0x18562",
                            "validatorIndex": "0x7c2e9",
                            "address": "0xf97e180c050e5ab072211ad2c213eb5aee4df134",
                            "amount": "0x583a6",
                        },
                    ],
                },
                "blockValue": "0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
            },
        });

        let server = MockServer::start();

        server.mock(|when, then| {
            when.method(Method::POST).path("/");
            then.status(200).body(body.to_string());
        });

        let config = Arc::new(Config::withdrawal_devnet_4());
        let auth = Arc::default();
        let server_url = server.url("/").parse()?;

        let eth1_api = Arc::new(Eth1Api::new(
            config,
            Client::new(),
            auth,
            vec![server_url],
            None,
            None,
        ));

        let payload_id = PayloadId::Capella(H64(hex!("a5f7426cdca69a73")));
        let payload = eth1_api.get_payload::<Mainnet>(payload_id).await?.result;

        assert_eq!(payload.value.phase(), Phase::Capella);

        Ok(())
    }

    #[tokio::test]
    async fn test_electra_payload_deserialization_with_default_execution_requests() -> Result<()> {
        let body = json!({
          "jsonrpc": "2.0",
          "id": 0,
          "result": {
            "executionPayload": {
              "parentHash": "0x128133536f44733af5e59ba865744690498529592c1e85655348ec6bb559c658",
              "feeRecipient": "0x8943545177806ed17b9f23f0a21ee5948ecaa776",
              "stateRoot": "0xfb458127dfb40b16693e70886d0f503160be2ad409ab885fb4051d96b07bdef1",
              "receiptsRoot": "0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421",
              "logsBloom": "0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
              "prevRandao": "0x4c2db6d476f102aa7b68808f9262c70760e6cd5f23213c039cbe7309437a8d9d",
              "blockNumber": "0x29",
              "gasLimit": "0x1c9c380",
              "gasUsed": "0x0",
              "timestamp": "0x671214b3",
              "extraData": "0xd883010e0c846765746888676f312e32332e32856c696e7578",
              "baseFeePerGas": "0x403226",
              "blockHash": "0x49a38631ab242befe4d9fbb1a49c7059c21363a534542f8bcf419a82b92a229b",
              "transactions": [],
              "withdrawals": [
                {
                  "index": "0xbb",
                  "validatorIndex": "0xd1",
                  "address": "0x65d08a056c17ae13370565b04cf77d2afa1cb9fa",
                  "amount": "0x51f0"
                },
                {
                  "index": "0xbc",
                  "validatorIndex": "0xd2",
                  "address": "0x65d08a056c17ae13370565b04cf77d2afa1cb9fa",
                  "amount": "0x51f0"
                }
              ],
              "blobGasUsed": "0x0",
              "excessBlobGas": "0x0"
            },
            "blockValue": "0x0",
            "blobsBundle": {
              "commitments": [],
              "proofs": [],
              "blobs": []
            },
            "executionRequests": [],
            "shouldOverrideBuilder": false
          }
        });

        let server = MockServer::start();

        server.mock(|when, then| {
            when.method(Method::POST).path("/");
            then.status(200).body(body.to_string());
        });

        let config = Arc::new(Config::mainnet());
        let auth = Arc::default();
        let server_url = server.url("/").parse()?;

        let eth1_api = Arc::new(Eth1Api::new(
            config,
            Client::new(),
            auth,
            vec![server_url],
            None,
            None,
        ));

        let payload_id = PayloadId::Electra(H64(hex!("a5f7426cdca69a73")));
        let payload = eth1_api.get_payload::<Mainnet>(payload_id).await?.result;

        assert_eq!(payload.value.phase(), Phase::Deneb);
        assert_eq!(
            payload.execution_requests,
            Some(ExecutionRequests::default().into())
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_electra_payload_deserialization_with_non_empty_execution_requests() -> Result<()>
    {
        let body = json!({
          "jsonrpc": "2.0",
          "id": 0,
          "result": {
            "executionPayload": {
              "parentHash": "0x128133536f44733af5e59ba865744690498529592c1e85655348ec6bb559c658",
              "feeRecipient": "0x8943545177806ed17b9f23f0a21ee5948ecaa776",
              "stateRoot": "0xfb458127dfb40b16693e70886d0f503160be2ad409ab885fb4051d96b07bdef1",
              "receiptsRoot": "0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421",
              "logsBloom": "0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
              "prevRandao": "0x4c2db6d476f102aa7b68808f9262c70760e6cd5f23213c039cbe7309437a8d9d",
              "blockNumber": "0x29",
              "gasLimit": "0x1c9c380",
              "gasUsed": "0x0",
              "timestamp": "0x671214b3",
              "extraData": "0xd883010e0c846765746888676f312e32332e32856c696e7578",
              "baseFeePerGas": "0x403226",
              "blockHash": "0x49a38631ab242befe4d9fbb1a49c7059c21363a534542f8bcf419a82b92a229b",
              "transactions": [],
              "withdrawals": [
                {
                  "index": "0xbb",
                  "validatorIndex": "0xd1",
                  "address": "0x65d08a056c17ae13370565b04cf77d2afa1cb9fa",
                  "amount": "0x51f0"
                },
                {
                  "index": "0xbc",
                  "validatorIndex": "0xd2",
                  "address": "0x65d08a056c17ae13370565b04cf77d2afa1cb9fa",
                  "amount": "0x51f0"
                }
              ],
              "blobGasUsed": "0x0",
              "excessBlobGas": "0x0"
            },
            "blockValue": "0x0",
            "blobsBundle": {
              "commitments": [],
              "proofs": [],
              "blobs": []
            },
            "executionRequests": [
              "0x0092f9fe7570a6650d030bb2227d699c744303d08a887cd2e1592e30906cd8cedf9646c1a1afd902235bb36620180eb68802000000000000000000000065d08a056c17ae13370565b04cf77d2afa1cb9fa0010a5d4e8000000a13741d65b47825c147201cfce3360438d4011fe81b455e86226c95a2669bfde14712ba36d1c2f44371a98bf28ff38370ce7d28c65872bf65ff88d6014468676029e298903c89c51c27ab5f07e178b8b14d3ca191e2ce3b24703629e3994e05b000000000000000090a58546229c585cef35f3afab904411530303d95c371e246a2e9a1ef6beb5db7a98c2fd79a388709a30ec782576a5d602000000000000000000000065d08a056c17ae13370565b04cf77d2afa1cb9fa0010a5d4e8000000b23e205d2fcfc3e9d3ae58c0f78b55b19f97f59eaf43d85113a1960ee2c38f6b4ef705302e46e0593fc41ba5632b047a14d76dc82bb2619d7c73e0d89da2eda2ea11fff9036c2d08f9d457c07f23b1411ecd13ff0e9c00eeb85d851bae2494e00100000000000000",
            ],
            "shouldOverrideBuilder": false
          }
        });

        let server = MockServer::start();

        server.mock(|when, then| {
            when.method(Method::POST).path("/");
            then.status(200).body(body.to_string());
        });

        let config = Arc::new(Config::mainnet());
        let auth = Arc::default();
        let server_url = server.url("/").parse()?;

        let eth1_api = Arc::new(Eth1Api::new(
            config,
            Client::new(),
            auth,
            vec![server_url],
            None,
            None,
        ));

        let payload_id = PayloadId::Electra(H64(hex!("a5f7426cdca69a73")));
        let payload = eth1_api.get_payload::<Mainnet>(payload_id).await?.result;

        assert_eq!(payload.value.phase(), Phase::Deneb);
        assert_eq!(
            payload.execution_requests,
            Some(ExecutionRequests {
                deposits: ContiguousList::try_from(vec![
                    DepositRequest {
                        pubkey: hex!("92f9fe7570a6650d030bb2227d699c744303d08a887cd2e1592e30906cd8cedf9646c1a1afd902235bb36620180eb688").into(),
                        withdrawal_credentials: hex!("02000000000000000000000065d08a056c17ae13370565b04cf77d2afa1cb9fa").into(),
                        amount: 1_000_000_000_000,
                        signature: hex!("a13741d65b47825c147201cfce3360438d4011fe81b455e86226c95a2669bfde14712ba36d1c2f44371a98bf28ff38370ce7d28c65872bf65ff88d6014468676029e298903c89c51c27ab5f07e178b8b14d3ca191e2ce3b24703629e3994e05b").into(),
                        index: 0,
                    },
                    DepositRequest {
                        pubkey: hex!("90a58546229c585cef35f3afab904411530303d95c371e246a2e9a1ef6beb5db7a98c2fd79a388709a30ec782576a5d6").into(),
                        withdrawal_credentials: hex!("02000000000000000000000065d08a056c17ae13370565b04cf77d2afa1cb9fa").into(),
                        amount: 1_000_000_000_000,
                        signature: hex!("b23e205d2fcfc3e9d3ae58c0f78b55b19f97f59eaf43d85113a1960ee2c38f6b4ef705302e46e0593fc41ba5632b047a14d76dc82bb2619d7c73e0d89da2eda2ea11fff9036c2d08f9d457c07f23b1411ecd13ff0e9c00eeb85d851bae2494e0").into(),
                        index: 1,
                    }
                ])?,
                ..Default::default()
            }.into())
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_valid_payload_status_deserialization() -> Result<()> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 0,
            "result": {
                "status": "VALID",
                "latestValidHash": "0x0da76c72389ffe8b8bef1266213dd0dc4bf7030293913bfd69869cb349b13d35",
                "validationError": null,
            },
        });

        let server = MockServer::start();

        server.mock(|when, then| {
            when.method(Method::POST).path("/");
            then.status(200).body(body.to_string());
        });

        let config = Arc::new(Config::mainnet());
        let auth = Arc::default();
        let server_url = server.url("/").parse()?;

        let eth1_api = Arc::new(Eth1Api::new(
            config,
            Client::new(),
            auth,
            vec![server_url],
            None,
            None,
        ));

        let actual_status = eth1_api
            .new_payload::<Mainnet>(default_payload(), None)
            .await?;

        let expected_status = PayloadStatusV1 {
            status: PayloadValidationStatus::Valid,
            latest_valid_hash: Some(H256(hex!(
                "0da76c72389ffe8b8bef1266213dd0dc4bf7030293913bfd69869cb349b13d35"
            ))),
            validation_error: None,
        };

        assert_eq!(actual_status, expected_status);

        Ok(())
    }

    // `geth` responds to invalid payloads with objects containing `method` and `params`.
    // We had to fork `jsonrpc` because it does not allow nonstandard members.
    #[tokio::test]
    async fn test_invalid_payload_status_deserialization() -> Result<()> {
        let body = json!({
            "jsonrpc": "2.0",
            "method": "",
            "params": null,
            "id": 0,
            "result": {
                "latestValidHash": "0x5669a0cec34c19c288b9db210ea180d11ad3d92975234bdc769610b5fa4d7f80",
                "status": "INVALID",
                "validationError": null,
            },
        });

        let server = MockServer::start();

        server.mock(|when, then| {
            when.method(Method::POST).path("/");
            then.status(200).body(body.to_string());
        });

        let config = Arc::new(Config::mainnet());
        let auth = Arc::default();
        let server_url = server.url("/").parse()?;

        let eth1_api = Arc::new(Eth1Api::new(
            config,
            Client::new(),
            auth,
            vec![server_url],
            None,
            None,
        ));

        let actual_status = eth1_api
            .new_payload::<Mainnet>(default_payload(), None)
            .await?;

        let expected_status = PayloadStatusV1 {
            status: PayloadValidationStatus::Invalid,
            latest_valid_hash: Some(H256(hex!(
                "5669a0cec34c19c288b9db210ea180d11ad3d92975234bdc769610b5fa4d7f80"
            ))),
            validation_error: None,
        };

        assert_eq!(actual_status, expected_status);

        Ok(())
    }

    #[tokio::test]
    async fn test_payload_bodies_by_hash_deserialization() -> Result<()> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": [
                {
                    "transactions": [
                        "0xf86e078459682f0782520894419f2d6c3f5fe8bf43f91923ba21e996032897298894a1739b5e1d49c8808328d2f0a069dffffc6f9b20157bd17872d326de8ed088de3e24f2801dd9375ddbecd013f0a041aab6f5dff83fdd2595cc55725b28128b8902f12f3db598dce9f9183f989300",
                        "0x02f87883146966830516988459682f008459682f078252089432960b83199ae0f78756dbcf016a8e88e4dd7a748894a19041886f000080c001a0f916421115b1dc667b959fe32fa01cc9ba07942078b9e28435fd0a55c1cbf2dba076da1b6e79fa9a3b6b77e1601546fa194652a3f9a73919c470254833dfae68f8",
                    ],
                    "withdrawals": [
                        {
                            "index": "0x18561",
                            "validatorIndex": "0x7c2e8",
                            "address": "0xf97e180c050e5ab072211ad2c213eb5aee4df134",
                            "amount": "0x18111",
                        },
                    ],
                },
                {
                    "transactions": [],
                    "withdrawals": [],
                },
            ],
        });

        let (_server, eth1_api) = eth1_api_serving(&body)?;

        let bodies = eth1_api
            .get_payload_bodies_by_hash::<Mainnet>(&[H256::repeat_byte(1), H256::repeat_byte(2)])
            .await?;

        assert_eq!(bodies.len(), 2);

        let populated = bodies[0].as_ref().expect("body should be present");

        assert_eq!(populated.transactions.len(), 2);
        assert_eq!(
            populated
                .withdrawals
                .as_ref()
                .expect("withdrawals should be present")
                .len(),
            1,
        );

        let empty = bodies[1].as_ref().expect("body should be present");

        assert_eq!(empty.transactions.len(), 0);
        assert_eq!(
            empty
                .withdrawals
                .as_ref()
                .expect("withdrawals should be present")
                .len(),
            0,
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_payload_bodies_by_hash_with_missing_body() -> Result<()> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": [
                null,
                {
                    "transactions": [],
                    "withdrawals": [],
                },
            ],
        });

        let (_server, eth1_api) = eth1_api_serving(&body)?;

        let bodies = eth1_api
            .get_payload_bodies_by_hash::<Mainnet>(&[H256::repeat_byte(1), H256::repeat_byte(2)])
            .await?;

        assert!(bodies[0].is_none());
        assert!(bodies[1].is_some());

        Ok(())
    }

    #[tokio::test]
    async fn test_payload_bodies_by_hash_with_short_response() -> Result<()> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": [
                {
                    "transactions": [],
                    "withdrawals": [],
                },
            ],
        });

        let (_server, eth1_api) = eth1_api_serving(&body)?;

        assert_eq!(
            eth1_api
                .get_payload_bodies_by_hash::<Mainnet>(&[
                    H256::repeat_byte(1),
                    H256::repeat_byte(2),
                ])
                .await
                .expect_err("a response shorter than the request should be an error")
                .downcast::<Error>()?,
            Error::PayloadBodyCountMismatch {
                requested: 2,
                returned: 1,
            },
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_payload_bodies_with_no_endpoints() -> Result<()> {
        let eth1_api = Arc::new(Eth1Api::new(
            Arc::new(Config::mainnet()),
            Client::new(),
            Arc::default(),
            vec![],
            None,
            None,
        ));

        assert_eq!(
            eth1_api
                .get_payload_bodies_by_hash::<Mainnet>(&[H256::repeat_byte(1)])
                .await
                .expect_err("Eth1Api with no endpoints should return an error")
                .downcast::<Error>()?,
            Error::NoEndpointsProvided,
        );

        assert_eq!(
            eth1_api
                .get_payload_bodies_by_range::<Mainnet>(1, 1)
                .await
                .expect_err("Eth1Api with no endpoints should return an error")
                .downcast::<Error>()?,
            Error::NoEndpointsProvided,
        );

        Ok(())
    }

    // Pre-Shanghai bodies have no withdrawals at all, and the execution client trims the response
    // at its latest known block.
    #[tokio::test]
    async fn test_payload_bodies_by_range_without_withdrawals() -> Result<()> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": [
                {
                    "transactions": [],
                    "withdrawals": null,
                },
                {
                    "transactions": [],
                },
            ],
        });

        let (_server, eth1_api) = eth1_api_serving(&body)?;

        let bodies = eth1_api
            .get_payload_bodies_by_range::<Mainnet>(100, 4)
            .await?;

        assert_eq!(bodies.len(), 2);

        for body in &bodies {
            assert!(
                body.as_ref()
                    .expect("body should be present")
                    .withdrawals
                    .is_none()
            );
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_payload_bodies_batch_size_shrinks_on_too_large_request() -> Result<()> {
        let server = MockServer::start();

        let too_large = server.mock(|when, then| {
            when.method(Method::POST)
                .path("/")
                .is_true(|request| requested_hashes(request) > PAYLOAD_BODIES_MIN_BATCH_SIZE);

            then.status(200).body(
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "error": {
                        "code": TOO_LARGE_REQUEST_ERROR_CODE,
                        "message": "Too large request",
                    },
                })
                .to_string(),
            );
        });

        let accepted = server.mock(|when, then| {
            when.method(Method::POST)
                .path("/")
                .is_true(|request| requested_hashes(request) <= PAYLOAD_BODIES_MIN_BATCH_SIZE);

            then.status(200).body(
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "result": vec![
                        json!({ "transactions": [], "withdrawals": [] });
                        PAYLOAD_BODIES_MIN_BATCH_SIZE
                    ],
                })
                .to_string(),
            );
        });

        let eth1_api = Arc::new(Eth1Api::new(
            Arc::new(Config::mainnet()),
            Client::new(),
            Arc::default(),
            vec![server.url("/").parse()?],
            None,
            None,
        ));

        let block_hashes = (0..2 * PAYLOAD_BODIES_MIN_BATCH_SIZE)
            .map(|index| H256::from_low_u64_be(index as u64))
            .collect::<Vec<_>>();

        assert_eq!(
            eth1_api.payload_bodies_batch_size(),
            PAYLOAD_BODIES_MAX_BATCH_SIZE,
        );

        let bodies = eth1_api
            .get_payload_bodies_by_hash::<Mainnet>(&block_hashes)
            .await?;

        assert_eq!(bodies.len(), block_hashes.len());
        assert_eq!(
            eth1_api.payload_bodies_batch_size(),
            PAYLOAD_BODIES_MIN_BATCH_SIZE,
        );

        too_large.assert_calls(1);
        accepted.assert_calls(2);

        // Probing for the batch size limit is not an outage.
        assert!(!eth1_api.el_offline());

        // The reduced size is reused, so the oversized request is not repeated.
        let bodies = eth1_api
            .get_payload_bodies_by_hash::<Mainnet>(&block_hashes)
            .await?;

        assert_eq!(bodies.len(), block_hashes.len());

        too_large.assert_calls(1);
        accepted.assert_calls(4);

        Ok(())
    }

    // A wrong `start`, a wrong `count`, or the wrong encoding would silently return bodies
    // belonging to other blocks, which surfaces much later as a payload mismatch.
    #[tokio::test]
    async fn test_payload_bodies_by_range_sends_hex_encoded_start_and_count() -> Result<()> {
        let server = MockServer::start();

        let mock = server.mock(|when, then| {
            when.method(Method::POST).path("/").is_true(|request| {
                let body = serde_json::from_slice::<Value>(request.body_ref())
                    .expect("request body is JSON");

                body["method"] == ENGINE_GET_PAYLOAD_BODIES_BY_RANGE_V1
                    && body["params"] == json!(["0x64", "0x4"])
            });

            then.status(200).body(
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "result": vec![json!({ "transactions": [], "withdrawals": [] }); 4],
                })
                .to_string(),
            );
        });

        let eth1_api = Arc::new(Eth1Api::new(
            Arc::new(Config::mainnet()),
            Client::new(),
            Arc::default(),
            vec![server.url("/").parse()?],
            None,
            None,
        ));

        let bodies = eth1_api
            .get_payload_bodies_by_range::<Mainnet>(100, 4)
            .await?;

        assert_eq!(bodies.len(), 4);
        mock.assert_calls(1);

        Ok(())
    }

    // Payload bodies are requested on behalf of peers and without a capability filter, so an
    // execution client that does not implement them must not be reported as offline: `el_offline`
    // is part of `GET /eth/v1/node/syncing` and validator clients fail over on it.
    #[tokio::test]
    async fn test_unimplemented_payload_bodies_do_not_mark_the_endpoint_offline() -> Result<()> {
        let server = MockServer::start();

        server.mock(|when, then| {
            when.method(Method::POST).path("/");

            then.status(200).body(
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "error": {
                        "code": METHOD_NOT_FOUND_ERROR_CODE,
                        "message": "Method not found",
                    },
                })
                .to_string(),
            );
        });

        let eth1_api = Arc::new(Eth1Api::new(
            Arc::new(Config::mainnet()),
            Client::new(),
            Arc::default(),
            vec![server.url("/").parse()?],
            None,
            None,
        ));

        eth1_api
            .get_payload_bodies_by_range::<Mainnet>(100, 4)
            .await
            .expect_err("an execution client without payload bodies should fail the request");

        assert!(!eth1_api.el_offline());

        Ok(())
    }

    // The exemption is not limited to the rejection codes: peers can make payload body requests
    // fail in any way the execution client sees fit, including by timing it out under the load
    // they cause.
    #[tokio::test]
    async fn test_failing_payload_bodies_do_not_mark_the_endpoint_offline() -> Result<()> {
        let server = MockServer::start();

        server.mock(|when, then| {
            when.method(Method::POST).path("/");

            then.status(500).body("execution client is busy");
        });

        let eth1_api = Arc::new(Eth1Api::new(
            Arc::new(Config::mainnet()),
            Client::new(),
            Arc::default(),
            vec![server.url("/").parse()?],
            None,
            None,
        ));

        eth1_api
            .get_payload_bodies_by_hash::<Mainnet>(&[H256::repeat_byte(1)])
            .await
            .expect_err("an execution client that fails the request should fail it");

        assert!(!eth1_api.el_offline());

        Ok(())
    }

    // The exemption above is scoped to payload bodies. Every other engine method is one the node
    // itself depends on, so a client that does not implement it is offline.
    #[tokio::test]
    async fn test_unimplemented_other_methods_mark_the_endpoint_offline() -> Result<()> {
        let server = MockServer::start();

        server.mock(|when, then| {
            when.method(Method::POST).path("/");

            then.status(200).body(
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "error": {
                        "code": METHOD_NOT_FOUND_ERROR_CODE,
                        "message": "Method not found",
                    },
                })
                .to_string(),
            );
        });

        let eth1_api = Arc::new(Eth1Api::new(
            Arc::new(Config::mainnet()),
            Client::new(),
            Arc::default(),
            vec![server.url("/").parse()?],
            None,
            None,
        ));

        eth1_api
            .current_head_number()
            .await
            .expect_err("an execution client without the method should fail the request");

        assert!(eth1_api.el_offline());

        Ok(())
    }

    // An execution client that rejects even the minimum batch must make the request fail rather
    // than shrink forever.
    #[tokio::test]
    async fn test_payload_bodies_stop_shrinking_at_the_minimum_batch_size() -> Result<()> {
        let server = MockServer::start();

        let too_large = server.mock(|when, then| {
            when.method(Method::POST).path("/");

            then.status(200).body(
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "error": {
                        "code": TOO_LARGE_REQUEST_ERROR_CODE,
                        "message": "Too large request",
                    },
                })
                .to_string(),
            );
        });

        let eth1_api = Arc::new(Eth1Api::new(
            Arc::new(Config::mainnet()),
            Client::new(),
            Arc::default(),
            vec![server.url("/").parse()?],
            None,
            None,
        ));

        let block_hashes = (0..4 * PAYLOAD_BODIES_MIN_BATCH_SIZE)
            .map(|index| H256::from_low_u64_be(index as u64))
            .collect::<Vec<_>>();

        eth1_api
            .get_payload_bodies_by_hash::<Mainnet>(&block_hashes)
            .await
            .expect_err("an execution client rejecting every batch size should fail the request");

        // 128 -> 64 -> 32, then the floor stops the retries.
        too_large.assert_calls(3);
        assert_eq!(
            eth1_api.payload_bodies_batch_size(),
            PAYLOAD_BODIES_MIN_BATCH_SIZE,
        );

        Ok(())
    }

    // The `MockServer` is returned along with the API because dropping it returns the server to
    // `httpmock`'s pool, where another test can claim it and replace the mocks this one relies on.
    fn eth1_api_serving(body: &Value) -> Result<(MockServer, Arc<Eth1Api>)> {
        let server = MockServer::start();

        server.mock(|when, then| {
            when.method(Method::POST).path("/");
            then.status(200).body(body.to_string());
        });

        let server_url = server.url("/").parse()?;

        let eth1_api = Arc::new(Eth1Api::new(
            Arc::new(Config::mainnet()),
            Client::new(),
            Arc::default(),
            vec![server_url],
            None,
            None,
        ));

        Ok((server, eth1_api))
    }

    fn requested_hashes(request: &HttpMockRequest) -> usize {
        let body =
            serde_json::from_slice::<Value>(request.body_ref()).expect("request body is JSON");

        body["params"][0]
            .as_array()
            .expect("engine_getPayloadBodiesByHashV1 takes an array of block hashes")
            .len()
    }

    fn default_payload<P: Preset>() -> ExecutionPayload<P> {
        BellatrixExecutionPayload::default().into()
    }
}
