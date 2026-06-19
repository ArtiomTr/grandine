//! Benchmarks for diffing `BeaconState` collections.

use std::{
    cell::Cell,
    collections::{HashMap, HashSet},
    fs,
    hint::black_box,
    io::ErrorKind,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context as _, Result};
use bls as _;
use criterion::{BenchmarkId, Criterion};
use diff::{
    BeaconStatePatch, ListBalancesPatch, ListPositionalPatch, ListQueuePatch, Patch,
    ValidatorListPatch, VectorPatch,
};
use qbsdiff::{Bsdiff, Bspatch};
use reqwest::{blocking::Client, header::ACCEPT};
use ssz::{SszHash as _, SszRead, SszWrite};
use try_from_iterator as _;
use typenum as _;
use types::{combined::BeaconState, config::Config, preset::Mainnet, traits::BeaconState as _};
use unsigned_varint as _;

const BEACON_API: &str = "http://testing.mainnet.beacon-api.nimbus.team";
const SLOTS_PER_EPOCH: u64 = 32;
const BASE: u64 = 14_319_872;

const STATE_PAIRS: [(&str, u64, u64); 7] = [
    ("epochs +1", BASE, BASE + SLOTS_PER_EPOCH),
    ("epochs +2", BASE, BASE + 2 * SLOTS_PER_EPOCH),
    ("epochs +31", BASE, BASE + 31 * SLOTS_PER_EPOCH),
    ("epochs +32", BASE, BASE + 32 * SLOTS_PER_EPOCH),
    ("epochs +128", BASE, BASE + 128 * SLOTS_PER_EPOCH),
    ("slot +1", BASE, BASE + 1),
    ("slot +31", BASE, BASE + SLOTS_PER_EPOCH - 1),
];

fn make<T, Q: Patch<T>>(a: &T, b: &T) -> Q {
    Q::diff(a, b).expect("benchmark diff should succeed")
}

fn apply<T: Clone, Q: Patch<T> + Clone>(base: &T, patch: &Q) -> T {
    patch
        .clone()
        .apply(base.clone())
        .expect("benchmark patch should apply")
}

fn compress<T: SszWrite>(value: &T) -> Vec<u8> {
    let bytes = value.to_ssz().expect("benchmark object should serialize");

    zstd::encode_all(bytes.as_slice(), zstd::DEFAULT_COMPRESSION_LEVEL)
        .expect("benchmark object should compress")
}

fn apply_compressed<T: Clone, Q: Patch<T> + SszRead<()>>(base: &T, compressed_patch: &[u8]) -> T {
    let patch_bytes =
        zstd::decode_all(compressed_patch).expect("benchmark patch should decompress");
    let patch = Q::from_ssz(&(), patch_bytes).expect("benchmark patch should deserialize");

    patch
        .apply(base.clone())
        .expect("benchmark patch should apply")
}

fn main() {
    let pairs = match load_pairs() {
        Ok(pairs) => pairs,
        Err(error) => {
            eprintln!("failed to load beacon states: {error:?}");
            eprintln!("skipping beacon_state benchmark");
            return;
        }
    };

    if pairs.is_empty() {
        eprintln!("no beacon states available, skipping beacon_state benchmark");
        return;
    }

    let mut criterion = Criterion::default().configure_from_args();

    benchmark_compressed::<BeaconStatePatch<_>, _>(
        &mut criterion,
        &pairs,
        "beacon_state",
        |a, b| make(a, b),
        |state| {
            state
                .to_ssz()
                .expect("benchmark object should serialize")
                .len()
        },
    );

    benchmark::<VectorPatch<_, _>, _, _>(
        &mut criterion,
        &pairs,
        "block_roots",
        |a, b| {
            make(
                a.post_electra()
                    .expect("benchmark states are post-Electra")
                    .block_roots(),
                b.post_electra()
                    .expect("benchmark states are post-Electra")
                    .block_roots(),
            )
        },
        |a, patch| {
            black_box(apply(
                a.post_electra()
                    .expect("benchmark states are post-Electra")
                    .block_roots(),
                patch,
            ));
        },
        |state| {
            state
                .post_electra()
                .expect("benchmark states are post-Electra")
                .block_roots()
                .to_ssz()
                .expect("benchmark object should serialize")
                .len()
        },
    );
    benchmark::<VectorPatch<_, _>, _, _>(
        &mut criterion,
        &pairs,
        "state_roots",
        |a, b| {
            make(
                a.post_electra()
                    .expect("benchmark states are post-Electra")
                    .state_roots(),
                b.post_electra()
                    .expect("benchmark states are post-Electra")
                    .state_roots(),
            )
        },
        |a, patch| {
            black_box(apply(
                a.post_electra()
                    .expect("benchmark states are post-Electra")
                    .state_roots(),
                patch,
            ));
        },
        |state| {
            state
                .post_electra()
                .expect("benchmark states are post-Electra")
                .state_roots()
                .to_ssz()
                .expect("benchmark object should serialize")
                .len()
        },
    );
    benchmark::<ValidatorListPatch<_>, _, _>(
        &mut criterion,
        &pairs,
        "validators",
        |a, b| {
            make(
                a.post_electra()
                    .expect("benchmark states are post-Electra")
                    .validators(),
                b.post_electra()
                    .expect("benchmark states are post-Electra")
                    .validators(),
            )
        },
        |a, patch| {
            black_box(apply(
                a.post_electra()
                    .expect("benchmark states are post-Electra")
                    .validators(),
                patch,
            ));
        },
        |state| {
            state
                .post_electra()
                .expect("benchmark states are post-Electra")
                .validators()
                .to_ssz()
                .expect("benchmark object should serialize")
                .len()
        },
    );
    benchmark::<ListBalancesPatch<_>, _, _>(
        &mut criterion,
        &pairs,
        "balances",
        |a, b| {
            make(
                a.post_electra()
                    .expect("benchmark states are post-Electra")
                    .balances(),
                b.post_electra()
                    .expect("benchmark states are post-Electra")
                    .balances(),
            )
        },
        |a, patch| {
            black_box(apply(
                a.post_electra()
                    .expect("benchmark states are post-Electra")
                    .balances(),
                patch,
            ));
        },
        |state| {
            state
                .post_electra()
                .expect("benchmark states are post-Electra")
                .balances()
                .to_ssz()
                .expect("benchmark object should serialize")
                .len()
        },
    );
    let balances_bytes = |state: &BeaconState<Mainnet>| {
        state
            .post_electra()
            .expect("benchmark states are post-Electra")
            .balances()
            .to_ssz()
            .expect("benchmark object should serialize")
    };

    let zstd_size = |patch: &[u8]| {
        zstd::encode_all(patch, zstd::DEFAULT_COMPRESSION_LEVEL)
            .map(|v| v.len())
            .unwrap_or_else(|_| patch.len())
    };
    let raw_size = |patch: &[u8]| patch.len();

    benchmark_bytes(
        &mut criterion,
        &pairs,
        "balances_qbsdiff",
        "zstd bytes",
        balances_bytes,
        |source, target| {
            let mut patch = Vec::new();
            Bsdiff::new(source, target)
                .compare(std::io::Cursor::new(&mut patch))
                .expect("qbsdiff diff should succeed");
            patch
        },
        |source, patch| {
            let bspatch = Bspatch::new(patch).expect("qbsdiff patch should parse");
            let mut out = Vec::with_capacity(bspatch.hint_target_size() as usize);
            bspatch
                .apply(source, std::io::Cursor::new(&mut out))
                .expect("qbsdiff apply should succeed");
            out
        },
        zstd_size,
    );

    benchmark_bytes(
        &mut criterion,
        &pairs,
        "balances_xdelta3",
        "zstd bytes",
        balances_bytes,
        |source, target| xdelta3::encode(target, source).expect("xdelta3 encode should succeed"),
        |source, patch| xdelta3::decode(patch, source).expect("xdelta3 decode should succeed"),
        zstd_size,
    );

    benchmark_bytes(
        &mut criterion,
        &pairs,
        "balances_delta_zlib",
        "patch bytes",
        balances_bytes,
        |source, target| {
            let encoded = encode_balance_deltas(source, target);
            let mut encoder = flate2::write::ZlibEncoder::new(
                Vec::with_capacity(encoded.len() / 2),
                flate2::Compression::default(),
            );
            std::io::Write::write_all(&mut encoder, &encoded).expect("zlib write should succeed");
            encoder.finish().expect("zlib finish should succeed")
        },
        |source, patch| {
            let mut decoder = flate2::read::ZlibDecoder::new(patch);
            let mut encoded = Vec::new();
            std::io::Read::read_to_end(&mut decoder, &mut encoded)
                .expect("zlib read should succeed");
            decode_balance_deltas(source, &encoded)
        },
        raw_size,
    );

    benchmark_bytes(
        &mut criterion,
        &pairs,
        "balances_delta_lz4",
        "patch bytes",
        balances_bytes,
        |source, target| {
            lz4_flex::block::compress_prepend_size(&encode_balance_deltas(source, target))
        },
        |source, patch| {
            let encoded = lz4_flex::block::decompress_size_prepended(patch)
                .expect("lz4 decode should succeed");
            decode_balance_deltas(source, &encoded)
        },
        raw_size,
    );

    benchmark_bytes(
        &mut criterion,
        &pairs,
        "balances_xdelta3_zlib",
        "patch bytes",
        balances_bytes,
        |source, target| {
            let encoded = xdelta3::encode(target, source).expect("xdelta3 encode should succeed");
            let mut encoder = flate2::write::ZlibEncoder::new(
                Vec::with_capacity(encoded.len() / 2),
                flate2::Compression::default(),
            );
            std::io::Write::write_all(&mut encoder, &encoded).expect("zlib write should succeed");
            encoder.finish().expect("zlib finish should succeed")
        },
        |source, patch| {
            let mut decoder = flate2::read::ZlibDecoder::new(patch);
            let mut encoded = Vec::new();
            std::io::Read::read_to_end(&mut decoder, &mut encoded)
                .expect("zlib read should succeed");
            xdelta3::decode(&encoded, source).expect("xdelta3 decode should succeed")
        },
        raw_size,
    );

    benchmark_bytes(
        &mut criterion,
        &pairs,
        "balances_xdelta3_lz4",
        "patch bytes",
        balances_bytes,
        |source, target| {
            let encoded = xdelta3::encode(target, source).expect("xdelta3 encode should succeed");
            lz4_flex::block::compress_prepend_size(&encoded)
        },
        |source, patch| {
            let encoded = lz4_flex::block::decompress_size_prepended(patch)
                .expect("lz4 decode should succeed");
            xdelta3::decode(&encoded, source).expect("xdelta3 decode should succeed")
        },
        raw_size,
    );

    benchmark::<VectorPatch<_, _>, _, _>(
        &mut criterion,
        &pairs,
        "randao_mixes",
        |a, b| {
            make(
                a.post_electra()
                    .expect("benchmark states are post-Electra")
                    .randao_mixes(),
                b.post_electra()
                    .expect("benchmark states are post-Electra")
                    .randao_mixes(),
            )
        },
        |a, patch| {
            black_box(apply(
                a.post_electra()
                    .expect("benchmark states are post-Electra")
                    .randao_mixes(),
                patch,
            ));
        },
        |state| {
            state
                .post_electra()
                .expect("benchmark states are post-Electra")
                .randao_mixes()
                .to_ssz()
                .expect("benchmark object should serialize")
                .len()
        },
    );
    benchmark::<VectorPatch<_, _>, _, _>(
        &mut criterion,
        &pairs,
        "slashings",
        |a, b| {
            make(
                a.post_electra()
                    .expect("benchmark states are post-Electra")
                    .slashings(),
                b.post_electra()
                    .expect("benchmark states are post-Electra")
                    .slashings(),
            )
        },
        |a, patch| {
            black_box(apply(
                a.post_electra()
                    .expect("benchmark states are post-Electra")
                    .slashings(),
                patch,
            ));
        },
        |state| {
            state
                .post_electra()
                .expect("benchmark states are post-Electra")
                .slashings()
                .to_ssz()
                .expect("benchmark object should serialize")
                .len()
        },
    );
    benchmark::<ListPositionalPatch<_, _>, _, _>(
        &mut criterion,
        &pairs,
        "eth1_data_votes",
        |a, b| {
            make(
                a.post_electra()
                    .expect("benchmark states are post-Electra")
                    .eth1_data_votes(),
                b.post_electra()
                    .expect("benchmark states are post-Electra")
                    .eth1_data_votes(),
            )
        },
        |a, patch| {
            black_box(apply(
                a.post_electra()
                    .expect("benchmark states are post-Electra")
                    .eth1_data_votes(),
                patch,
            ));
        },
        |state| {
            state
                .post_electra()
                .expect("benchmark states are post-Electra")
                .eth1_data_votes()
                .to_ssz()
                .expect("benchmark object should serialize")
                .len()
        },
    );
    benchmark::<ListPositionalPatch<_, _>, _, _>(
        &mut criterion,
        &pairs,
        "historical_roots",
        |a, b| {
            make(
                a.post_electra()
                    .expect("benchmark states are post-Electra")
                    .historical_roots(),
                b.post_electra()
                    .expect("benchmark states are post-Electra")
                    .historical_roots(),
            )
        },
        |a, patch| {
            black_box(apply(
                a.post_electra()
                    .expect("benchmark states are post-Electra")
                    .historical_roots(),
                patch,
            ));
        },
        |state| {
            state
                .post_electra()
                .expect("benchmark states are post-Electra")
                .historical_roots()
                .to_ssz()
                .expect("benchmark object should serialize")
                .len()
        },
    );
    benchmark::<ListPositionalPatch<_, _>, _, _>(
        &mut criterion,
        &pairs,
        "previous_epoch_participation",
        |a, b| {
            make(
                a.post_electra()
                    .expect("benchmark states are post-Electra")
                    .previous_epoch_participation(),
                b.post_electra()
                    .expect("benchmark states are post-Electra")
                    .previous_epoch_participation(),
            )
        },
        |a, patch| {
            black_box(apply(
                a.post_electra()
                    .expect("benchmark states are post-Electra")
                    .previous_epoch_participation(),
                patch,
            ));
        },
        |state| {
            state
                .post_electra()
                .expect("benchmark states are post-Electra")
                .previous_epoch_participation()
                .to_ssz()
                .expect("benchmark object should serialize")
                .len()
        },
    );
    benchmark::<ListPositionalPatch<_, _>, _, _>(
        &mut criterion,
        &pairs,
        "current_epoch_participation",
        |a, b| {
            make(
                a.post_electra()
                    .expect("benchmark states are post-Electra")
                    .current_epoch_participation(),
                b.post_electra()
                    .expect("benchmark states are post-Electra")
                    .current_epoch_participation(),
            )
        },
        |a, patch| {
            black_box(apply(
                a.post_electra()
                    .expect("benchmark states are post-Electra")
                    .current_epoch_participation(),
                patch,
            ));
        },
        |state| {
            state
                .post_electra()
                .expect("benchmark states are post-Electra")
                .current_epoch_participation()
                .to_ssz()
                .expect("benchmark object should serialize")
                .len()
        },
    );
    benchmark::<ListPositionalPatch<_, _>, _, _>(
        &mut criterion,
        &pairs,
        "inactivity_scores",
        |a, b| {
            make(
                a.post_electra()
                    .expect("benchmark states are post-Electra")
                    .inactivity_scores(),
                b.post_electra()
                    .expect("benchmark states are post-Electra")
                    .inactivity_scores(),
            )
        },
        |a, patch| {
            black_box(apply(
                a.post_electra()
                    .expect("benchmark states are post-Electra")
                    .inactivity_scores(),
                patch,
            ));
        },
        |state| {
            state
                .post_electra()
                .expect("benchmark states are post-Electra")
                .inactivity_scores()
                .to_ssz()
                .expect("benchmark object should serialize")
                .len()
        },
    );
    benchmark::<ListQueuePatch<_, _>, _, _>(
        &mut criterion,
        &pairs,
        "pending_deposits",
        |a, b| {
            make(
                a.post_electra()
                    .expect("benchmark states are post-Electra")
                    .pending_deposits(),
                b.post_electra()
                    .expect("benchmark states are post-Electra")
                    .pending_deposits(),
            )
        },
        |a, patch| {
            black_box(apply(
                a.post_electra()
                    .expect("benchmark states are post-Electra")
                    .pending_deposits(),
                patch,
            ));
        },
        |state| {
            state
                .post_electra()
                .expect("benchmark states are post-Electra")
                .pending_deposits()
                .to_ssz()
                .expect("benchmark object should serialize")
                .len()
        },
    );
    benchmark::<ListQueuePatch<_, _>, _, _>(
        &mut criterion,
        &pairs,
        "pending_partial_withdrawals",
        |a, b| {
            make(
                a.post_electra()
                    .expect("benchmark states are post-Electra")
                    .pending_partial_withdrawals(),
                b.post_electra()
                    .expect("benchmark states are post-Electra")
                    .pending_partial_withdrawals(),
            )
        },
        |a, patch| {
            black_box(apply(
                a.post_electra()
                    .expect("benchmark states are post-Electra")
                    .pending_partial_withdrawals(),
                patch,
            ));
        },
        |state| {
            state
                .post_electra()
                .expect("benchmark states are post-Electra")
                .pending_partial_withdrawals()
                .to_ssz()
                .expect("benchmark object should serialize")
                .len()
        },
    );
    benchmark::<ListPositionalPatch<_, _>, _, _>(
        &mut criterion,
        &pairs,
        "pending_consolidations",
        |a, b| {
            make(
                a.post_electra()
                    .expect("benchmark states are post-Electra")
                    .pending_consolidations(),
                b.post_electra()
                    .expect("benchmark states are post-Electra")
                    .pending_consolidations(),
            )
        },
        |a, patch| {
            black_box(apply(
                a.post_electra()
                    .expect("benchmark states are post-Electra")
                    .pending_consolidations(),
                patch,
            ));
        },
        |state| {
            state
                .post_electra()
                .expect("benchmark states are post-Electra")
                .pending_consolidations()
                .to_ssz()
                .expect("benchmark object should serialize")
                .len()
        },
    );

    if pairs.iter().all(|pair| {
        pair.before.as_ref().post_fulu().is_some() && pair.after.as_ref().post_fulu().is_some()
    }) {
        benchmark::<VectorPatch<_, _>, _, _>(
            &mut criterion,
            &pairs,
            "proposer_lookahead",
            |a, b| {
                make(
                    a.post_fulu()
                        .expect("benchmark states are post-Fulu")
                        .proposer_lookahead(),
                    b.post_fulu()
                        .expect("benchmark states are post-Fulu")
                        .proposer_lookahead(),
                )
            },
            |a, patch| {
                black_box(apply(
                    a.post_fulu()
                        .expect("benchmark states are post-Fulu")
                        .proposer_lookahead(),
                    patch,
                ));
            },
            |state| {
                state
                    .post_fulu()
                    .expect("benchmark states are post-Fulu")
                    .proposer_lookahead()
                    .to_ssz()
                    .expect("benchmark object should serialize")
                    .len()
            },
        );
    }

    if pairs.iter().all(|pair| {
        pair.before.as_ref().post_gloas().is_some() && pair.after.as_ref().post_gloas().is_some()
    }) {
        benchmark::<VectorPatch<_, _>, _, _>(
            &mut criterion,
            &pairs,
            "builder_pending_payments",
            |a, b| {
                make(
                    a.post_gloas()
                        .expect("benchmark states are post-Gloas")
                        .builder_pending_payments(),
                    b.post_gloas()
                        .expect("benchmark states are post-Gloas")
                        .builder_pending_payments(),
                )
            },
            |a, patch| {
                black_box(apply(
                    a.post_gloas()
                        .expect("benchmark states are post-Gloas")
                        .builder_pending_payments(),
                    patch,
                ));
            },
            |state| {
                state
                    .post_gloas()
                    .expect("benchmark states are post-Gloas")
                    .builder_pending_payments()
                    .to_ssz()
                    .expect("benchmark object should serialize")
                    .len()
            },
        );
        benchmark::<VectorPatch<_, _>, _, _>(
            &mut criterion,
            &pairs,
            "ptc_window",
            |a, b| {
                make(
                    a.post_gloas()
                        .expect("benchmark states are post-Gloas")
                        .ptc_window(),
                    b.post_gloas()
                        .expect("benchmark states are post-Gloas")
                        .ptc_window(),
                )
            },
            |a, patch| {
                black_box(apply(
                    a.post_gloas()
                        .expect("benchmark states are post-Gloas")
                        .ptc_window(),
                    patch,
                ));
            },
            |state| {
                state
                    .post_gloas()
                    .expect("benchmark states are post-Gloas")
                    .ptc_window()
                    .to_ssz()
                    .expect("benchmark object should serialize")
                    .len()
            },
        );
    }

    criterion.final_summary();
}

fn benchmark<P, F, G>(
    criterion: &mut Criterion,
    pairs: &[Pair],
    name: &str,
    make_patch: F,
    apply_patch: G,
    object_size: impl Fn(&BeaconState<Mainnet>) -> usize,
) where
    F: Fn(&BeaconState<Mainnet>, &BeaconState<Mainnet>) -> P,
    G: Fn(&BeaconState<Mainnet>, &P),
    P: SszWrite + Clone,
{
    if pairs.is_empty() {
        return;
    }

    let patch_bytes = pairs
        .iter()
        .map(|pair| {
            make_patch(pair.before.as_ref(), pair.after.as_ref())
                .to_ssz()
                .expect("benchmark patch should serialize")
                .len()
        })
        .collect::<Vec<_>>();
    let object_bytes = pairs
        .iter()
        .map(|pair| object_size(&pair.before).max(object_size(&pair.after)))
        .collect::<Vec<_>>();
    let ran = pairs.iter().map(|_| Cell::new(false)).collect::<Vec<_>>();

    let mut group = criterion.benchmark_group(name);

    for (pair, ran) in pairs.iter().zip(&ran) {
        let patch = make_patch(pair.before.as_ref(), pair.after.as_ref());

        group.bench_function(BenchmarkId::new("diff", &pair.label), |bencher| {
            ran.set(true);
            bencher.iter(|| {
                make_patch(
                    black_box(pair.before.as_ref()),
                    black_box(pair.after.as_ref()),
                )
            });
        });

        group.bench_function(BenchmarkId::new("apply", &pair.label), |bencher| {
            bencher.iter(|| apply_patch(black_box(pair.before.as_ref()), black_box(&patch)));
        });
    }

    group.finish();

    print_size_report(
        name,
        &pairs,
        &patch_bytes,
        &object_bytes,
        &ran,
        "patch bytes",
    );
}

fn benchmark_bytes<G, D, A, S>(
    criterion: &mut Criterion,
    pairs: &[Pair],
    name: &str,
    size_label: &str,
    get_bytes: G,
    make_patch: D,
    apply_patch: A,
    report_size: S,
) where
    G: Fn(&BeaconState<Mainnet>) -> Vec<u8>,
    D: Fn(&[u8], &[u8]) -> Vec<u8>,
    A: Fn(&[u8], &[u8]) -> Vec<u8>,
    S: Fn(&[u8]) -> usize,
{
    if pairs.is_empty() {
        return;
    }

    let befores = pairs
        .iter()
        .map(|pair| get_bytes(pair.before.as_ref()))
        .collect::<Vec<_>>();
    let afters = pairs
        .iter()
        .map(|pair| get_bytes(pair.after.as_ref()))
        .collect::<Vec<_>>();
    let patches = befores
        .iter()
        .zip(&afters)
        .map(|(b, a)| make_patch(b, a))
        .collect::<Vec<_>>();
    let reported_bytes = patches
        .iter()
        .map(|patch| report_size(patch))
        .collect::<Vec<_>>();
    let object_bytes = befores
        .iter()
        .zip(&afters)
        .map(|(b, a)| b.len().max(a.len()))
        .collect::<Vec<_>>();

    for ((before, after), patch) in befores.iter().zip(&afters).zip(&patches) {
        let applied = apply_patch(before, patch);
        assert_eq!(applied, *after, "{name} byte diff round-trip mismatch");
    }

    let ran = pairs.iter().map(|_| Cell::new(false)).collect::<Vec<_>>();
    let mut group = criterion.benchmark_group(name);

    for (idx, (pair, ran)) in pairs.iter().zip(&ran).enumerate() {
        group.bench_function(BenchmarkId::new("diff", &pair.label), |bencher| {
            ran.set(true);
            bencher.iter(|| make_patch(black_box(&befores[idx]), black_box(&afters[idx])));
        });

        group.bench_function(BenchmarkId::new("apply", &pair.label), |bencher| {
            bencher.iter(|| apply_patch(black_box(&befores[idx]), black_box(&patches[idx])));
        });
    }

    group.finish();

    print_size_report(
        name,
        pairs,
        &reported_bytes,
        &object_bytes,
        &ran,
        size_label,
    );
}

fn benchmark_compressed<P, F>(
    criterion: &mut Criterion,
    pairs: &[Pair],
    name: &str,
    make_patch: F,
    object_size: impl Fn(&Arc<BeaconState<Mainnet>>) -> usize,
) where
    F: Fn(&Arc<BeaconState<Mainnet>>, &Arc<BeaconState<Mainnet>>) -> P,
    P: Patch<Arc<BeaconState<Mainnet>>> + SszWrite + SszRead<()>,
{
    if pairs.is_empty() {
        return;
    }

    let patch_bytes = pairs
        .iter()
        .map(|pair| compress(&make_patch(&pair.before, &pair.after)).len())
        .collect::<Vec<_>>();
    let object_bytes = pairs
        .iter()
        .map(|pair| object_size(&pair.before).max(object_size(&pair.after)))
        .collect::<Vec<_>>();
    let ran = pairs.iter().map(|_| Cell::new(false)).collect::<Vec<_>>();

    let mut group = criterion.benchmark_group(name);

    for (pair, ran) in pairs.iter().zip(&ran) {
        let compressed_patch = compress(&make_patch(&pair.before, &pair.after));
        let applied = apply_compressed::<_, P>(&pair.before, compressed_patch.as_slice());

        assert_eq!(
            applied.hash_tree_root(),
            pair.after.hash_tree_root(),
            "applied patch root mismatch for {}",
            pair.label,
        );

        group.bench_function(BenchmarkId::new("diff", &pair.label), |bencher| {
            ran.set(true);
            bencher.iter(|| {
                let patch = make_patch(black_box(&pair.before), black_box(&pair.after));

                black_box(compress(&patch));
            });
        });

        group.bench_function(BenchmarkId::new("apply", &pair.label), |bencher| {
            bencher.iter(|| {
                black_box(apply_compressed::<_, P>(
                    black_box(&pair.before),
                    black_box(compressed_patch.as_slice()),
                ));
            });
        });
    }

    group.finish();

    print_size_report(
        name,
        &pairs,
        &patch_bytes,
        &object_bytes,
        &ran,
        "zstd bytes",
    );
}

fn print_size_report(
    name: &str,
    pairs: &[Pair],
    patch_bytes: &[usize],
    object_bytes: &[usize],
    ran: &[Cell<bool>],
    patch_label: &str,
) {
    let label_width = pairs
        .iter()
        .zip(ran)
        .filter(|(_, ran)| ran.get())
        .map(|(pair, _)| pair.label.len())
        .max();

    let Some(label_width) = label_width else {
        return;
    };

    println!("\n{name} diff size report");
    println!(
        "  {:<label_width$}  {:>12}  {:>12}  {:>8}",
        "pair", patch_label, "object bytes", "patch %"
    );

    for (((pair, patch_bytes), object_bytes), ran) in
        pairs.iter().zip(patch_bytes).zip(object_bytes).zip(ran)
    {
        if ran.get() {
            let percentage = if *object_bytes == 0 {
                String::from("n/a")
            } else {
                format!("{:.2}%", *patch_bytes as f64 * 100.0 / *object_bytes as f64)
            };

            println!(
                "  {:<label_width$}  {patch_bytes:>12}  {object_bytes:>12}  {percentage:>8}",
                pair.label,
            );
        }
    }

    println!();
}

fn load_pairs() -> Result<Vec<Pair>> {
    let config = Config::mainnet();
    let asset_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("benches/assets");
    let client = Client::builder()
        .timeout(Duration::from_secs(600))
        .build()
        .context("failed to build HTTP client")?;

    fs::create_dir_all(&asset_dir).context("failed to create asset directory")?;

    let slots = STATE_PAIRS
        .into_iter()
        .flat_map(|(_, before, after)| [before, after])
        .collect::<HashSet<_>>();

    let mut states = HashMap::new();

    for slot in slots {
        let path = asset_dir.join(format!("state_{slot}.ssz"));
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                let url = format!("{BEACON_API}/eth/v2/debug/beacon/states/{slot}");

                println!("downloading beacon state for slot {slot} from {url}");

                let bytes = client
                    .get(&url)
                    .header(ACCEPT, "application/octet-stream")
                    .send()
                    .with_context(|| format!("request for slot {slot} failed"))?
                    .error_for_status()
                    .with_context(|| format!("beacon node returned an error for slot {slot}"))?
                    .bytes()
                    .with_context(|| format!("failed to read response body for slot {slot}"))?
                    .to_vec();

                let tmp_path = path.with_extension("ssz.part");
                fs::write(&tmp_path, &bytes)
                    .with_context(|| format!("failed to write state for slot {slot}"))?;
                fs::rename(&tmp_path, &path)
                    .with_context(|| format!("failed to finalize state for slot {slot}"))?;

                bytes
            }
            Err(error) => {
                return Err(error).with_context(|| format!("failed to read state for slot {slot}"));
            }
        };

        let mut state = BeaconState::<Mainnet>::from_ssz(&config, &bytes)
            .with_context(|| format!("failed to decode state for slot {slot}"))?;

        state.validators_mut().clear_pubkeys(usize::MAX);

        states.insert(slot, Arc::new(state));
    }

    let mut pairs = Vec::new();

    for (label, before, after) in STATE_PAIRS {
        let before_state = states
            .get(&before)
            .expect("before state was loaded")
            .clone();
        let after_state = states.get(&after).expect("after state was loaded").clone();

        pairs.push(Pair {
            label: format!("{label} ({before}->{after})"),
            before: before_state,
            after: after_state,
        });
    }

    Ok(pairs)
}

fn read_u64_le(bytes: &[u8], index: usize) -> u64 {
    let start = index * 8;
    u64::from_le_bytes(
        bytes[start..start + 8]
            .try_into()
            .expect("8 bytes per balance"),
    )
}

fn encode_balance_deltas(before: &[u8], after: &[u8]) -> Vec<u8> {
    assert_eq!(before.len() % 8, 0, "before balances must be u64-aligned");
    assert_eq!(after.len() % 8, 0, "after balances must be u64-aligned");

    let before_count = before.len() / 8;
    let after_count = after.len() / 8;

    assert!(after_count >= before_count, "balances list must not shrink");

    let mut buffer = unsigned_varint::encode::u64_buffer();
    let mut out = Vec::with_capacity(after.len());

    out.extend_from_slice(unsigned_varint::encode::u64(
        before_count as u64,
        &mut buffer,
    ));

    for i in 0..before_count {
        let b = read_u64_le(before, i) as i128;
        let a = read_u64_le(after, i) as i128;
        let delta = a - b;
        let zz = ((delta << 1) ^ (delta >> 127)) as u128 as u64;
        out.extend_from_slice(unsigned_varint::encode::u64(zz, &mut buffer));
    }

    let appended_count = after_count - before_count;
    out.extend_from_slice(unsigned_varint::encode::u64(
        appended_count as u64,
        &mut buffer,
    ));
    out.extend_from_slice(&after[before_count * 8..]);
    out
}

fn decode_balance_deltas(before: &[u8], encoded: &[u8]) -> Vec<u8> {
    let (common, mut rest) =
        unsigned_varint::decode::u64(encoded).expect("delta header should decode");
    let common = common as usize;

    assert_eq!(
        common * 8,
        before.len(),
        "delta common count must match source",
    );

    let mut out = Vec::with_capacity(before.len());

    for i in 0..common {
        let (zz, next) = unsigned_varint::decode::u64(rest).expect("delta value should decode");
        rest = next;
        let delta = ((zz >> 1) as i64) ^ -((zz & 1) as i64);
        let b = read_u64_le(before, i) as i128;
        let v = u64::try_from(b + delta as i128).expect("patched balance must fit in u64");
        out.extend_from_slice(&v.to_le_bytes());
    }

    let (appended, rest) =
        unsigned_varint::decode::u64(rest).expect("delta appended length should decode");
    let appended = appended as usize;

    assert_eq!(
        appended * 8,
        rest.len(),
        "delta appended payload must match length",
    );

    out.extend_from_slice(rest);
    out
}

struct Pair {
    label: String,
    before: Arc<BeaconState<Mainnet>>,
    after: Arc<BeaconState<Mainnet>>,
}
