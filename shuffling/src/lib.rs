use core::{
    fmt::Debug,
    num::NonZeroU64,
    ops::{Index as _, Rem as _},
};

use anyhow::Result;
use bit_field::BitArray as _;
use itertools::izip;
use nonzero_ext::nonzero;
use tap::TryConv as _;
use types::{phase0::primitives::H256, preset::Preset};

const BITS_PER_HASH: usize = H256::len_bytes() * 8;

// Originally based on:
// <https://github.com/protolambda/eth2-shuffle/tree/fd840f1036c1f8f6d7625ffe6ff4d9c60f942876>
// See the following for an explanation of the algorithm:
// - <https://github.com/protolambda/eth2-docs/tree/de65f38857f1e27ffb6f25107d61e795cf1a5ad7#shuffling>
// - <https://github.com/protolambda/eth2-impl-design/tree/782b1d2da088e4ebbbea227cfa0a8752399239fb#shuffling>
pub fn shuffle_slice<P: Preset, T: Send>(slice: &mut [T], seed: H256) -> Result<()> {
    let Some(length) = slice.len().try_into().map(NonZeroU64::new)? else {
        return Ok(());
    };

    // The rounds are sequential and cannot be parallelized: each reads the slice the previous one
    // produced. The parallelism is *within* a round (see `swap_main_chunks`). When the slice is
    // large enough to be worth splitting, the whole round loop is driven on a dedicated Rayon thread
    // pool so the per-round fan-out reuses persistent worker threads instead of spawning fresh ones
    // every round. The dedicated pool is also what keeps this safe to call from inside
    // `OnceCell::get_or_init`: its workers only ever run shuffle work, so they cannot re-enter the
    // guarded initializer, and `ThreadPool::install` blocks the caller without work-stealing.
    #[cfg(not(target_os = "zkvm"))]
    if should_parallelize(slice.len()) {
        shuffle_pool().install(|| run_rounds::<P, T>(slice, seed, length, true));
        return Ok(());
    }

    run_rounds::<P, T>(slice, seed, length, false);

    Ok(())
}

fn run_rounds<P: Preset, T: Send>(
    slice: &mut [T],
    seed: H256,
    length: NonZeroU64,
    parallel: bool,
) {
    for round in (0..P::SHUFFLE_ROUND_COUNT).rev() {
        let pivot = compute_pivot(seed, round, length)
            .try_conv::<usize>()
            .expect("remainder of division by number that fits in usize also fits in usize");

        let midpoint = pivot.saturating_add(1);
        let (low, high) = slice.split_at_mut(midpoint);

        // The two halves are disjoint and independent.
        swap_around_mirror(seed, round, low, 0, parallel);
        swap_around_mirror(seed, round, high, midpoint, parallel);
    }
}

fn swap_around_mirror<T: Send>(
    seed: H256,
    round: u8,
    slice: &mut [T],
    offset: usize,
    parallel: bool,
) {
    // `[T]::chunks_exact_mut` and `[T]::rchunks_exact_mut` are needed for full performance.
    // `[T]::as_chunks_mut` and `[T]::as_rchunks_mut` could simplify this when stabilized.

    let mirror = slice.len() / 2;
    let offset_mirror = offset.saturating_add(mirror);
    let offset_length = offset.saturating_add(slice.len());
    let trailing = mirror.min(offset_length % BITS_PER_HASH);
    let leading = mirror.saturating_sub(trailing) % BITS_PER_HASH;

    let (low, mut high) = slice.split_at_mut(mirror);

    if low.len() < high.len() {
        high = &mut high[1..];
    }

    assert_eq!(low.len(), mirror);
    assert_eq!(high.len(), mirror);

    // The trailing and leading edges are partial `BITS_PER_HASH` windows. They are small (under one
    // window each) and touch the boundaries of `low`/`high`, so they are handled serially here,
    // leaving the bulk full-window region in the middle to be split across threads below.
    if trailing > 0 {
        let source = compute_source(seed, round, offset_length / BITS_PER_HASH);
        let bit_indices = (0..offset_length % BITS_PER_HASH).rev();
        let low_elements = low[..trailing].iter_mut();
        let high_elements = high[mirror.saturating_sub(trailing)..].iter_mut().rev();

        swap_using_source(source, bit_indices, low_elements, high_elements);
    }

    if leading > 0 {
        let source = compute_source(seed, round, offset_mirror / BITS_PER_HASH);
        let bit_indices = (0..BITS_PER_HASH).rev();
        let low_elements = low[mirror.saturating_sub(leading)..].iter_mut();
        let high_elements = high[..leading].iter_mut().rev();

        swap_using_source(source, bit_indices, low_elements, high_elements);
    }

    // The full `BITS_PER_HASH` windows between the two edges. Each chunk pair is independent, so
    // this region is what gets parallelized.
    let chunk_count = mirror.saturating_sub(trailing) / BITS_PER_HASH;

    if chunk_count == 0 {
        return;
    }

    let source_base = offset_length / BITS_PER_HASH - 1;
    let low_main = &mut low[trailing..mirror - leading];
    let high_main = &mut high[leading..mirror - trailing];

    debug_assert_eq!(low_main.len(), chunk_count * BITS_PER_HASH);
    debug_assert_eq!(high_main.len(), chunk_count * BITS_PER_HASH);

    dispatch_main(seed, round, low_main, high_main, source_base, chunk_count, parallel);
}

/// Runs the full-window region either sequentially or fanned out across the dedicated pool.
#[cfg(target_os = "zkvm")]
fn dispatch_main<T>(
    seed: H256,
    round: u8,
    low: &mut [T],
    high: &mut [T],
    source_base: usize,
    _chunk_count: usize,
    _parallel: bool,
) {
    swap_chunk_range(seed, round, low, high, source_base);
}

/// Runs the full-window region either sequentially or fanned out across the dedicated pool.
#[cfg(not(target_os = "zkvm"))]
fn dispatch_main<T: Send>(
    seed: H256,
    round: u8,
    low: &mut [T],
    high: &mut [T],
    source_base: usize,
    chunk_count: usize,
    parallel: bool,
) {
    if parallel {
        swap_main_chunks(seed, round, low, high, source_base, chunk_count);
    } else {
        swap_chunk_range(seed, round, low, high, source_base);
    }
}

/// Applies one shuffle round to a contiguous range of mirror chunk pairs.
///
/// `low` and `high` have equal length, a whole multiple of [`BITS_PER_HASH`]. Pairs are
/// `low.chunks_exact(BITS_PER_HASH)` zipped with `high.rchunks_exact(BITS_PER_HASH)`, so the lowest
/// `low` window pairs with the highest `high` window (the swap-or-not mirror). `source_base` is the
/// source-window index of the first pair; pair `i` uses `source_base - i`.
fn swap_chunk_range<T>(seed: H256, round: u8, low: &mut [T], high: &mut [T], source_base: usize) {
    for (index, (low_chunk, high_chunk)) in low
        .chunks_exact_mut(BITS_PER_HASH)
        .zip(high.rchunks_exact_mut(BITS_PER_HASH))
        .enumerate()
    {
        let source = compute_source(seed, round, source_base - index);
        let bit_indices = 0..BITS_PER_HASH;
        let low_elements = low_chunk.iter_mut().rev();
        let high_elements = high_chunk;

        swap_using_source(source, bit_indices, low_elements, high_elements);
    }
}

/// Minimum number of full chunk pairs to give a worker. Below this, dispatch overhead outweighs the
/// parallelism, so the round runs on the calling thread.
#[cfg(not(target_os = "zkvm"))]
const MIN_CHUNKS_PER_WORKER: usize = 8;

/// Splits the full-window region of one shuffle round across the dedicated shuffle thread pool.
///
/// Must be called from within [`shuffle_pool`]'s `install` (which is the case for the whole round
/// loop): the `rayon::scope` below then runs on that dedicated pool rather than the global one. The
/// pool's workers only ever run shuffle work, so — even though shuffling happens inside
/// `OnceCell::get_or_init` — they cannot re-enter the guarded initializer and deadlock.
#[cfg(not(target_os = "zkvm"))]
fn swap_main_chunks<T: Send>(
    seed: H256,
    round: u8,
    low: &mut [T],
    high: &mut [T],
    source_base: usize,
    chunk_count: usize,
) {
    let worker_count = shuffle_worker_count().min(chunk_count / MIN_CHUNKS_PER_WORKER);

    // Too few chunks (or a single core) to bother dispatching: run on the calling thread.
    if worker_count <= 1 {
        swap_chunk_range(seed, round, low, high, source_base);
        return;
    }

    let part_chunks = chunk_count.div_ceil(worker_count);
    let part_len = part_chunks * BITS_PER_HASH;

    // `low` is split low-to-high and `high` high-to-low so that piece `worker` of each holds the
    // same mirrored chunk pairs. All pieces are disjoint, so the tasks never alias.
    let pieces = low
        .chunks_mut(part_len)
        .zip(high.rchunks_mut(part_len))
        .enumerate();

    rayon::scope(|scope| {
        for (worker, (low_part, high_part)) in pieces {
            let base = source_base - worker * part_chunks;
            scope.spawn(move |_| swap_chunk_range(seed, round, low_part, high_part, base));
        }
    });
}

/// Slices shorter than this shuffle at least as fast on a single thread as split across the pool:
/// with so few full windows per round, the per-round dispatch cost cancels out the parallelism.
/// Benchmarks put the crossover around `2 ** 16` elements; below it the serial path wins outright,
/// from `2 ** 16` up parallelism pulls ahead (steeply so by `~100_000`). Mainnet's active set is far
/// larger still — on the order of `2 ** 20`.
#[cfg(not(target_os = "zkvm"))]
const MIN_PARALLEL_LEN: usize = 1 << 16;

/// Whether a slice of this length is worth splitting across worker threads.
#[cfg(not(target_os = "zkvm"))]
fn should_parallelize(length: usize) -> bool {
    shuffle_worker_count() > 1 && length >= MIN_PARALLEL_LEN
}

/// The dedicated thread pool that drives parallel shuffling.
///
/// Built once and reused across every shuffle. Kept separate from Rayon's global pool so its workers
/// never run unrelated tasks: that is what makes it safe to use from inside `OnceCell::get_or_init`
/// (see [`swap_main_chunks`]).
#[cfg(not(target_os = "zkvm"))]
fn shuffle_pool() -> &'static rayon::ThreadPool {
    use std::sync::OnceLock;

    static POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();

    POOL.get_or_init(|| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(shuffle_worker_count())
            .thread_name(|index| format!("shuffle-{index}"))
            .build()
            .expect("shuffle thread pool should build with default settings")
    })
}

#[cfg(not(target_os = "zkvm"))]
fn shuffle_worker_count() -> usize {
    use std::{num::NonZeroUsize, sync::OnceLock, thread::available_parallelism};

    static COUNT: OnceLock<usize> = OnceLock::new();

    *COUNT.get_or_init(|| available_parallelism().map_or(1, NonZeroUsize::get))
}

fn swap_using_source<'slice, T: 'slice>(
    source: H256,
    bit_indices: impl IntoIterator<Item = usize>,
    low: impl IntoIterator<Item = &'slice mut T>,
    high: impl IntoIterator<Item = &'slice mut T>,
) {
    for (bit_index, index, flip) in izip!(bit_indices, low, high) {
        let bit = source.as_bytes().get_bit(bit_index);

        if bit {
            core::mem::swap(index, flip);
        }
    }
}

#[must_use]
pub fn shuffle_single<P: Preset>(mut index: u64, index_count: NonZeroU64, seed: H256) -> u64 {
    assert!(index < index_count.get());

    for round in 0..P::SHUFFLE_ROUND_COUNT {
        let pivot = compute_pivot(seed, round, index_count);
        let flip = pivot
            .saturating_add(index_count.get())
            .saturating_sub(index)
            % index_count;

        let position = index.max(flip);
        let source = compute_source(seed, round, position / nonzero!(BITS_PER_HASH as u64));
        let bit_index = position.to_le_bytes()[0].into();
        let bit = source.as_bytes().get_bit(bit_index);

        if bit {
            index = flip;
        }
    }

    index
}

fn compute_pivot(seed: H256, round: u8, index_count: NonZeroU64) -> u64 {
    hashing::hash_256_8(seed, round)
        .index(..size_of::<u64>())
        .try_into()
        .map(u64::from_le_bytes)
        .expect("slice has the same size as u64")
        .rem(index_count)
}

fn compute_source(
    seed: H256,
    round: u8,
    position_window: impl TryInto<u64, Error = impl Debug>,
) -> H256 {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "Truncate to match the behavior of `compute_shuffled_index` in `consensus-specs`."
    )]
    let position_window = position_window
        .try_into()
        .expect("position_window should fit in u64") as u32;

    hashing::hash_256_8_32(seed, round, position_window)
}

// The edge cases with 0 and 1 elements are covered by `consensus-spec-tests`.
// In fact, there are 30 test cases for each of them.
#[cfg(test)]
mod spec_tests {
    use itertools::Itertools as _;
    use serde::Deserialize;
    use spec_test_utils::Case;
    use test_generator::test_resources;
    use types::preset::{Mainnet, Minimal};

    use super::*;

    #[expect(clippy::struct_field_names)]
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Mapping {
        seed: H256,
        count: u64,
        mapping: Vec<u64>,
    }

    #[test_resources("consensus-spec-tests/tests/mainnet/phase0/shuffling/*/*/*")]
    fn mainnet(case: Case) {
        run_case::<Mainnet>(case);
    }

    #[test_resources("consensus-spec-tests/tests/minimal/phase0/shuffling/*/*/*")]
    fn minimal(case: Case) {
        run_case::<Minimal>(case);
    }

    fn run_case<P: Preset>(case: Case) {
        let Mapping {
            seed,
            count,
            mapping,
        } = case.yaml("mapping");
        let mut actual_mapping = (0..count).collect_vec();

        shuffle_slice::<P, _>(&mut actual_mapping, seed)
            .expect("length of mapping fits in u64 because count is u64");

        assert_eq!(actual_mapping, mapping);
    }

    // The spec cases top out at 9999 elements, which only engages a couple of workers. This case is
    // large enough to split the round across every available worker, exercising the partitioning in
    // `swap_main_chunks` (including the smaller final piece). It validates the parallel result
    // against `shuffle_single`, the independent single-threaded per-index version of the algorithm.
    // Shuffling the identity yields `shuffled[i] == shuffle_single(i)` (the spec's
    // `output[i] = input[compute_shuffled_index(i)]` with `input` the identity).
    #[test]
    fn parallel_shuffle_matches_single_index_oracle() {
        let count: u64 = 100_000;
        let seed = H256::repeat_byte(0xab);
        let index_count = NonZeroU64::new(count).expect("count is non-zero");

        let mut shuffled = (0..count).collect_vec();
        shuffle_slice::<Mainnet, _>(&mut shuffled, seed).expect("count fits in u64");

        for index in 0..count {
            assert_eq!(
                shuffled[usize::try_from(index).expect("index fits in usize")],
                shuffle_single::<Mainnet>(index, index_count, seed),
                "mismatch at position {index}",
            );
        }
    }
}
