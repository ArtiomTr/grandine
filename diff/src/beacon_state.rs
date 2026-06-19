use std::sync::Arc;

use ssz::{Hc, ReadError, Size, Ssz, SszRead, SszReadDefault, SszSize, SszWrite, WriteError};
use std_ext::ArcExt as _;
use types::{
    Ptc,
    altair::containers::SyncCommittee,
    cache::Cache,
    bellatrix::containers::ExecutionPayloadHeader as BellatrixExecutionPayloadHeader,
    capella::containers::{
        ExecutionPayloadHeader as CapellaExecutionPayloadHeader, HistoricalSummary, Withdrawal,
    },
    combined::BeaconState as CombinedBeaconState,
    deneb::containers::ExecutionPayloadHeader as DenebExecutionPayloadHeader,
    electra::containers::{PendingConsolidation, PendingDeposit, PendingPartialWithdrawal},
    gloas::containers::{
        Builder, BuilderPendingPayment, BuilderPendingWithdrawal, ExecutionPayloadBid,
    },
    phase0::{
        consts::JustificationBitsLength,
        containers::{BeaconBlockHeader, Checkpoint, Eth1Data, PendingAttestation},
        primitives::H256,
    },
    preset::{
        BuilderPendingPaymentsLength, MaxAttestationsPerEpoch, Preset, ProposerLookaheadLength,
        PtcWindowLength, SlotsPerEth1VotingPeriod, SlotsPerHistoricalRoot,
    },
    traits::{BeaconState as _, PostCapellaBeaconState, PostFuluBeaconState, PostGloasBeaconState},
};

use crate::{
    Error, ListBalancesPatch, ListPositionalPatch, ListQueuePatch, Patch, PatchResult,
    ReplacePatch, ValidatorListPatch, VectorPatch,
};

// Versioned wrapper. New on-disk layouts go in a new `Vn` variant; older variants stay readable.
// The SSZ encoding is a 1-byte version selector followed by the variant payload.
#[derive(Clone, Debug)]
pub enum BeaconStatePatch<P: Preset> {
    V1(BeaconStatePatchV1<P>),
}

impl<P: Preset> Patch<Arc<CombinedBeaconState<P>>> for BeaconStatePatch<P> {
    fn diff(
        base: &Arc<CombinedBeaconState<P>>,
        changed: &Arc<CombinedBeaconState<P>>,
    ) -> PatchResult<Self> {
        Ok(Self::V1(Patch::diff(base, changed)?))
    }

    fn apply(self, base: Arc<CombinedBeaconState<P>>) -> PatchResult<Arc<CombinedBeaconState<P>>> {
        match self {
            Self::V1(patch) => patch.apply(base),
        }
    }
}

// Opaque to outside callers: all fields are private and the only constructors live in this module.
// Outside code can only obtain a value via the `BeaconStatePatch::V1` variant produced by `diff`.
#[derive(Clone, Debug, Ssz)]
#[ssz(derive_hash = false)]
pub struct BeaconStatePatchV1<P: Preset> {
    // Discriminant used to reject cross-phase diffs and to drive the fork-gated fields.
    phase: u8,

    // > Versioning (`fork` is phase-invariant and has no setter, so it is not patched)
    genesis_time: ReplacePatch<u64>,
    genesis_validators_root: ReplacePatch<H256>,
    slot: ReplacePatch<u64>,

    // > History
    latest_block_header: ReplacePatch<BeaconBlockHeader>,
    block_roots: VectorPatch<H256, SlotsPerHistoricalRoot<P>>,
    state_roots: VectorPatch<H256, SlotsPerHistoricalRoot<P>>,
    historical_roots: ListPositionalPatch<H256, P::HistoricalRootsLimit>,

    // > Eth1
    eth1_data: ReplacePatch<Eth1Data>,
    eth1_data_votes: ListPositionalPatch<Eth1Data, SlotsPerEth1VotingPeriod<P>>,
    eth1_deposit_index: ReplacePatch<u64>,

    // > Registry
    validators: ValidatorListPatch<P::ValidatorRegistryLimit>,
    balances: ListBalancesPatch<P::ValidatorRegistryLimit>,

    // > Randomness / Slashings
    randao_mixes: VectorPatch<H256, P::EpochsPerHistoricalVector>,
    slashings: VectorPatch<u64, P::EpochsPerSlashingsVector>,

    // > Phase 0 attestations (absent from Altair on)
    previous_epoch_attestations:
        Option<ListPositionalPatch<PendingAttestation<P>, MaxAttestationsPerEpoch<P>>>,
    current_epoch_attestations:
        Option<ListPositionalPatch<PendingAttestation<P>, MaxAttestationsPerEpoch<P>>>,

    // > Finality
    justification_bits: ReplacePatch<ssz::BitVector<JustificationBitsLength>>,
    previous_justified_checkpoint: ReplacePatch<Checkpoint>,
    current_justified_checkpoint: ReplacePatch<Checkpoint>,
    finalized_checkpoint: ReplacePatch<Checkpoint>,

    // > Participation / Sync (Altair on)
    previous_epoch_participation: Option<ListPositionalPatch<u8, P::ValidatorRegistryLimit>>,
    current_epoch_participation: Option<ListPositionalPatch<u8, P::ValidatorRegistryLimit>>,
    inactivity_scores: Option<ListPositionalPatch<u64, P::ValidatorRegistryLimit>>,
    current_sync_committee: Option<ReplacePatch<Arc<Hc<SyncCommittee<P>>>>>,
    next_sync_committee: Option<ReplacePatch<Arc<Hc<SyncCommittee<P>>>>>,

    // > Execution payload header (Bellatrix on, dropped in Gloas for the payload bid)
    latest_execution_payload_header: Option<ReplacePatch<CombinedExecutionPayloadHeader<P>>>,

    // > Withdrawals (Capella on)
    next_withdrawal_index: Option<ReplacePatch<u64>>,
    next_withdrawal_validator_index: Option<ReplacePatch<u64>>,
    historical_summaries: Option<ListPositionalPatch<HistoricalSummary, P::HistoricalRootsLimit>>,

    // > Electra
    deposit_requests_start_index: Option<ReplacePatch<u64>>,
    deposit_balance_to_consume: Option<ReplacePatch<u64>>,
    exit_balance_to_consume: Option<ReplacePatch<u64>>,
    earliest_exit_epoch: Option<ReplacePatch<u64>>,
    consolidation_balance_to_consume: Option<ReplacePatch<u64>>,
    earliest_consolidation_epoch: Option<ReplacePatch<u64>>,
    pending_deposits: Option<ListQueuePatch<PendingDeposit, P::PendingDepositsLimit>>,
    pending_partial_withdrawals:
        Option<ListQueuePatch<PendingPartialWithdrawal, P::PendingPartialWithdrawalsLimit>>,
    pending_consolidations:
        Option<ListPositionalPatch<PendingConsolidation, P::PendingConsolidationsLimit>>,

    // > Fulu
    proposer_lookahead: Option<VectorPatch<u64, ProposerLookaheadLength<P>>>,

    // > Gloas
    latest_execution_payload_bid: Option<ReplacePatch<ExecutionPayloadBid<P>>>,
    execution_payload_availability: Option<ReplacePatch<ssz::BitVector<SlotsPerHistoricalRoot<P>>>>,
    builder_pending_payments:
        Option<VectorPatch<BuilderPendingPayment, BuilderPendingPaymentsLength<P>>>,
    builder_pending_withdrawals:
        Option<ListQueuePatch<BuilderPendingWithdrawal, P::BuilderPendingWithdrawalsLimit>>,
    latest_block_hash: Option<ReplacePatch<H256>>,
    payload_expected_withdrawals: Option<ListQueuePatch<Withdrawal, P::MaxWithdrawalsPerPayload>>,
    builders: Option<ListPositionalPatch<Builder, P::BuilderRegistryLimit>>,
    next_withdrawal_builder_index: Option<ReplacePatch<u64>>,
    ptc_window: Option<VectorPatch<Ptc<P>, PtcWindowLength<P>>>,
}

impl<P: Preset> Patch<Arc<CombinedBeaconState<P>>> for BeaconStatePatchV1<P> {
    fn diff(
        base: &Arc<CombinedBeaconState<P>>,
        changed: &Arc<CombinedBeaconState<P>>,
    ) -> PatchResult<Self> {
        if base.phase() != changed.phase() {
            return Err(Error::CrossPhaseDiff);
        }

        let altair = base.post_altair().zip(changed.post_altair());
        let capella = base.post_capella().zip(changed.post_capella());
        let electra = base.post_electra().zip(changed.post_electra());
        let fulu = base.post_fulu().zip(changed.post_fulu());
        let gloas = base.post_gloas().zip(changed.post_gloas());

        let (previous_epoch_attestations, current_epoch_attestations) =
            match (base.as_ref(), changed.as_ref()) {
                (CombinedBeaconState::Phase0(b), CombinedBeaconState::Phase0(c)) => (
                    Some(Patch::diff(
                        &b.previous_epoch_attestations,
                        &c.previous_epoch_attestations,
                    )?),
                    Some(Patch::diff(
                        &b.current_epoch_attestations,
                        &c.current_epoch_attestations,
                    )?),
                ),
                _ => (None, None),
            };

        let historical_summaries =
            match (base.as_ref(), changed.as_ref()) {
                (CombinedBeaconState::Capella(b), CombinedBeaconState::Capella(c)) => Some(
                    Patch::diff(&b.historical_summaries, &c.historical_summaries)?,
                ),
                (CombinedBeaconState::Deneb(b), CombinedBeaconState::Deneb(c)) => Some(
                    Patch::diff(&b.historical_summaries, &c.historical_summaries)?,
                ),
                (CombinedBeaconState::Electra(b), CombinedBeaconState::Electra(c)) => Some(
                    Patch::diff(&b.historical_summaries, &c.historical_summaries)?,
                ),
                (CombinedBeaconState::Fulu(b), CombinedBeaconState::Fulu(c)) => Some(Patch::diff(
                    &b.historical_summaries,
                    &c.historical_summaries,
                )?),
                (CombinedBeaconState::Gloas(b), CombinedBeaconState::Gloas(c)) => Some(
                    Patch::diff(&b.historical_summaries, &c.historical_summaries)?,
                ),
                _ => None,
            };

        let latest_execution_payload_header = match (base.as_ref(), changed.as_ref()) {
            (CombinedBeaconState::Bellatrix(b), CombinedBeaconState::Bellatrix(c)) => {
                Some(Patch::diff(
                    &CombinedExecutionPayloadHeader::Bellatrix(
                        b.latest_execution_payload_header.clone(),
                    ),
                    &CombinedExecutionPayloadHeader::Bellatrix(
                        c.latest_execution_payload_header.clone(),
                    ),
                )?)
            }
            (CombinedBeaconState::Capella(b), CombinedBeaconState::Capella(c)) => {
                Some(Patch::diff(
                    &CombinedExecutionPayloadHeader::Capella(
                        b.latest_execution_payload_header.clone(),
                    ),
                    &CombinedExecutionPayloadHeader::Capella(
                        c.latest_execution_payload_header.clone(),
                    ),
                )?)
            }
            (CombinedBeaconState::Deneb(b), CombinedBeaconState::Deneb(c)) => Some(Patch::diff(
                &CombinedExecutionPayloadHeader::Deneb(b.latest_execution_payload_header.clone()),
                &CombinedExecutionPayloadHeader::Deneb(c.latest_execution_payload_header.clone()),
            )?),
            (CombinedBeaconState::Electra(b), CombinedBeaconState::Electra(c)) => {
                Some(Patch::diff(
                    &CombinedExecutionPayloadHeader::Deneb(
                        b.latest_execution_payload_header.clone(),
                    ),
                    &CombinedExecutionPayloadHeader::Deneb(
                        c.latest_execution_payload_header.clone(),
                    ),
                )?)
            }
            (CombinedBeaconState::Fulu(b), CombinedBeaconState::Fulu(c)) => Some(Patch::diff(
                &CombinedExecutionPayloadHeader::Deneb(b.latest_execution_payload_header.clone()),
                &CombinedExecutionPayloadHeader::Deneb(c.latest_execution_payload_header.clone()),
            )?),
            _ => None,
        };

        Ok(Self {
            phase: changed.phase() as u8,

            genesis_time: Patch::diff(&base.genesis_time(), &changed.genesis_time())?,
            genesis_validators_root: Patch::diff(
                &base.genesis_validators_root(),
                &changed.genesis_validators_root(),
            )?,
            slot: Patch::diff(&base.slot(), &changed.slot())?,

            latest_block_header: Patch::diff(
                &base.latest_block_header(),
                &changed.latest_block_header(),
            )?,
            block_roots: Patch::diff(base.block_roots(), changed.block_roots())?,
            state_roots: Patch::diff(base.state_roots(), changed.state_roots())?,
            historical_roots: Patch::diff(base.historical_roots(), changed.historical_roots())?,

            eth1_data: Patch::diff(&base.eth1_data(), &changed.eth1_data())?,
            eth1_data_votes: Patch::diff(base.eth1_data_votes(), changed.eth1_data_votes())?,
            eth1_deposit_index: Patch::diff(
                &base.eth1_deposit_index(),
                &changed.eth1_deposit_index(),
            )?,

            validators: Patch::diff(base.validators(), changed.validators())?,
            balances: Patch::diff(base.balances(), changed.balances())?,

            randao_mixes: Patch::diff(base.randao_mixes(), changed.randao_mixes())?,
            slashings: Patch::diff(base.slashings(), changed.slashings())?,

            previous_epoch_attestations,
            current_epoch_attestations,

            justification_bits: Patch::diff(
                &base.justification_bits(),
                &changed.justification_bits(),
            )?,
            previous_justified_checkpoint: Patch::diff(
                &base.previous_justified_checkpoint(),
                &changed.previous_justified_checkpoint(),
            )?,
            current_justified_checkpoint: Patch::diff(
                &base.current_justified_checkpoint(),
                &changed.current_justified_checkpoint(),
            )?,
            finalized_checkpoint: Patch::diff(
                &base.finalized_checkpoint(),
                &changed.finalized_checkpoint(),
            )?,

            previous_epoch_participation: altair
                .map(|(b, c)| {
                    Patch::diff(
                        b.previous_epoch_participation(),
                        c.previous_epoch_participation(),
                    )
                })
                .transpose()?,
            current_epoch_participation: altair
                .map(|(b, c)| {
                    Patch::diff(
                        b.current_epoch_participation(),
                        c.current_epoch_participation(),
                    )
                })
                .transpose()?,
            inactivity_scores: altair
                .map(|(b, c)| Patch::diff(b.inactivity_scores(), c.inactivity_scores()))
                .transpose()?,
            current_sync_committee: altair
                .map(|(b, c)| Patch::diff(b.current_sync_committee(), c.current_sync_committee()))
                .transpose()?,
            next_sync_committee: altair
                .map(|(b, c)| Patch::diff(b.next_sync_committee(), c.next_sync_committee()))
                .transpose()?,

            latest_execution_payload_header,

            next_withdrawal_index: capella
                .map(|(b, c)| Patch::diff(&b.next_withdrawal_index(), &c.next_withdrawal_index()))
                .transpose()?,
            next_withdrawal_validator_index: capella
                .map(|(b, c)| {
                    Patch::diff(
                        &b.next_withdrawal_validator_index(),
                        &c.next_withdrawal_validator_index(),
                    )
                })
                .transpose()?,
            historical_summaries,

            deposit_requests_start_index: electra
                .map(|(b, c)| {
                    Patch::diff(
                        &b.deposit_requests_start_index(),
                        &c.deposit_requests_start_index(),
                    )
                })
                .transpose()?,
            deposit_balance_to_consume: electra
                .map(|(b, c)| {
                    Patch::diff(
                        &b.deposit_balance_to_consume(),
                        &c.deposit_balance_to_consume(),
                    )
                })
                .transpose()?,
            exit_balance_to_consume: electra
                .map(|(b, c)| {
                    Patch::diff(&b.exit_balance_to_consume(), &c.exit_balance_to_consume())
                })
                .transpose()?,
            earliest_exit_epoch: electra
                .map(|(b, c)| Patch::diff(&b.earliest_exit_epoch(), &c.earliest_exit_epoch()))
                .transpose()?,
            consolidation_balance_to_consume: electra
                .map(|(b, c)| {
                    Patch::diff(
                        &b.consolidation_balance_to_consume(),
                        &c.consolidation_balance_to_consume(),
                    )
                })
                .transpose()?,
            earliest_consolidation_epoch: electra
                .map(|(b, c)| {
                    Patch::diff(
                        &b.earliest_consolidation_epoch(),
                        &c.earliest_consolidation_epoch(),
                    )
                })
                .transpose()?,
            pending_deposits: electra
                .map(|(b, c)| Patch::diff(b.pending_deposits(), c.pending_deposits()))
                .transpose()?,
            pending_partial_withdrawals: electra
                .map(|(b, c)| {
                    Patch::diff(
                        b.pending_partial_withdrawals(),
                        c.pending_partial_withdrawals(),
                    )
                })
                .transpose()?,
            pending_consolidations: electra
                .map(|(b, c)| Patch::diff(b.pending_consolidations(), c.pending_consolidations()))
                .transpose()?,

            proposer_lookahead: fulu
                .map(|(b, c)| Patch::diff(b.proposer_lookahead(), c.proposer_lookahead()))
                .transpose()?,

            latest_execution_payload_bid: gloas
                .map(|(b, c)| {
                    Patch::diff(
                        b.latest_execution_payload_bid(),
                        c.latest_execution_payload_bid(),
                    )
                })
                .transpose()?,
            execution_payload_availability: gloas
                .map(|(b, c)| {
                    Patch::diff(
                        &b.execution_payload_availability(),
                        &c.execution_payload_availability(),
                    )
                })
                .transpose()?,
            builder_pending_payments: gloas
                .map(|(b, c)| {
                    Patch::diff(b.builder_pending_payments(), c.builder_pending_payments())
                })
                .transpose()?,
            builder_pending_withdrawals: gloas
                .map(|(b, c)| {
                    Patch::diff(
                        b.builder_pending_withdrawals(),
                        c.builder_pending_withdrawals(),
                    )
                })
                .transpose()?,
            latest_block_hash: gloas
                .map(|(b, c)| Patch::diff(&b.latest_block_hash(), &c.latest_block_hash()))
                .transpose()?,
            payload_expected_withdrawals: gloas
                .map(|(b, c)| {
                    Patch::diff(
                        b.payload_expected_withdrawals(),
                        c.payload_expected_withdrawals(),
                    )
                })
                .transpose()?,
            builders: gloas
                .map(|(b, c)| Patch::diff(b.builders(), c.builders()))
                .transpose()?,
            next_withdrawal_builder_index: gloas
                .map(|(b, c)| {
                    Patch::diff(
                        &b.next_withdrawal_builder_index(),
                        &c.next_withdrawal_builder_index(),
                    )
                })
                .transpose()?,
            ptc_window: gloas
                .map(|(b, c)| Patch::diff(b.ptc_window(), c.ptc_window()))
                .transpose()?,
        })
    }

    fn apply(
        self,
        mut base: Arc<CombinedBeaconState<P>>,
    ) -> PatchResult<Arc<CombinedBeaconState<P>>> {
        let base_ptr = base.make_mut();

        if self.phase != base_ptr.phase() as u8 {
            return Err(Error::PatchPhaseMismatch);
        }

        // The patch relocates the state to a different slot without going through the state
        // transition machinery that would normally invalidate cached values (`advance_slot` /
        // `advance_epoch`). The base may be a referential frame whose slot/epoch-relative caches
        // were populated in place while it sat at its own slot, so carrying them onto the relocated
        // state would expose stale data (e.g. a proposer index or shuffling from the wrong epoch).
        // Clear the cache; callers recompute what they need on the resulting state.
        *base_ptr.cache_mut() = Cache::default();

        {
            let slot = base_ptr.genesis_time_mut();
            *slot = self.genesis_time.apply(slot.clone())?;

            let slot = base_ptr.genesis_validators_root_mut();
            *slot = self.genesis_validators_root.apply(slot.clone())?;

            let slot = base_ptr.slot_mut();
            *slot = self.slot.apply(slot.clone())?;

            let slot = base_ptr.latest_block_header_mut();
            *slot = self.latest_block_header.apply(slot.clone())?;

            let slot = base_ptr.block_roots_mut();
            *slot = self.block_roots.apply(slot.clone())?;

            let slot = base_ptr.state_roots_mut();
            *slot = self.state_roots.apply(slot.clone())?;

            let slot = base_ptr.historical_roots_mut();
            *slot = self.historical_roots.apply(slot.clone())?;

            let slot = base_ptr.eth1_data_mut();
            *slot = self.eth1_data.apply(slot.clone())?;

            let slot = base_ptr.eth1_data_votes_mut();
            *slot = self.eth1_data_votes.apply(slot.clone())?;

            let slot = base_ptr.eth1_deposit_index_mut();
            *slot = self.eth1_deposit_index.apply(slot.clone())?;

            let slot = base_ptr.validators_mut();
            *slot = self.validators.apply(slot.clone())?;

            let slot = base_ptr.balances_mut();
            *slot = self.balances.apply(slot.clone())?;

            let slot = base_ptr.randao_mixes_mut();
            *slot = self.randao_mixes.apply(slot.clone())?;

            let slot = base_ptr.slashings_mut();
            *slot = self.slashings.apply(slot.clone())?;

            let slot = base_ptr.justification_bits_mut();
            *slot = self.justification_bits.apply(slot.clone())?;

            let slot = base_ptr.previous_justified_checkpoint_mut();
            *slot = self.previous_justified_checkpoint.apply(slot.clone())?;

            let slot = base_ptr.current_justified_checkpoint_mut();
            *slot = self.current_justified_checkpoint.apply(slot.clone())?;

            let slot = base_ptr.finalized_checkpoint_mut();
            *slot = self.finalized_checkpoint.apply(slot.clone())?;
        }

        match (
            self.previous_epoch_attestations,
            self.current_epoch_attestations,
            &mut *base_ptr,
        ) {
            (Some(previous), Some(current), CombinedBeaconState::Phase0(state)) => {
                state.previous_epoch_attestations =
                    previous.apply(state.previous_epoch_attestations.clone())?;
                state.current_epoch_attestations =
                    current.apply(state.current_epoch_attestations.clone())?;
            }
            (None, None, _) => {}
            _ => return Err(Error::PatchPhaseMismatch),
        }

        if let Some(state) = base_ptr.post_altair_mut() {
            let patch = self
                .previous_epoch_participation
                .ok_or(Error::PatchPhaseMismatch)?;
            let slot = state.previous_epoch_participation_mut();
            *slot = patch.apply(slot.clone())?;

            let patch = self
                .current_epoch_participation
                .ok_or(Error::PatchPhaseMismatch)?;
            let slot = state.current_epoch_participation_mut();
            *slot = patch.apply(slot.clone())?;

            let patch = self.inactivity_scores.ok_or(Error::PatchPhaseMismatch)?;
            let slot = state.inactivity_scores_mut();
            *slot = patch.apply(slot.clone())?;

            let patch = self
                .current_sync_committee
                .ok_or(Error::PatchPhaseMismatch)?;
            let slot = state.current_sync_committee_mut();
            *slot = patch.apply(slot.clone())?;

            let patch = self.next_sync_committee.ok_or(Error::PatchPhaseMismatch)?;
            let slot = state.next_sync_committee_mut();
            *slot = patch.apply(slot.clone())?;
        }

        match (self.latest_execution_payload_header, &mut *base_ptr) {
            (Some(patch), CombinedBeaconState::Bellatrix(state)) => {
                let applied = patch.apply(CombinedExecutionPayloadHeader::Bellatrix(
                    state.latest_execution_payload_header.clone(),
                ))?;
                state.latest_execution_payload_header = match applied {
                    CombinedExecutionPayloadHeader::Bellatrix(header) => header,
                    _ => return Err(Error::PatchPhaseMismatch),
                };
            }
            (Some(patch), CombinedBeaconState::Capella(state)) => {
                let applied = patch.apply(CombinedExecutionPayloadHeader::Capella(
                    state.latest_execution_payload_header.clone(),
                ))?;
                state.latest_execution_payload_header = match applied {
                    CombinedExecutionPayloadHeader::Capella(header) => header,
                    _ => return Err(Error::PatchPhaseMismatch),
                };
            }
            (Some(patch), CombinedBeaconState::Deneb(state)) => {
                let applied = patch.apply(CombinedExecutionPayloadHeader::Deneb(
                    state.latest_execution_payload_header.clone(),
                ))?;
                state.latest_execution_payload_header = match applied {
                    CombinedExecutionPayloadHeader::Deneb(header) => header,
                    _ => return Err(Error::PatchPhaseMismatch),
                };
            }
            (Some(patch), CombinedBeaconState::Electra(state)) => {
                let applied = patch.apply(CombinedExecutionPayloadHeader::Deneb(
                    state.latest_execution_payload_header.clone(),
                ))?;
                state.latest_execution_payload_header = match applied {
                    CombinedExecutionPayloadHeader::Deneb(header) => header,
                    _ => return Err(Error::PatchPhaseMismatch),
                };
            }
            (Some(patch), CombinedBeaconState::Fulu(state)) => {
                let applied = patch.apply(CombinedExecutionPayloadHeader::Deneb(
                    state.latest_execution_payload_header.clone(),
                ))?;
                state.latest_execution_payload_header = match applied {
                    CombinedExecutionPayloadHeader::Deneb(header) => header,
                    _ => return Err(Error::PatchPhaseMismatch),
                };
            }
            (
                None,
                CombinedBeaconState::Phase0(_)
                | CombinedBeaconState::Altair(_)
                | CombinedBeaconState::Gloas(_),
            ) => {}
            _ => return Err(Error::PatchPhaseMismatch),
        }

        if let Some(state) = post_capella_mut(&mut *base_ptr) {
            let patch = self
                .next_withdrawal_index
                .ok_or(Error::PatchPhaseMismatch)?;
            let slot = state.next_withdrawal_index_mut();
            *slot = patch.apply(slot.clone())?;

            let patch = self
                .next_withdrawal_validator_index
                .ok_or(Error::PatchPhaseMismatch)?;
            let slot = state.next_withdrawal_validator_index_mut();
            *slot = patch.apply(slot.clone())?;
        }

        match (self.historical_summaries, &mut *base_ptr) {
            (Some(patch), CombinedBeaconState::Capella(state)) => {
                state.historical_summaries = patch.apply(state.historical_summaries.clone())?;
            }
            (Some(patch), CombinedBeaconState::Deneb(state)) => {
                state.historical_summaries = patch.apply(state.historical_summaries.clone())?;
            }
            (Some(patch), CombinedBeaconState::Electra(state)) => {
                state.historical_summaries = patch.apply(state.historical_summaries.clone())?;
            }
            (Some(patch), CombinedBeaconState::Fulu(state)) => {
                state.historical_summaries = patch.apply(state.historical_summaries.clone())?;
            }
            (Some(patch), CombinedBeaconState::Gloas(state)) => {
                state.historical_summaries = patch.apply(state.historical_summaries.clone())?;
            }
            (
                None,
                CombinedBeaconState::Phase0(_)
                | CombinedBeaconState::Altair(_)
                | CombinedBeaconState::Bellatrix(_),
            ) => {}
            _ => return Err(Error::PatchPhaseMismatch),
        }

        if let Some(state) = base_ptr.post_electra_mut() {
            let patch = self
                .deposit_requests_start_index
                .ok_or(Error::PatchPhaseMismatch)?;
            let slot = state.deposit_requests_start_index_mut();
            *slot = patch.apply(slot.clone())?;

            let patch = self
                .deposit_balance_to_consume
                .ok_or(Error::PatchPhaseMismatch)?;
            let slot = state.deposit_balance_to_consume_mut();
            *slot = patch.apply(slot.clone())?;

            let patch = self
                .exit_balance_to_consume
                .ok_or(Error::PatchPhaseMismatch)?;
            let slot = state.exit_balance_to_consume_mut();
            *slot = patch.apply(slot.clone())?;

            let patch = self.earliest_exit_epoch.ok_or(Error::PatchPhaseMismatch)?;
            let slot = state.earliest_exit_epoch_mut();
            *slot = patch.apply(slot.clone())?;

            let patch = self
                .consolidation_balance_to_consume
                .ok_or(Error::PatchPhaseMismatch)?;
            let slot = state.consolidation_balance_to_consume_mut();
            *slot = patch.apply(slot.clone())?;

            let patch = self
                .earliest_consolidation_epoch
                .ok_or(Error::PatchPhaseMismatch)?;
            let slot = state.earliest_consolidation_epoch_mut();
            *slot = patch.apply(slot.clone())?;

            let patch = self.pending_deposits.ok_or(Error::PatchPhaseMismatch)?;
            let slot = state.pending_deposits_mut();
            *slot = patch.apply(slot.clone())?;

            let patch = self
                .pending_partial_withdrawals
                .ok_or(Error::PatchPhaseMismatch)?;
            let slot = state.pending_partial_withdrawals_mut();
            *slot = patch.apply(slot.clone())?;

            let patch = self
                .pending_consolidations
                .ok_or(Error::PatchPhaseMismatch)?;
            let slot = state.pending_consolidations_mut();
            *slot = patch.apply(slot.clone())?;
        }

        if let Some(state) = post_fulu_mut(&mut *base_ptr) {
            let patch = self.proposer_lookahead.ok_or(Error::PatchPhaseMismatch)?;
            let slot = state.proposer_lookahead_mut();
            *slot = patch.apply(slot.clone())?;
        }

        if let Some(state) = post_gloas_mut(&mut *base_ptr) {
            let patch = self
                .latest_execution_payload_bid
                .ok_or(Error::PatchPhaseMismatch)?;
            let slot = state.latest_execution_payload_bid_mut();
            *slot = patch.apply(slot.clone())?;

            let patch = self
                .execution_payload_availability
                .ok_or(Error::PatchPhaseMismatch)?;
            let slot = state.execution_payload_availability_mut();
            *slot = patch.apply(slot.clone())?;

            let patch = self
                .builder_pending_payments
                .ok_or(Error::PatchPhaseMismatch)?;
            let slot = state.builder_pending_payments_mut();
            *slot = patch.apply(slot.clone())?;

            let patch = self
                .builder_pending_withdrawals
                .ok_or(Error::PatchPhaseMismatch)?;
            let slot = state.builder_pending_withdrawals_mut();
            *slot = patch.apply(slot.clone())?;

            let patch = self.latest_block_hash.ok_or(Error::PatchPhaseMismatch)?;
            let slot = state.latest_block_hash_mut();
            *slot = patch.apply(slot.clone())?;

            let patch = self
                .payload_expected_withdrawals
                .ok_or(Error::PatchPhaseMismatch)?;
            let slot = state.payload_expected_withdrawals_mut();
            *slot = patch.apply(slot.clone())?;

            let patch = self.builders.ok_or(Error::PatchPhaseMismatch)?;
            let slot = state.builders_mut();
            *slot = patch.apply(slot.clone())?;

            let patch = self
                .next_withdrawal_builder_index
                .ok_or(Error::PatchPhaseMismatch)?;
            let slot = state.next_withdrawal_builder_index_mut();
            *slot = patch.apply(slot.clone())?;

            let patch = self.ptc_window.ok_or(Error::PatchPhaseMismatch)?;
            let slot = state.ptc_window_mut();
            *slot = patch.apply(slot.clone())?;
        }

        Ok(base)
    }
}

fn post_capella_mut<P: Preset>(
    base: &mut CombinedBeaconState<P>,
) -> Option<&mut dyn PostCapellaBeaconState<P>> {
    match base {
        CombinedBeaconState::Phase0(_)
        | CombinedBeaconState::Altair(_)
        | CombinedBeaconState::Bellatrix(_) => None,
        CombinedBeaconState::Capella(state) => Some(state),
        CombinedBeaconState::Deneb(state) => Some(state),
        CombinedBeaconState::Electra(state) => Some(state),
        CombinedBeaconState::Fulu(state) => Some(state),
        CombinedBeaconState::Gloas(state) => Some(state),
    }
}

fn post_fulu_mut<P: Preset>(
    base: &mut CombinedBeaconState<P>,
) -> Option<&mut dyn PostFuluBeaconState<P>> {
    match base {
        CombinedBeaconState::Fulu(state) => Some(state),
        CombinedBeaconState::Gloas(state) => Some(state),
        _ => None,
    }
}

fn post_gloas_mut<P: Preset>(
    base: &mut CombinedBeaconState<P>,
) -> Option<&mut dyn PostGloasBeaconState<P>> {
    match base {
        CombinedBeaconState::Gloas(state) => Some(state),
        _ => None,
    }
}

impl<P: Preset> SszSize for BeaconStatePatch<P> {
    const SIZE: Size = Size::Variable { minimum_size: 1 };
}

impl<P: Preset> SszWrite for BeaconStatePatch<P> {
    fn write_variable(&self, bytes: &mut Vec<u8>) -> Result<(), WriteError> {
        match self {
            Self::V1(patch) => {
                bytes.push(1);
                patch.write_variable(bytes)
            }
        }
    }
}

impl<P: Preset, C> SszRead<C> for BeaconStatePatch<P> {
    fn from_ssz_unchecked(_: &C, bytes: &[u8]) -> Result<Self, ReadError> {
        let (version, payload) = bytes.split_first().ok_or(ReadError::Custom {
            message: "beacon state patch is empty",
        })?;

        match version {
            1 => Ok(Self::V1(SszReadDefault::from_ssz_default(payload)?)),
            _ => Err(ReadError::Custom {
                message: "unsupported beacon state patch version",
            }),
        }
    }
}

// A self-describing union over the three concrete `latest_execution_payload_header` types so the
// patch can store a single field instead of one per fork. The SSZ encoding is a 1-byte selector
// followed by the variant payload — internal to the patch format, not the consensus spec.
#[derive(Clone, Debug)]
enum CombinedExecutionPayloadHeader<P: Preset> {
    Bellatrix(BellatrixExecutionPayloadHeader<P>),
    Capella(CapellaExecutionPayloadHeader<P>),
    Deneb(DenebExecutionPayloadHeader<P>),
}

impl<P: Preset> SszSize for CombinedExecutionPayloadHeader<P> {
    const SIZE: Size = Size::Variable { minimum_size: 1 };
}

impl<P: Preset> SszWrite for CombinedExecutionPayloadHeader<P> {
    fn write_variable(&self, bytes: &mut Vec<u8>) -> Result<(), WriteError> {
        match self {
            Self::Bellatrix(header) => {
                bytes.push(1);
                header.write_variable(bytes)
            }
            Self::Capella(header) => {
                bytes.push(2);
                header.write_variable(bytes)
            }
            Self::Deneb(header) => {
                bytes.push(3);
                header.write_variable(bytes)
            }
        }
    }
}

impl<P: Preset, C> SszRead<C> for CombinedExecutionPayloadHeader<P> {
    fn from_ssz_unchecked(_: &C, bytes: &[u8]) -> Result<Self, ReadError> {
        let (selector, payload) = bytes.split_first().ok_or(ReadError::Custom {
            message: "execution payload header union is empty",
        })?;

        match selector {
            1 => Ok(Self::Bellatrix(SszReadDefault::from_ssz_default(payload)?)),
            2 => Ok(Self::Capella(SszReadDefault::from_ssz_default(payload)?)),
            3 => Ok(Self::Deneb(SszReadDefault::from_ssz_default(payload)?)),
            _ => Err(ReadError::Custom {
                message: "invalid execution payload header union selector",
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ssz::Hc;
    use std_ext::ArcExt as _;
    use types::{
        combined::BeaconState as CombinedBeaconState,
        phase0::beacon_state::BeaconState as Phase0BeaconState, preset::Minimal,
        traits::BeaconState as _,
    };

    use crate::{BeaconStatePatch, Patch as _};

    fn phase0_state(slot: u64) -> Arc<CombinedBeaconState<Minimal>> {
        let mut inner = Phase0BeaconState::<Minimal>::default();
        inner.slot = slot;
        Arc::new(CombinedBeaconState::Phase0(Hc::from(inner)))
    }

    // A referential frame is kept in memory and can be returned directly from the API. Querying it
    // populates its (slot/epoch-relative) caches in place via `OnceCell` interior mutability. When a
    // diff is later applied to that same frame to reconstruct a state at a different slot/epoch,
    // `apply` must not carry those now-stale caches onto the relocated state.
    #[test]
    fn apply_does_not_carry_stale_cache() {
        let base = phase0_state(0);
        // Next epoch on Minimal (8 slots per epoch).
        let changed = phase0_state(8);

        let patch = BeaconStatePatch::diff(&base, &changed).expect("diff should succeed");

        // Simulate the frame's cache being populated while it sat at slot 0.
        let mut frame = base.clone_arc();
        frame
            .make_mut()
            .cache_mut()
            .proposer_index
            .set(99_999)
            .expect("cache should start empty");

        let applied = patch.apply(frame).expect("apply should succeed");

        assert_eq!(applied.slot(), 8);
        assert_eq!(
            applied.cache().proposer_index.get(),
            None,
            "apply carried the base frame's stale cache onto a state at a different slot",
        );
    }
}
