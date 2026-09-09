use core::ops::Range;

use types::{
    nonstandard::PartialValidator,
    phase0::primitives::{Gwei, ValidatorIndex},
    traits::SszValidatorList,
};

#[macro_export]
macro_rules! par_iter {
    ($collection: expr) => {{
        #[cfg(target_os = "zkvm")]
        {
            $collection.iter()
        }

        #[cfg(not(target_os = "zkvm"))]
        {
            use rayon::iter::IntoParallelRefIterator as _;
            $collection.par_iter()
        }
    }};
}

#[macro_export]
macro_rules! into_par_iter {
    ($collection: expr) => {{
        #[cfg(target_os = "zkvm")]
        {
            $collection.into_iter()
        }

        #[cfg(not(target_os = "zkvm"))]
        {
            use rayon::iter::IntoParallelIterator as _;
            $collection.into_par_iter()
        }
    }};
}

#[cfg_attr(feature = "tracing", tracing::instrument(level = "debug", skip_all))]
pub fn join<T1: Send, T2: Send, F1: FnOnce() -> T1 + Send, F2: FnOnce() -> T2 + Send>(
    f1: F1,
    f2: F2,
) -> (T1, T2) {
    #[cfg(all(feature = "tracing", not(target_os = "zkvm")))]
    {
        let span = tracing::debug_span!("helper_functions::join");

        return rayon::join(
            || {
                let _entered = span.enter();
                f1()
            },
            || {
                let _entered = span.enter();
                f2()
            },
        );
    }

    #[cfg(all(not(feature = "tracing"), not(target_os = "zkvm")))]
    return rayon::join(f1, f2);

    #[cfg(target_os = "zkvm")]
    return (f1(), f2());
}

/// Maps the validator registry into a `Vec`, in parallel where Rayon is available.
///
/// Epoch processing walks the whole registry several times. On Mainnet that is millions of
/// elements per walk, and those walks are the single largest cost of reconstructing an archived
/// state, so they are worth spreading across cores.
///
/// The registry is taken as a whole rather than as an iterator because `SszValidatorList` is used
/// through `dyn`, which rules out handing back an `impl ParallelIterator`.
#[cfg(not(target_os = "zkvm"))]
#[cfg_attr(feature = "tracing", tracing::instrument(level = "debug", skip_all))]
pub fn map_registry<T, F>(validators: &dyn SszValidatorList, f: F) -> Vec<T>
where
    T: Send,
    F: Fn(&PartialValidator, Gwei) -> T + Send + Sync,
{
    use rayon::iter::{
        IndexedParallelIterator as _, IntoParallelRefIterator as _, ParallelIterator as _,
    };

    let mut mapped = Vec::new();

    validators
        .partial_validator_column()
        .par_iter()
        .zip(validators.effective_balance_column().par_iter())
        .map(|(validator, effective_balance)| f(validator, *effective_balance))
        .collect_into_vec(&mut mapped);

    mapped
}

#[cfg(target_os = "zkvm")]
pub fn map_registry<T, F>(validators: &dyn SszValidatorList, f: F) -> Vec<T>
where
    F: Fn(&PartialValidator, Gwei) -> T,
{
    itertools::zip_eq(
        validators.partial_validators(),
        validators.effective_balances().copied(),
    )
    .map(|(validator, effective_balance)| f(validator, effective_balance))
    .collect()
}

/// Maps a range of indices into a `Vec`, in parallel where Rayon is available.
///
/// Fails with the first error any index produced; which one that is when several fail is not
/// specified, the same as it is not for a sequential `collect` into a `Result`.
#[cfg(not(target_os = "zkvm"))]
pub fn try_map_range<T, E, F>(range: Range<u64>, f: F) -> Result<Vec<T>, E>
where
    T: Send,
    E: Send,
    F: Fn(u64) -> Result<T, E> + Send + Sync,
{
    use rayon::iter::{IntoParallelIterator as _, ParallelIterator as _};

    range.into_par_iter().map(f).collect()
}

#[cfg(target_os = "zkvm")]
pub fn try_map_range<T, E, F>(range: Range<u64>, f: F) -> Result<Vec<T>, E>
where
    F: Fn(u64) -> Result<T, E>,
{
    range.map(f).collect()
}

/// The indices of the registry entries a predicate selects, in ascending order, computed in
/// parallel where Rayon is available.
///
/// This is the shape of every "which validators are active" walk, and those run often enough - and
/// over a registry large enough - to be worth spreading across cores.
#[cfg(not(target_os = "zkvm"))]
#[cfg_attr(feature = "tracing", tracing::instrument(level = "debug", skip_all))]
pub fn filter_registry_indices<F>(validators: &dyn SszValidatorList, keep: F) -> Vec<ValidatorIndex>
where
    F: Fn(&PartialValidator) -> bool + Send + Sync,
{
    use rayon::iter::{
        IndexedParallelIterator as _, IntoParallelRefIterator as _, ParallelIterator as _,
    };

    validators
        .partial_validator_column()
        .par_iter()
        .enumerate()
        .filter(|(_, validator)| keep(validator))
        .map(|(index, _)| ValidatorIndex::try_from(index).expect("index fits in u64"))
        .collect()
}

#[cfg(target_os = "zkvm")]
pub fn filter_registry_indices<F>(validators: &dyn SszValidatorList, keep: F) -> Vec<ValidatorIndex>
where
    F: Fn(&PartialValidator) -> bool,
{
    validators
        .partial_validators()
        .enumerate()
        .filter(|(_, validator)| keep(validator))
        .map(|(index, _)| ValidatorIndex::try_from(index).expect("index fits in u64"))
        .collect()
}

/// Maps three equally long slices into a `Vec`, in parallel where Rayon is available.
///
/// Fails if any element does; which error comes back when several fail is unspecified, the same as
/// it is for a sequential `collect` into a `Result`.
#[cfg(not(target_os = "zkvm"))]
#[cfg_attr(feature = "tracing", tracing::instrument(level = "debug", skip_all))]
pub fn try_map_zipped<A, B, C, T, E, F>(a: &[A], b: &[B], c: &[C], f: F) -> Result<Vec<T>, E>
where
    A: Sync,
    B: Sync,
    C: Sync,
    T: Send,
    E: Send,
    F: Fn(&A, &B, &C) -> Result<T, E> + Send + Sync,
{
    use rayon::iter::{
        IndexedParallelIterator as _, IntoParallelRefIterator as _, ParallelIterator as _,
    };

    let mut mapped = Vec::new();

    a.par_iter()
        .zip(b.par_iter())
        .zip(c.par_iter())
        .map(|((a, b), c)| f(a, b, c))
        .collect_into_vec(&mut mapped);

    mapped.into_iter().collect()
}

#[cfg(target_os = "zkvm")]
pub fn try_map_zipped<A, B, C, T, E, F>(a: &[A], b: &[B], c: &[C], f: F) -> Result<Vec<T>, E>
where
    F: Fn(&A, &B, &C) -> Result<T, E>,
{
    itertools::izip!(a, b, c)
        .map(|(a, b, c)| f(a, b, c))
        .collect()
}
