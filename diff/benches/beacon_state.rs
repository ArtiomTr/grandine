#![expect(
    unused_crate_dependencies,
    reason = "The `unused_crate_dependencies` lint checks every crate in a package separately. \
              See <https://github.com/rust-lang/rust/issues/57274>."
)]

mod common;

use core::hint::black_box;

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use diff::{BeaconStatePatch, Patch, PatchConfig};
use std_ext::ArcExt as _;

use crate::common::{PAIRS, state};

fn diff_apply_speed(c: &mut Criterion) {
    let mut group = c.benchmark_group("apply");
    for (name, base, changed) in PAIRS {
        let base = state(base);
        let changed = state(changed);

        let patch = BeaconStatePatch::diff(PatchConfig::default(), &base, &changed)
            .unwrap_or_else(|_| panic!("failed to produce patch for `{name}` case"));

        let mut received = base.clone_arc();

        patch
            .clone()
            .apply(&mut received)
            .unwrap_or_else(|_| panic!("patch, produced for `{name}` case, won't apply"));

        assert_eq!(changed, received);

        group.bench_function(name, |b| {
            b.iter_batched(
                || (patch.clone(), base.clone_arc()),
                |(patch, mut base)| black_box(patch.apply(&mut base)),
                // patch takes quite some space in memory, so we can't clone it
                // million times - but that doesn't matter, as 750 picoseconds
                // is absolutely insignificant for our case.
                BatchSize::LargeInput,
            );
        });
    }
    group.finish();

    let mut group = c.benchmark_group("diff");
    for (name, base, changed) in PAIRS {
        let base = state(base);
        let changed = state(changed);

        group.bench_function(name, |b| {
            b.iter(|| {
                black_box(BeaconStatePatch::diff(
                    PatchConfig::default(),
                    black_box(&base),
                    black_box(&changed),
                ))
            });
        });
    }
    group.finish();
}

criterion_group!(benches, diff_apply_speed);
criterion_main!(benches);
