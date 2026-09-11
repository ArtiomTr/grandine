// Dev-dependencies of the `comparison` and `beacon_state` benchmarks, which are separate targets.
#[cfg(test)]
use {
    bytesize as _, criterion as _, eth_state_diff as _, fs_err as _, qbsdiff as _, reqwest as _,
    rkyv as _, std_ext as _, tabled as _, xdelta3 as _,
};

/// Enters a span named `$name` for the rest of the enclosing block, when the `tracing` feature is
/// on. The patch fields are applied one after another, so a span per field is what tells a harness
/// which of them a delta's time went to.
macro_rules! field_span {
    ($name: literal) => {
        #[cfg(feature = "tracing")]
        let _span = tracing::debug_span!($name).entered();
    };
}

pub(crate) use field_span;

/// Runs two closures, potentially at the same time.
///
/// Diffing is a read-only walk of two states, so the fields of a patch are independent and the
/// registry-sized ones are what a diff spends nearly all of its time on.
#[cfg(not(target_os = "zkvm"))]
pub(crate) fn join<A: Send, B: Send>(
    a: impl FnOnce() -> A + Send,
    b: impl FnOnce() -> B + Send,
) -> (A, B) {
    rayon::join(a, b)
}

#[cfg(target_os = "zkvm")]
pub(crate) fn join<A, B>(a: impl FnOnce() -> A, b: impl FnOnce() -> B) -> (A, B) {
    (a(), b())
}

mod beacon_state;
mod compress;
mod error;
mod list;
mod patch;
mod replace;

pub use crate::{
    beacon_state::BeaconStatePatch,
    error::Error,
    patch::{Patch, PatchConfig},
};
