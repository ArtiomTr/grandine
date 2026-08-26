use core::marker::PhantomData;

use ssz::Ssz;
use types::{
    deneb::containers::ExecutionPayloadHeader as DenebExecutionPayloadHeader,
    electra::{
        beacon_state::BeaconState as ElectraBeaconState,
        containers::{PendingConsolidation, PendingDeposit, PendingPartialWithdrawal},
    },
    phase0::primitives::{DepositIndex, Epoch, Gwei},
    preset::Preset,
    traits::PostElectraBeaconState,
};

use crate::{
    beacon_state::{altair::AltairPatch, capella::CapellaPatch, shared::SharedPatch},
    compress::Compressed,
    error::Error,
    list::{PositionalPatch, QueuePatch},
    patch::{Patch, PatchConfig},
    replace::ReplacePatch,
};

#[derive(Debug, Clone, Ssz)]
#[ssz(derive_hash = false)]
pub struct ElectraStatePatchV1<P: Preset> {
    shared: SharedPatch<P>,
    altair: AltairPatch<P>,
    capella: CapellaPatch<P>,
    electra: ElectraPatch<P>,

    latest_execution_payload_header: ReplacePatch<DenebExecutionPayloadHeader<P>>,
}

impl<P: Preset> Patch<ElectraBeaconState<P>> for ElectraStatePatchV1<P> {
    fn diff(
        config: PatchConfig,
        base: &ElectraBeaconState<P>,
        changed: &ElectraBeaconState<P>,
    ) -> Result<Self, Error> {
        Ok(Self {
            shared: Patch::diff(config, base, changed)?,
            altair: Patch::diff(config, base, changed)?,
            capella: Patch::diff(config, base, changed)?,
            electra: Patch::diff(config, base, changed)?,

            latest_execution_payload_header: Patch::diff(
                config,
                &base.latest_execution_payload_header,
                &changed.latest_execution_payload_header,
            )?,
        })
    }

    fn apply(self, base: &mut ElectraBeaconState<P>) -> Result<(), Error> {
        self.shared.apply(base)?;
        self.altair.apply(base)?;
        self.capella.apply(base)?;
        self.electra.apply(base)?;

        self.latest_execution_payload_header
            .apply(&mut base.latest_execution_payload_header)
    }
}

#[derive(Debug, Clone, Ssz)]
#[ssz(derive_hash = false)]
pub struct ElectraPatch<P: Preset> {
    deposit_requests_start_index: ReplacePatch<DepositIndex>,
    deposit_balance_to_consume: ReplacePatch<Gwei>,
    exit_balance_to_consume: ReplacePatch<Gwei>,
    earliest_exit_epoch: ReplacePatch<Epoch>,
    consolidation_balance_to_consume: ReplacePatch<Gwei>,
    earliest_consolidation_epoch: ReplacePatch<Epoch>,
    pending_deposits: Compressed<QueuePatch<PendingDeposit>>,
    pending_partial_withdrawals: Compressed<QueuePatch<PendingPartialWithdrawal>>,
    // TODO(delta-db): try replacing this with QueuePatch, and measuring performance.
    pending_consolidations: Compressed<PositionalPatch<PendingConsolidation>>,

    #[ssz(skip)]
    phantom: PhantomData<P>,
}

impl<P: Preset, S: PostElectraBeaconState<P>> Patch<S> for ElectraPatch<P> {
    fn diff(config: PatchConfig, base: &S, changed: &S) -> Result<Self, Error> {
        Ok(Self {
            deposit_requests_start_index: Patch::diff(
                config,
                &base.deposit_requests_start_index(),
                &changed.deposit_requests_start_index(),
            )?,
            deposit_balance_to_consume: Patch::diff(
                config,
                &base.deposit_balance_to_consume(),
                &changed.deposit_balance_to_consume(),
            )?,
            exit_balance_to_consume: Patch::diff(
                config,
                &base.exit_balance_to_consume(),
                &changed.exit_balance_to_consume(),
            )?,
            earliest_exit_epoch: Patch::diff(
                config,
                &base.earliest_exit_epoch(),
                &changed.earliest_exit_epoch(),
            )?,
            consolidation_balance_to_consume: Patch::diff(
                config,
                &base.consolidation_balance_to_consume(),
                &changed.consolidation_balance_to_consume(),
            )?,
            earliest_consolidation_epoch: Patch::diff(
                config,
                &base.earliest_consolidation_epoch(),
                &changed.earliest_consolidation_epoch(),
            )?,
            pending_deposits: Patch::diff(
                config,
                base.pending_deposits(),
                changed.pending_deposits(),
            )?,
            pending_partial_withdrawals: Patch::diff(
                config,
                base.pending_partial_withdrawals(),
                changed.pending_partial_withdrawals(),
            )?,
            pending_consolidations: Patch::diff(
                config,
                base.pending_consolidations(),
                changed.pending_consolidations(),
            )?,
            phantom: PhantomData,
        })
    }

    fn apply(self, base: &mut S) -> Result<(), Error> {
        self.deposit_requests_start_index
            .apply(base.deposit_requests_start_index_mut())?;
        self.deposit_balance_to_consume
            .apply(base.deposit_balance_to_consume_mut())?;
        self.exit_balance_to_consume
            .apply(base.exit_balance_to_consume_mut())?;
        self.earliest_exit_epoch
            .apply(base.earliest_exit_epoch_mut())?;
        self.consolidation_balance_to_consume
            .apply(base.consolidation_balance_to_consume_mut())?;
        self.earliest_consolidation_epoch
            .apply(base.earliest_consolidation_epoch_mut())?;
        self.pending_deposits.apply(base.pending_deposits_mut())?;
        self.pending_partial_withdrawals
            .apply(base.pending_partial_withdrawals_mut())?;
        self.pending_consolidations
            .apply(base.pending_consolidations_mut())
    }
}
