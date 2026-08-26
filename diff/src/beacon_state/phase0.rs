use ssz::Ssz;
use types::{
    phase0::{beacon_state::BeaconState as Phase0BeaconState, containers::PendingAttestation},
    preset::Preset,
};

use crate::{
    beacon_state::shared::SharedPatch,
    compress::Compressed,
    error::Error,
    list::PositionalPatch,
    patch::{Patch, PatchConfig},
};

#[derive(Debug, Clone, Ssz)]
#[ssz(derive_hash = false)]
pub struct Phase0StatePatchV1<P: Preset> {
    shared: SharedPatch<P>,

    previous_epoch_attestations: Compressed<PositionalPatch<PendingAttestation<P>>>,
    current_epoch_attestations: Compressed<PositionalPatch<PendingAttestation<P>>>,
}

impl<P: Preset> Patch<Phase0BeaconState<P>> for Phase0StatePatchV1<P> {
    fn diff(
        config: PatchConfig,
        base: &Phase0BeaconState<P>,
        changed: &Phase0BeaconState<P>,
    ) -> Result<Self, Error> {
        Ok(Self {
            shared: Patch::diff(config, base, changed)?,
            previous_epoch_attestations: Patch::diff(
                config,
                &base.previous_epoch_attestations,
                &changed.previous_epoch_attestations,
            )?,
            current_epoch_attestations: Patch::diff(
                config,
                &base.current_epoch_attestations,
                &changed.current_epoch_attestations,
            )?,
        })
    }

    fn apply(self, base: &mut Phase0BeaconState<P>) -> Result<(), Error> {
        self.shared.apply(base)?;
        self.previous_epoch_attestations
            .apply(&mut base.previous_epoch_attestations)?;
        self.current_epoch_attestations
            .apply(&mut base.current_epoch_attestations)
    }
}
