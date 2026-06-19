#![cfg_attr(
    test,
    expect(
        unused_crate_dependencies,
        reason = "The `unused_crate_dependencies` lint checks every crate in a package separately. \
                  See <https://github.com/rust-lang/rust/issues/57274>."
    )
)]

mod beacon_state;
mod diff;
mod list;
mod replace;
mod vector;

pub use beacon_state::BeaconStatePatch;
pub use diff::{Error, Patch, PatchResult};
pub use list::{ListBalancesPatch, ListPositionalPatch, ListQueuePatch, ValidatorListPatch};
pub use vector::VectorPatch;

pub(crate) use replace::ReplacePatch;
