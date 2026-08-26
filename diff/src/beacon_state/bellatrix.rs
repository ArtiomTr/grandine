use ssz::Ssz;
use types::{
    bellatrix::{
        beacon_state::BeaconState as BellatrixBeaconState,
        containers::ExecutionPayloadHeader as BellatrixExecutionPayloadHeader,
    },
    preset::Preset,
};

use crate::{
    beacon_state::{altair::AltairPatch, shared::SharedPatch},
    error::Error,
    patch::{Patch, PatchConfig},
    replace::ReplacePatch,
};

#[derive(Debug, Clone, Ssz)]
#[ssz(derive_hash = false)]
pub struct BellatrixStatePatchV1<P: Preset> {
    shared: SharedPatch<P>,
    altair: AltairPatch<P>,

    latest_execution_payload_header: ReplacePatch<BellatrixExecutionPayloadHeader<P>>,
}

impl<P: Preset> Patch<BellatrixBeaconState<P>> for BellatrixStatePatchV1<P> {
    fn diff(
        config: PatchConfig,
        base: &BellatrixBeaconState<P>,
        changed: &BellatrixBeaconState<P>,
    ) -> Result<Self, Error> {
        Ok(Self {
            shared: Patch::diff(config, base, changed)?,
            altair: Patch::diff(config, base, changed)?,
            latest_execution_payload_header: Patch::diff(
                config,
                &base.latest_execution_payload_header,
                &changed.latest_execution_payload_header,
            )?,
        })
    }

    fn apply(self, base: &mut BellatrixBeaconState<P>) -> Result<(), Error> {
        self.shared.apply(base)?;
        self.altair.apply(base)?;
        self.latest_execution_payload_header
            .apply(&mut base.latest_execution_payload_header)
    }
}
