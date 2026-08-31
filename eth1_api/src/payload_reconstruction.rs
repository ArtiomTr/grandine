//! Rebuilding of full blocks from blinded ones stored without execution payloads.
//!
//! Everything except the transactions and withdrawals of a payload is already present in the
//! `ExecutionPayloadHeader` carried by a blinded block, so only those two lists have to be
//! fetched from the execution client.

use std::sync::Arc;

use anyhow::{Result, ensure};
use execution_engine::ExecutionPayloadBodyV1;
use fork_choice_control::StoredBlock;
use ssz::ContiguousList;
use std_ext::ArcExt as _;
use thiserror::Error;
use types::{
    bellatrix::{
        containers::{
            ExecutionPayload as BellatrixExecutionPayload,
            ExecutionPayloadHeader as BellatrixExecutionPayloadHeader,
        },
        primitives::Transaction,
    },
    capella::containers::{
        ExecutionPayload as CapellaExecutionPayload,
        ExecutionPayloadHeader as CapellaExecutionPayloadHeader, Withdrawal,
    },
    combined::{ExecutionPayload, SignedBeaconBlock, SignedBlindedBeaconBlock},
    deneb::containers::{
        ExecutionPayload as DenebExecutionPayload,
        ExecutionPayloadHeader as DenebExecutionPayloadHeader,
    },
    phase0::primitives::{ExecutionBlockHash, ExecutionBlockNumber},
    preset::Preset,
};

use crate::Eth1Api;

#[derive(Debug, Error)]
#[cfg_attr(test, derive(PartialEq, Eq))]
pub enum Error {
    #[error("execution client has no payload body for execution block {block_hash:?}")]
    BodyMissing { block_hash: ExecutionBlockHash },
    #[error(
        "payload rebuilt from execution client body does not match \
         the header of execution block {block_hash:?}"
    )]
    HeaderMismatch { block_hash: ExecutionBlockHash },
    #[error(
        "execution client returned {returned} payload bodies \
         for a range of {requested} execution blocks"
    )]
    BodyCountMismatch { requested: usize, returned: usize },
}

/// Rebuilds full blocks, fetching payload bodies by execution block hash.
///
/// Suitable for arbitrary, possibly non-canonical blocks.
async fn reconstruct_blocks<P: Preset>(
    eth1_api: &Eth1Api,
    blocks: impl IntoIterator<Item = Arc<SignedBlindedBeaconBlock<P>>>,
) -> Result<Vec<Arc<SignedBeaconBlock<P>>>> {
    let blocks = blocks.into_iter().collect::<Vec<_>>();

    let block_hashes = blocks
        .iter()
        .filter_map(|block| execution_block_hash(block))
        .collect::<Vec<_>>();

    let bodies = if block_hashes.is_empty() {
        vec![]
    } else {
        eth1_api.get_payload_bodies_by_hash(&block_hashes).await?
    };

    let mut bodies = bodies.into_iter();

    blocks
        .into_iter()
        .map(|block| {
            let body = match execution_block_hash(&block) {
                Some(block_hash) => Some(
                    bodies
                        .next()
                        .flatten()
                        .ok_or(Error::BodyMissing { block_hash })?,
                ),
                None => None,
            };

            reconstruct_block(block, body)
        })
        .collect()
}

/// Rebuilds full blocks, fetching payload bodies in a single call by execution block number.
///
/// The blocks must be canonical and ordered by slot. Consecutive beacon blocks always have
/// consecutive execution block numbers, because a skipped beacon slot produces no execution block.
/// Blocks that were never stored or have already been pruned leave gaps in the numbering, so each
/// contiguous run is fetched with a call of its own.
async fn reconstruct_blocks_in_range<P: Preset>(
    eth1_api: &Eth1Api,
    blocks: impl IntoIterator<Item = Arc<SignedBlindedBeaconBlock<P>>>,
) -> Result<Vec<Arc<SignedBeaconBlock<P>>>> {
    let blocks = blocks.into_iter().collect::<Vec<_>>();

    let numbers = blocks
        .iter()
        .filter_map(|block| execution_block_number(block))
        .collect::<Vec<_>>();

    let mut bodies = vec![];

    for run in numbers.chunk_by(|left, right| right.checked_sub(*left) == Some(1)) {
        let [start, ..] = run else { continue };

        let returned = eth1_api
            .get_payload_bodies_by_range(*start, u64::try_from(run.len())?)
            .await?;

        // A short response would pair the blocks of the next run with the wrong bodies.
        ensure!(
            returned.len() == run.len(),
            Error::BodyCountMismatch {
                requested: run.len(),
                returned: returned.len(),
            },
        );

        bodies.extend(returned);
    }

    let mut bodies = bodies.into_iter();

    blocks
        .into_iter()
        .map(|block| {
            let body = match execution_block_hash(&block) {
                Some(block_hash) => Some(
                    bodies
                        .next()
                        .flatten()
                        .ok_or(Error::BodyMissing { block_hash })?,
                ),
                None => None,
            };

            reconstruct_block(block, body)
        })
        .collect()
}

/// Rebuilds the blinded blocks in a mixed list of stored blocks, fetching payload bodies by
/// execution block hash.
///
/// Suitable for arbitrary, possibly non-canonical blocks.
pub async fn reconstruct_stored_blocks<P: Preset>(
    eth1_api: &Eth1Api,
    blocks: impl IntoIterator<Item = StoredBlock<P>>,
) -> Result<Vec<Arc<SignedBeaconBlock<P>>>> {
    let blocks = blocks.into_iter().collect::<Vec<_>>();

    let blinded = blocks
        .iter()
        .filter_map(|block| match block {
            StoredBlock::Full(_) => None,
            StoredBlock::Blinded(block) => Some(block.clone_arc()),
        })
        .collect::<Vec<_>>();

    let mut reconstructed = reconstruct_blocks(eth1_api, blinded).await?.into_iter();

    let blocks = blocks
        .into_iter()
        .map(|block| match block {
            StoredBlock::Full(block) => block,
            StoredBlock::Blinded(_) => reconstructed
                .next()
                .expect("every blinded block is reconstructed into exactly one full block"),
        })
        .collect();

    Ok(blocks)
}

/// Rebuilds the blinded blocks in a mixed list of stored blocks, fetching payload bodies by
/// execution block number.
///
/// The blocks must be canonical and ordered by slot. Each run of consecutive blinded blocks is
/// fetched with a single call.
pub async fn reconstruct_stored_blocks_in_range<P: Preset>(
    eth1_api: &Eth1Api,
    blocks: impl IntoIterator<Item = StoredBlock<P>>,
) -> Result<Vec<Arc<SignedBeaconBlock<P>>>> {
    let mut reconstructed = vec![];
    let mut blinded = vec![];

    for block in blocks {
        match block {
            StoredBlock::Full(block) => {
                let run = core::mem::take(&mut blinded);

                reconstructed.extend(reconstruct_blocks_in_range(eth1_api, run).await?);
                reconstructed.push(block);
            }
            StoredBlock::Blinded(block) => blinded.push(block),
        }
    }

    reconstructed.extend(reconstruct_blocks_in_range(eth1_api, blinded).await?);

    Ok(reconstructed)
}

fn reconstruct_block<P: Preset>(
    block: Arc<SignedBlindedBeaconBlock<P>>,
    body: Option<ExecutionPayloadBodyV1<P>>,
) -> Result<Arc<SignedBeaconBlock<P>>> {
    let payload = rebuild_payload(&block, body)?;

    Ok(Arc::new(
        Arc::unwrap_or_clone(block).with_execution_payload(payload)?,
    ))
}

/// Returns the execution block hash a payload body must be fetched for.
///
/// A zero block hash means the block was proposed before the merge and carries an empty payload
/// that is rebuilt locally.
fn execution_block_hash<P: Preset>(
    block: &SignedBlindedBeaconBlock<P>,
) -> Option<ExecutionBlockHash> {
    Some(block.execution_payload_header().block_hash()).filter(|block_hash| !block_hash.is_zero())
}

fn execution_block_number<P: Preset>(
    block: &SignedBlindedBeaconBlock<P>,
) -> Option<ExecutionBlockNumber> {
    execution_block_hash(block)?;
    block.execution_payload_header().block_number()
}

#[expect(clippy::too_many_lines)]
fn rebuild_payload<P: Preset>(
    block: &SignedBlindedBeaconBlock<P>,
    body: Option<ExecutionPayloadBodyV1<P>>,
) -> Result<ExecutionPayload<P>> {
    let (transactions, withdrawals) = match body {
        Some(ExecutionPayloadBodyV1 {
            transactions,
            withdrawals,
        }) => (transactions, withdrawals.unwrap_or_default()),
        None => (Arc::default(), ContiguousList::default()),
    };

    let withdrawals = withdrawals.map(Withdrawal::from);

    let payload = match block {
        SignedBlindedBeaconBlock::Bellatrix(block) => {
            let header = &block.message.body.execution_payload_header;

            let BellatrixExecutionPayloadHeader {
                parent_hash,
                fee_recipient,
                state_root,
                receipts_root,
                ref logs_bloom,
                prev_randao,
                block_number,
                gas_limit,
                gas_used,
                timestamp,
                ref extra_data,
                base_fee_per_gas,
                block_hash,
                transactions_root: _,
            } = *header;

            let payload = BellatrixExecutionPayload {
                parent_hash,
                fee_recipient,
                state_root,
                receipts_root,
                logs_bloom: *logs_bloom,
                prev_randao,
                block_number,
                gas_limit,
                gas_used,
                timestamp,
                extra_data: extra_data.clone_arc(),
                base_fee_per_gas,
                block_hash,
                transactions,
            };

            ensure!(
                BellatrixExecutionPayloadHeader::from(&payload) == *header,
                Error::HeaderMismatch { block_hash },
            );

            ExecutionPayload::Bellatrix(payload)
        }
        SignedBlindedBeaconBlock::Capella(block) => {
            let header = &block.message.body.execution_payload_header;

            let CapellaExecutionPayloadHeader {
                parent_hash,
                fee_recipient,
                state_root,
                receipts_root,
                ref logs_bloom,
                prev_randao,
                block_number,
                gas_limit,
                gas_used,
                timestamp,
                ref extra_data,
                base_fee_per_gas,
                block_hash,
                transactions_root: _,
                withdrawals_root: _,
            } = *header;

            let payload = CapellaExecutionPayload {
                parent_hash,
                fee_recipient,
                state_root,
                receipts_root,
                logs_bloom: *logs_bloom,
                prev_randao,
                block_number,
                gas_limit,
                gas_used,
                timestamp,
                extra_data: extra_data.clone_arc(),
                base_fee_per_gas,
                block_hash,
                transactions,
                withdrawals,
            };

            ensure!(
                CapellaExecutionPayloadHeader::from(&payload) == *header,
                Error::HeaderMismatch { block_hash },
            );

            ExecutionPayload::Capella(payload)
        }
        SignedBlindedBeaconBlock::Deneb(block) => deneb_payload(
            &block.message.body.execution_payload_header,
            transactions,
            withdrawals,
        )?,
        SignedBlindedBeaconBlock::Electra(block) => deneb_payload(
            &block.message.body.execution_payload_header,
            transactions,
            withdrawals,
        )?,
        SignedBlindedBeaconBlock::Fulu(block) => deneb_payload(
            &block.message.body.execution_payload_header,
            transactions,
            withdrawals,
        )?,
    };

    Ok(payload)
}

fn deneb_payload<P: Preset>(
    header: &DenebExecutionPayloadHeader<P>,
    transactions: Arc<ContiguousList<Transaction<P>, P::MaxTransactionsPerPayload>>,
    withdrawals: ContiguousList<Withdrawal, P::MaxWithdrawalsPerPayload>,
) -> Result<ExecutionPayload<P>> {
    let DenebExecutionPayloadHeader {
        parent_hash,
        fee_recipient,
        state_root,
        receipts_root,
        ref logs_bloom,
        prev_randao,
        block_number,
        gas_limit,
        gas_used,
        timestamp,
        ref extra_data,
        base_fee_per_gas,
        block_hash,
        transactions_root: _,
        withdrawals_root: _,
        blob_gas_used,
        excess_blob_gas,
    } = *header;

    let payload = DenebExecutionPayload {
        parent_hash,
        fee_recipient,
        state_root,
        receipts_root,
        logs_bloom: *logs_bloom,
        prev_randao,
        block_number,
        gas_limit,
        gas_used,
        timestamp,
        extra_data: extra_data.clone_arc(),
        base_fee_per_gas,
        block_hash,
        transactions,
        withdrawals,
        blob_gas_used,
        excess_blob_gas,
    };

    ensure!(
        DenebExecutionPayloadHeader::from(&payload) == *header,
        Error::HeaderMismatch { block_hash },
    );

    Ok(ExecutionPayload::Deneb(payload))
}

#[cfg(test)]
mod tests {
    use bls::SignatureBytes;
    use hex_literal::hex;
    use httpmock::{Method, MockServer};
    use reqwest::Client;
    use serde_json::{Value, json};
    use test_case::test_case;
    use types::{
        bellatrix::containers::{
            BeaconBlock as BellatrixBeaconBlock, BeaconBlockBody as BellatrixBeaconBlockBody,
        },
        capella::containers::{
            BeaconBlock as CapellaBeaconBlock, BeaconBlockBody as CapellaBeaconBlockBody,
        },
        combined::BeaconBlock,
        config::Config,
        deneb::containers::{
            BeaconBlock as DenebBeaconBlock, BeaconBlockBody as DenebBeaconBlockBody,
        },
        electra::containers::{
            BeaconBlock as ElectraBeaconBlock, BeaconBlockBody as ElectraBeaconBlockBody,
        },
        fulu::containers::{
            BeaconBlock as FuluBeaconBlock, BeaconBlockBody as FuluBeaconBlockBody,
        },
        nonstandard::Phase,
        phase0::primitives::H256,
        preset::Mainnet,
    };

    use super::*;

    const FIRST_BLOCK_NUMBER: ExecutionBlockNumber = 17_000_000;

    const TRANSACTIONS: [&[u8]; 2] = [&hex!("01020304"), &hex!("aabbccdd0e")];

    #[test_case(Phase::Bellatrix)]
    #[test_case(Phase::Capella)]
    #[test_case(Phase::Deneb)]
    #[test_case(Phase::Electra)]
    #[test_case(Phase::Fulu)]
    #[tokio::test]
    async fn reconstruction_by_hash_restores_the_original_block(phase: Phase) -> Result<()> {
        let block = full_block(phase, FIRST_BLOCK_NUMBER);
        let blinded = SignedBlindedBeaconBlock::try_from(block.clone())?;

        let (_server, eth1_api) = eth1_api_serving(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": [body_json(phase)],
        }))?;

        let reconstructed = reconstruct_blocks(&eth1_api, [Arc::new(blinded)]).await?;

        assert_eq!(reconstructed.len(), 1);
        assert_eq!(*reconstructed[0], block);

        Ok(())
    }

    #[tokio::test]
    async fn reconstruction_by_range_restores_consecutive_blocks() -> Result<()> {
        let blocks = [FIRST_BLOCK_NUMBER, FIRST_BLOCK_NUMBER + 1]
            .map(|block_number| full_block(Phase::Deneb, block_number));

        let blinded = blocks
            .iter()
            .cloned()
            .map(SignedBlindedBeaconBlock::try_from)
            .map(|block| block.map(Arc::new))
            .collect::<Result<Vec<_>, _>>()?;

        let (_server, eth1_api) = eth1_api_serving(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": [body_json(Phase::Deneb), body_json(Phase::Deneb)],
        }))?;

        let reconstructed = reconstruct_blocks_in_range(&eth1_api, blinded).await?;

        assert_eq!(reconstructed.len(), 2);
        assert_eq!(*reconstructed[0], blocks[0]);
        assert_eq!(*reconstructed[1], blocks[1]);

        Ok(())
    }

    #[tokio::test]
    async fn reconstruction_by_range_splits_blocks_with_a_gap() -> Result<()> {
        let blocks = [FIRST_BLOCK_NUMBER, FIRST_BLOCK_NUMBER + 2]
            .map(|block_number| full_block(Phase::Deneb, block_number));

        let blinded = blocks
            .iter()
            .cloned()
            .map(SignedBlindedBeaconBlock::try_from)
            .map(|block| block.map(Arc::new))
            .collect::<Result<Vec<_>, _>>()?;

        // Each run holds a single block, so the same one-body response serves both calls.
        let (_server, eth1_api) = eth1_api_serving(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": [body_json(Phase::Deneb)],
        }))?;

        let reconstructed = reconstruct_blocks_in_range(&eth1_api, blinded).await?;

        assert_eq!(reconstructed.len(), 2);
        assert_eq!(*reconstructed[0], blocks[0]);
        assert_eq!(*reconstructed[1], blocks[1]);

        Ok(())
    }

    #[tokio::test]
    async fn reconstruction_by_range_reports_a_short_response() -> Result<()> {
        let blinded = [FIRST_BLOCK_NUMBER, FIRST_BLOCK_NUMBER + 1]
            .map(|block_number| full_block(Phase::Deneb, block_number))
            .map(SignedBlindedBeaconBlock::try_from)
            .into_iter()
            .map(|block| block.map(Arc::new))
            .collect::<Result<Vec<_>, _>>()?;

        let (_server, eth1_api) = eth1_api_serving(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": [body_json(Phase::Deneb)],
        }))?;

        assert_eq!(
            reconstruct_blocks_in_range(&eth1_api, blinded)
                .await
                .expect_err("execution client returned fewer bodies than requested")
                .downcast::<Error>()?,
            Error::BodyCountMismatch {
                requested: 2,
                returned: 1,
            },
        );

        Ok(())
    }

    #[tokio::test]
    async fn reconstruction_reports_a_missing_body() -> Result<()> {
        let block = full_block(Phase::Deneb, FIRST_BLOCK_NUMBER);
        let blinded = SignedBlindedBeaconBlock::try_from(block.clone())?;
        let block_hash = blinded.execution_payload_header().block_hash();

        let (_server, eth1_api) = eth1_api_serving(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": [null],
        }))?;

        assert_eq!(
            reconstruct_blocks(&eth1_api, [Arc::new(blinded)])
                .await
                .expect_err("execution client has pruned the body")
                .downcast::<Error>()?,
            Error::BodyMissing { block_hash },
        );

        Ok(())
    }

    #[tokio::test]
    async fn reconstruction_rejects_a_body_that_does_not_match_the_header() -> Result<()> {
        let block = full_block(Phase::Deneb, FIRST_BLOCK_NUMBER);
        let blinded = SignedBlindedBeaconBlock::try_from(block.clone())?;
        let block_hash = blinded.execution_payload_header().block_hash();

        let (_server, eth1_api) = eth1_api_serving(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": [{
                "transactions": ["0xdeadbeef"],
                "withdrawals": [],
            }],
        }))?;

        assert_eq!(
            reconstruct_blocks(&eth1_api, [Arc::new(blinded)])
                .await
                .expect_err("transactions do not hash to the transactions root in the header")
                .downcast::<Error>()?,
            Error::HeaderMismatch { block_hash },
        );

        Ok(())
    }

    #[tokio::test]
    async fn stored_blocks_are_reconstructed_by_hash_keeping_full_blocks_as_they_are() -> Result<()>
    {
        let blocks = [FIRST_BLOCK_NUMBER, FIRST_BLOCK_NUMBER + 1]
            .map(|block_number| full_block(Phase::Deneb, block_number))
            .map(Arc::new);

        let (_server, eth1_api) = eth1_api_serving(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": [body_json(Phase::Deneb)],
        }))?;

        let stored = vec![
            StoredBlock::Full(blocks[0].clone_arc()),
            StoredBlock::Blinded(Arc::new(SignedBlindedBeaconBlock::try_from(
                blocks[1].as_ref().clone(),
            )?)),
        ];

        let reconstructed = reconstruct_stored_blocks(&eth1_api, stored).await?;

        assert_eq!(reconstructed, blocks);

        Ok(())
    }

    // Full and blinded blocks are interleaved when payload storage is toggled between restarts.
    // Each run of blinded blocks is consecutive on its own, so each is fetched by its own range
    // call.
    #[tokio::test]
    async fn stored_blocks_in_range_are_reconstructed_run_by_run() -> Result<()> {
        let blocks = [
            FIRST_BLOCK_NUMBER,
            FIRST_BLOCK_NUMBER + 1,
            FIRST_BLOCK_NUMBER + 2,
        ]
        .map(|block_number| full_block(Phase::Deneb, block_number))
        .map(Arc::new);

        let (_server, eth1_api) = eth1_api_serving(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": [body_json(Phase::Deneb)],
        }))?;

        let stored = vec![
            StoredBlock::Blinded(Arc::new(SignedBlindedBeaconBlock::try_from(
                blocks[0].as_ref().clone(),
            )?)),
            StoredBlock::Full(blocks[1].clone_arc()),
            StoredBlock::Blinded(Arc::new(SignedBlindedBeaconBlock::try_from(
                blocks[2].as_ref().clone(),
            )?)),
        ];

        let reconstructed = reconstruct_stored_blocks_in_range(&eth1_api, stored).await?;

        assert_eq!(reconstructed, blocks);

        Ok(())
    }

    // A block from before the merge carries a default payload with a zero block hash. The
    // execution client has nothing to serve for it, so it must be rebuilt locally. An `Eth1Api`
    // with no endpoints fails every request, which is what makes the absence of a call observable.
    #[tokio::test]
    async fn pre_merge_blocks_are_reconstructed_without_calling_the_execution_client() -> Result<()>
    {
        let block = BeaconBlock::<Mainnet>::Bellatrix(BellatrixBeaconBlock::default().into())
            .with_signature(SignatureBytes::default());

        let blinded = SignedBlindedBeaconBlock::try_from(block.clone())?;

        let eth1_api = Eth1Api::new(
            Arc::new(Config::mainnet()),
            Client::new(),
            Arc::default(),
            vec![],
            None,
            None,
        );

        let reconstructed = reconstruct_blocks(&eth1_api, [Arc::new(blinded.clone())]).await?;

        assert_eq!(*reconstructed[0], block);

        let reconstructed = reconstruct_blocks_in_range(&eth1_api, [Arc::new(blinded)]).await?;

        assert_eq!(*reconstructed[0], block);

        Ok(())
    }

    fn full_block(phase: Phase, block_number: ExecutionBlockNumber) -> SignedBeaconBlock<Mainnet> {
        let block = match phase {
            Phase::Bellatrix => BeaconBlock::Bellatrix(
                BellatrixBeaconBlock {
                    body: BellatrixBeaconBlockBody {
                        execution_payload: BellatrixExecutionPayload {
                            block_number,
                            block_hash: block_hash(block_number),
                            gas_limit: 30_000_000,
                            transactions: transactions(),
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
                            block_number,
                            block_hash: block_hash(block_number),
                            gas_limit: 30_000_000,
                            transactions: transactions(),
                            withdrawals: withdrawals(),
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
                        execution_payload: deneb_execution_payload(block_number),
                        ..DenebBeaconBlockBody::default()
                    },
                    ..DenebBeaconBlock::default()
                }
                .into(),
            ),
            Phase::Electra => BeaconBlock::Electra(
                ElectraBeaconBlock {
                    body: ElectraBeaconBlockBody {
                        execution_payload: deneb_execution_payload(block_number),
                        ..ElectraBeaconBlockBody::default()
                    },
                    ..ElectraBeaconBlock::default()
                }
                .into(),
            ),
            Phase::Fulu => BeaconBlock::Fulu(
                FuluBeaconBlock {
                    body: FuluBeaconBlockBody {
                        execution_payload: deneb_execution_payload(block_number),
                        ..FuluBeaconBlockBody::default()
                    },
                    ..FuluBeaconBlock::default()
                }
                .into(),
            ),
            _ => panic!("{phase} blocks have no blindable payload"),
        };

        block.with_signature(SignatureBytes::default())
    }

    fn deneb_execution_payload(
        block_number: ExecutionBlockNumber,
    ) -> DenebExecutionPayload<Mainnet> {
        DenebExecutionPayload {
            block_number,
            block_hash: block_hash(block_number),
            gas_limit: 30_000_000,
            blob_gas_used: 0x0002_0000,
            excess_blob_gas: 0x0004_0000,
            transactions: transactions(),
            withdrawals: withdrawals(),
            ..DenebExecutionPayload::default()
        }
    }

    fn body_json(phase: Phase) -> Value {
        let transactions = TRANSACTIONS
            .map(|transaction| format!("0x{}", hex::encode(transaction)))
            .to_vec();

        if phase == Phase::Bellatrix {
            return json!({ "transactions": transactions, "withdrawals": null });
        }

        json!({
            "transactions": transactions,
            "withdrawals": [{
                "index": "0x18561",
                "validatorIndex": "0x7c2e8",
                "address": "0xf97e180c050e5ab072211ad2c213eb5aee4df134",
                "amount": "0x18111",
            }],
        })
    }

    fn transactions()
    -> Arc<ContiguousList<Transaction<Mainnet>, <Mainnet as Preset>::MaxTransactionsPerPayload>>
    {
        Arc::new(
            TRANSACTIONS
                .map(|transaction| {
                    Transaction::<Mainnet>::try_from(transaction.to_vec())
                        .expect("test transaction fits in a transaction")
                })
                .try_into()
                .expect("test transactions fit in a payload"),
        )
    }

    fn withdrawals() -> ContiguousList<Withdrawal, <Mainnet as Preset>::MaxWithdrawalsPerPayload> {
        ContiguousList::try_from([Withdrawal {
            index: 0x0001_8561,
            validator_index: 0x0007_c2e8,
            address: hex!("f97e180c050e5ab072211ad2c213eb5aee4df134").into(),
            amount: 0x0001_8111,
        }])
        .expect("a single withdrawal fits in a payload")
    }

    fn block_hash(block_number: ExecutionBlockNumber) -> ExecutionBlockHash {
        H256::from_low_u64_be(block_number)
    }

    // The `MockServer` is returned along with the API because dropping it returns the server to
    // `httpmock`'s pool, where another test can claim it and replace the mocks this one relies on.
    fn eth1_api_serving(body: &Value) -> Result<(MockServer, Eth1Api)> {
        let server = MockServer::start();

        server.mock(|when, then| {
            when.method(Method::POST).path("/");
            then.status(200).body(body.to_string());
        });

        let eth1_api = Eth1Api::new(
            Arc::new(Config::mainnet()),
            Client::new(),
            Arc::default(),
            vec![server.url("/").parse()?],
            None,
            None,
        );

        Ok((server, eth1_api))
    }
}
