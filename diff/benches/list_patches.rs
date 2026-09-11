//! Timings for the whole patch pipeline at mainnet registry scale.
//!
//! Unlike `beacon_state`, this needs no downloaded states: the registry-sized fields are built
//! synthetically with the change densities an epoch-boundary delta has, which is what the patch
//! algorithms are sensitive to. Everything else is left at its default, so a phase reports close
//! to the cost of the four fields that dominate a real delta.

#![expect(
    unused_crate_dependencies,
    reason = "The `unused_crate_dependencies` lint checks every crate in a package separately."
)]

use core::{hint::black_box, time::Duration};
use std::{sync::Arc, time::Instant};

use diff::{BeaconStatePatch, Patch as _, PatchConfig};
use ssz::{H256, SszListMut as _, SszRead as _, SszWrite as _};
use std_ext::ArcExt as _;
use try_from_iterator::TryFromIterator;
use types::{
    altair::primitives::ParticipationFlags, combined::BeaconState as CombinedBeaconState,
    config::Config, fulu::beacon_state::BeaconState as FuluBeaconState,
    phase0::containers::Validator, preset::Mainnet, traits::SszValidatorList as _,
};

const REGISTRY: u64 = 2_360_000;
const APPENDED: u64 = 512;
const RUNS: usize = 5;

type State = Arc<CombinedBeaconState<Mainnet>>;

fn validator(index: u64) -> Validator {
    Validator {
        withdrawal_credentials: H256::from_low_u64_be(index),
        effective_balance: 32_000_000_000,
        activation_eligibility_epoch: index % 1024,
        activation_epoch: index % 1024,
        exit_epoch: u64::MAX,
        withdrawable_epoch: u64::MAX,
        ..Validator::default()
    }
}

fn time<T>(name: &str, mut body: impl FnMut() -> T) {
    let mut best = Duration::MAX;
    let mut total = Duration::ZERO;

    for _ in 0..RUNS {
        let started = Instant::now();
        black_box(body());
        let elapsed = started.elapsed();
        best = best.min(elapsed);
        total = total.saturating_add(elapsed);
    }

    println!(
        "{name:<34} best {:>10.2?}   mean {:>10.2?}",
        best,
        total / u32::try_from(RUNS).expect("run count fits in u32"),
    );
}

fn base_state() -> FuluBeaconState<Mainnet> {
    FuluBeaconState {
        validators: TryFromIterator::try_from_iter((0..REGISTRY).map(validator))
            .expect("registry fits in the list"),
        balances: TryFromIterator::try_from_iter((0..REGISTRY).map(|index| 32_000_000_000 + index))
            .expect("registry fits in the list"),
        previous_epoch_participation: TryFromIterator::try_from_iter(
            (0..REGISTRY).map(|index| ParticipationFlags::try_from(index % 8).unwrap_or_default()),
        )
        .expect("registry fits in the list"),
        current_epoch_participation: TryFromIterator::try_from_iter(
            (0..REGISTRY).map(|index| ParticipationFlags::try_from(index % 8).unwrap_or_default()),
        )
        .expect("registry fits in the list"),
        inactivity_scores: TryFromIterator::try_from_iter((0..REGISTRY).map(|_| 0))
            .expect("registry fits in the list"),
        ..FuluBeaconState::default()
    }
}

fn mutate_validators(changed: &mut FuluBeaconState<Mainnet>) {
    // Effective balances move for the ~2% of validators that crossed a hysteresis bound.
    for index in (0..REGISTRY).step_by(50) {
        *changed
            .validators
            .effective_balance_mut(index)
            .expect("index is within bounds") = 31_000_000_000;
    }

    // Exits, activations and slashings touch a few hundred validators per epoch.
    for index in (0..REGISTRY).step_by(4096) {
        changed
            .validators
            .partial_validator_mut(index)
            .expect("index is within bounds")
            .exit_epoch = 1234;
    }

    // Withdrawal credential changes are rarer still.
    for index in (0..REGISTRY).step_by(65536) {
        changed
            .validators
            .partial_validator_mut(index)
            .expect("index is within bounds")
            .withdrawal_credentials = H256::repeat_byte(0xab);
    }

    for index in REGISTRY..REGISTRY + APPENDED {
        changed
            .validators
            .push(validator(index))
            .expect("list is not full");
    }
}

fn mutate_balances(changed: &mut FuluBeaconState<Mainnet>) {
    // Every balance moves at an epoch boundary.
    changed.balances = TryFromIterator::try_from_iter(
        (0..REGISTRY).map(|index| 32_000_017_000 + index - (index % 7)),
    )
    .expect("registry fits in the list");
}

fn mutate_participation(changed: &mut FuluBeaconState<Mainnet>) {
    // Participation is reset for the previous epoch and rewritten for almost everyone.
    changed.previous_epoch_participation = TryFromIterator::try_from_iter(
        (0..REGISTRY).map(|index| if index % 20 == 0 { 0 } else { 7 }),
    )
    .expect("registry fits in the list");

    changed.current_epoch_participation = TryFromIterator::try_from_iter(
        (0..REGISTRY).map(|index| if index % 13 == 0 { 0 } else { 3 }),
    )
    .expect("registry fits in the list");
}

fn mutate_inactivity_scores(changed: &mut FuluBeaconState<Mainnet>) {
    // Outside a leak only the validators that missed a duty carry a nonzero score.
    changed.inactivity_scores = TryFromIterator::try_from_iter(
        (0..REGISTRY).map(|index| if index % 64 == 0 { index % 15 + 1 } else { 0 }),
    )
    .expect("registry fits in the list");
}

fn case(
    name: &str,
    base: &State,
    base_state: &FuluBeaconState<Mainnet>,
    mutate: fn(&mut FuluBeaconState<Mainnet>),
) {
    let config = Config::mainnet();

    let mut changed = base_state.clone();
    changed.slot = base_state.slot + 32;
    mutate(&mut changed);

    let changed: State = Arc::new(CombinedBeaconState::Fulu(changed.into()));

    let patch = BeaconStatePatch::diff(PatchConfig::default(), base, &changed)
        .expect("patch should represent the change");

    let encoded = patch.to_ssz().expect("patch should serialize");

    println!("--- {name} ({} bytes on the wire)", encoded.len());

    time("  diff", || {
        BeaconStatePatch::diff(PatchConfig::default(), base, &changed)
            .expect("patch should represent the change")
    });

    time("  to_ssz (compress)", || {
        patch.to_ssz().expect("patch should serialize")
    });

    time("  from_ssz (decompress)", || {
        BeaconStatePatch::<Mainnet>::from_ssz(&config, encoded.as_slice())
            .expect("patch should deserialize")
    });

    time("  apply", || {
        let patch = BeaconStatePatch::<Mainnet>::from_ssz(&config, encoded.as_slice())
            .expect("patch should deserialize");

        let mut target = base.clone_arc();

        patch.apply(&mut target).expect("patch should apply");

        target
    });

    println!();
}

fn scan_costs(base: &FuluBeaconState<Mainnet>) {
    use ssz::SszList;

    let a = &base.balances;
    let b = &base.balances;

    time("  boxed SszList::iter zip", || {
        SszList::iter(a)
            .zip(SszList::iter(b))
            .filter(|(x, y)| x != y)
            .count()
    });

    time("  concrete IntoIterator zip", || {
        a.into_iter()
            .zip(b.into_iter())
            .filter(|(x, y)| x != y)
            .count()
    });

    let v = &base.validators;

    time("  im::Vector effective_balances zip", || {
        v.effective_balances()
            .zip(v.effective_balances())
            .filter(|(x, y)| x != y)
            .count()
    });

    time("  im::Vector partial_validators zip", || {
        v.partial_validators()
            .zip(v.partial_validators())
            .filter(|(x, y)| x != y)
            .count()
    });

    time("  validators.clone()", || v.clone());

    time("  clone + 47200 effective_balance_mut", || {
        let mut copy = v.clone();

        for index in (0..REGISTRY).step_by(50) {
            *copy
                .effective_balance_mut(index)
                .expect("index is within bounds") = 1;
        }

        copy
    });

    time("  clone + 47200 partial_validator_mut", || {
        let mut copy = v.clone();

        for index in (0..REGISTRY).step_by(50) {
            copy.partial_validator_mut(index)
                .expect("index is within bounds")
                .exit_epoch = 1;
        }

        copy
    });

    let balance_column = v.effective_balance_column().clone();
    let item_column = v.partial_validator_column().clone();

    time("  clone + 47200 im::Vector<Gwei> index_mut", || {
        let mut copy = balance_column.clone();

        for index in (0..REGISTRY).step_by(50) {
            *copy
                .get_mut(usize::try_from(index).expect("index fits in usize"))
                .expect("index is within bounds") = 1;
        }

        copy
    });

    time("  clone + 47200 im::Vector<Partial> index_mut", || {
        let mut copy = item_column.clone();

        for index in (0..REGISTRY).step_by(50) {
            copy.get_mut(usize::try_from(index).expect("index fits in usize"))
                .expect("index is within bounds")
                .exit_epoch = 1;
        }

        copy
    });

    // The same edits against a uniquely owned registry: `Arc::make_mut` has nothing to clone, so
    // this is the cost of the walk alone, without the copy-on-write the frame cache forces.
    time("  unshared 47200 effective_balance_mut", || {
        let mut copy = v.clone();
        let mut sink = 0;

        for index in (0..REGISTRY).step_by(50) {
            sink += *copy
                .effective_balance_mut(index)
                .expect("index is within bounds");
        }

        for index in (0..REGISTRY).step_by(50) {
            *copy
                .effective_balance_mut(index)
                .expect("index is within bounds") = 1;
        }

        (copy, sink)
    });

    // What a full-density balance delta actually runs into: `PositionSet` picks the bitmap layout
    // and applies it through `SszListMut::update`, which visits every element.
    let balances = &base.balances;

    time("  balances update() no-op", || {
        let mut copy = balances.clone();
        copy.update(&mut |_| ());
        copy
    });

    time("  balances update() +1 everywhere", || {
        let mut copy = balances.clone();
        copy.update(&mut |balance| *balance = balance.saturating_add(1));
        copy
    });

    time("  balances iter_mut() +1 everywhere", || {
        let mut copy = balances.clone();

        for balance in copy.iter_mut() {
            *balance = balance.saturating_add(1);
        }

        copy
    });

    time("  balances rebuild from an iterator", || {
        let mut rebuilt = balances.clone();

        rebuilt
            .try_assign_from_iter(&mut balances.into_iter().map(|balance| balance + 1))
            .expect("registry fits in the list");

        rebuilt
    });

    let scores = &base.inactivity_scores;

    time("  clone + 36875 PersistentList get_mut", || {
        let mut copy = scores.clone();

        for index in (0..REGISTRY).step_by(64) {
            *copy.get_mut(index).expect("index is within bounds") = 1;
        }

        copy
    });

    println!();
}

fn main() {
    println!("registry of {REGISTRY} validators, {RUNS} runs each\n");

    let built_at = Instant::now();
    let base_fulu = base_state();
    println!("built the base state in {:.1?}\n", built_at.elapsed());

    let base: State = Arc::new(CombinedBeaconState::Fulu(base_fulu.clone().into()));

    println!("--- raw scan costs over one registry-sized list");
    scan_costs(&base_fulu);

    case("nothing but the slot", &base, &base_fulu, |_| ());
    case("validators", &base, &base_fulu, mutate_validators);
    case("balances", &base, &base_fulu, mutate_balances);
    case("participation", &base, &base_fulu, mutate_participation);
    case(
        "inactivity scores",
        &base,
        &base_fulu,
        mutate_inactivity_scores,
    );
    case("everything", &base, &base_fulu, |changed| {
        mutate_validators(changed);
        mutate_balances(changed);
        mutate_participation(changed);
        mutate_inactivity_scores(changed);
    });
}
