//! Standalone micro-benchmark for the validator-list diffing rewrite.
//!
//! Isolates the cost of each `apply` phase (effective-balance edits, partial-field
//! edits, appends) for the column-wise [`ValidatorListPatch`], plus a combined
//! "epoch" scenario, and compares against the previous
//! `ListPositionalPatch<Validator>` over a `PersistentList<Validator>`.
//!
//! Self-contained (no beacon-node download required).
//!
//! Run with: `cargo run --release -p diff --example bench_validators`

use std::{hint::black_box, time::Instant};

use bls::PublicKeyBytes;
use diff::{ListPositionalPatch, Patch, ValidatorListPatch};
use ssz::{H256, PersistentList, SszHash as _, SszWrite};
use try_from_iterator::TryFromIterator as _;
use types::{
    phase0::{containers::Validator, validator_list::ValidatorList},
    preset::{Mainnet, Preset},
};

type N = <Mainnet as Preset>::ValidatorRegistryLimit;

const COUNT: usize = 1_000_000;
const BALANCE_CHANGES: usize = 10_000; // ~1% effective-balance churn per epoch
const PARTIAL_CHANGES: usize = 200; // exits / slashings per epoch
const APPENDED: usize = 1_000; // activations per epoch
const ITERS: u32 = 50;

fn make_validator(index: usize) -> Validator {
    let seed = index as u8;

    Validator {
        pubkey: PublicKeyBytes::repeat_byte(seed),
        withdrawal_credentials: H256::repeat_byte(seed),
        effective_balance: 32_000_000_000,
        slashed: false,
        activation_eligibility_epoch: 0,
        activation_epoch: 0,
        exit_epoch: u64::MAX,
        withdrawable_epoch: u64::MAX,
    }
}

fn time<T>(mut body: impl FnMut() -> T) -> std::time::Duration {
    let start = Instant::now();
    for _ in 0..ITERS {
        black_box(body());
    }
    start.elapsed() / ITERS
}

fn bench_new(label: &str, base: &ValidatorList<N>, changed: &ValidatorList<N>) {
    let patch = ValidatorListPatch::<N>::diff(base, changed).expect("diff");
    let applied = patch.clone().apply(base.clone()).expect("apply");
    assert!(applied == *changed, "{label}: round-trip mismatch");
    assert_eq!(
        applied.hash_tree_root(),
        changed.hash_tree_root(),
        "{label}: hash_tree_root mismatch (batched cache invalidation bug)"
    );

    let size = patch.to_ssz().expect("serialize").len();
    let diff_time = time(|| ValidatorListPatch::<N>::diff(base, changed).expect("diff"));
    let apply_time = time(|| patch.clone().apply(base.clone()).expect("apply"));

    println!("  new {label:24}  diff {diff_time:>10.2?}  apply {apply_time:>10.2?}  patch {size:>10} B");
}

fn bench_old(
    label: &str,
    base: &PersistentList<Validator, N>,
    changed: &PersistentList<Validator, N>,
) {
    let patch = ListPositionalPatch::<Validator, N>::diff(base, changed).expect("diff");
    let applied = patch.clone().apply(base.clone()).expect("apply");
    assert!(applied == *changed, "{label}: round-trip mismatch");

    let size = patch.to_ssz().expect("serialize").len();
    let diff_time = time(|| ListPositionalPatch::<Validator, N>::diff(base, changed).expect("diff"));
    let apply_time = time(|| patch.clone().apply(base.clone()).expect("apply"));

    println!("  old {label:24}  diff {diff_time:>10.2?}  apply {apply_time:>10.2?}  patch {size:>10} B");
}

fn main() {
    println!(
        "registry: {COUNT} validators; per-epoch churn: {BALANCE_CHANGES} balances / \
         {PARTIAL_CHANGES} partial / {APPENDED} appended ({ITERS} iters)\n"
    );

    let base_vec: Vec<Validator> = (0..COUNT).map(make_validator).collect();

    // Scenario builders over a fresh clone of the base registry.
    let balance_only = {
        let mut v = base_vec.clone();
        for k in 0..BALANCE_CHANGES {
            v[k * (COUNT / BALANCE_CHANGES)].effective_balance -= 1_000_000_000;
        }
        v
    };
    let partial_only = {
        let mut v = base_vec.clone();
        for k in 0..PARTIAL_CHANGES {
            v[k * (COUNT / PARTIAL_CHANGES)].exit_epoch = 12_345;
        }
        v
    };
    let append_only = {
        let mut v = base_vec.clone();
        v.extend((0..APPENDED).map(|j| make_validator(COUNT + j)));
        v
    };
    let epoch = {
        let mut v = base_vec.clone();
        for k in 0..BALANCE_CHANGES {
            v[k * (COUNT / BALANCE_CHANGES)].effective_balance -= 1_000_000_000;
        }
        for k in 0..PARTIAL_CHANGES {
            v[k * (COUNT / PARTIAL_CHANGES)].exit_epoch = 12_345;
        }
        v.extend((0..APPENDED).map(|j| make_validator(COUNT + j)));
        v
    };

    let base_new = ValidatorList::<N>::try_from_iter(base_vec.iter().cloned()).expect("base");
    let to_new = |v: &[Validator]| ValidatorList::<N>::try_from_iter(v.iter().cloned()).expect("v");

    let base_old = PersistentList::<Validator, N>::try_from_iter(base_vec.iter().cloned()).expect("base");
    let to_old = |v: &[Validator]| {
        PersistentList::<Validator, N>::try_from_iter(v.iter().cloned()).expect("v")
    };

    println!("per-phase breakdown (new ValidatorListPatch):");
    bench_new("balances only", &base_new, &to_new(&balance_only));
    bench_new("partial only", &base_new, &to_new(&partial_only));
    bench_new("appends only", &base_new, &to_new(&append_only));

    println!("\nfull epoch (new vs old):");
    bench_new("epoch", &base_new, &to_new(&epoch));
    bench_old("epoch", &base_old, &to_old(&epoch));
}
