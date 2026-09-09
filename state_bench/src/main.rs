//! A standalone driver for the state reads the beacon API serves.
//!
//! It opens a node's database read-only, so it can be pointed at the data directory of a running
//! node and re-run as often as needed without restarting or rebuilding that node. Everything below
//! `Storage` is the same code the HTTP API runs, so a timing measured here is a timing the API
//! would see.

use core::time::Duration;
use std::{path::PathBuf, sync::Arc, time::Instant};

mod timing;

use crate::timing::{TimingLayer, Totals};

use allocator as _;
use anyhow::{Result, bail};
use bls as _;
use bytesize::ByteSize;
use clap::{Parser, ValueEnum};
use database::{Database, DatabaseMode};
use fork_choice_control::{StateStorageConfig, Storage};
use helper_functions as _;
use kzg_utils as _;
use pubkey_cache::PubkeyCache;
use ssz::SszHash as _;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt as _, util::SubscriberInitExt as _};
use transition_functions as _;
use types::{
    config::Config,
    nonstandard::StorageMode,
    phase0::primitives::Slot,
    preset::{Mainnet, Preset},
    traits::BeaconState as _,
};

/// Big enough to cover any environment this opens. Only used for the read-write path, which this
/// binary never takes, but `Database::persistent` wants a size either way.
const MAX_DATABASE_SIZE: ByteSize = ByteSize::tib(2);

#[derive(Clone, Copy, ValueEnum)]
enum Network {
    Mainnet,
}

#[derive(Parser)]
#[clap(about = "Benchmarks beacon state reads against a node's database, read-only")]
struct Options {
    /// Directory holding the `beacon_fork_choice` and `pubkey_cache` environments, e.g.
    /// `<data-dir>/mainnet/beacon`.
    #[clap(long)]
    store_directory: PathBuf,

    /// Network the database belongs to.
    #[clap(long, value_enum, default_value = "mainnet")]
    network: Network,

    /// Slots to read a state for. Repeat the flag to read several.
    #[clap(long = "slot", required = true)]
    slots: Vec<Slot>,

    /// How many times to read every slot. The first read of a slot is cold; later ones measure
    /// what the in-process caches are worth.
    #[clap(long, default_value_t = 1)]
    iterations: usize,

    /// Also hash the state, which is what `/eth/v1/beacon/states/{state_id}/root` does.
    #[clap(long)]
    hash: bool,

    /// Per-phase span timings, as the node would report them at debug level.
    #[clap(long)]
    spans: bool,

    /// Comma-separated hierarchy exponents. Must match what the node wrote the database with.
    #[clap(long)]
    hierarchy: Option<String>,

    /// Number of states cached per hierarchy layer, shallowest first.
    #[clap(long, value_delimiter = ',')]
    cache_sizes: Option<Vec<usize>>,
}

fn main() -> Result<()> {
    let options = Options::parse();

    let timings = init_logging(options.spans);

    match options.network {
        Network::Mainnet => run::<Mainnet>(&options, Config::mainnet(), timings.as_ref()),
    }
}

fn init_logging(spans: bool) -> Option<TimingLayer> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        if spans {
            EnvFilter::new(
                "warn,fork_choice_control=debug,transition_functions=debug,helper_functions=debug",
            )
        } else {
            EnvFilter::new("warn")
        }
    });

    let format = tracing_subscriber::fmt::layer()
        .with_target(true)
        .without_time();

    let timings = spans.then(TimingLayer::default);

    tracing_subscriber::registry()
        .with(filter)
        .with(format)
        .with(timings.clone())
        .init();

    timings
}

fn run<P: Preset>(options: &Options, config: Config, timings: Option<&TimingLayer>) -> Result<()> {
    let Options {
        store_directory,
        slots,
        iterations,
        hash,
        hierarchy,
        cache_sizes,
        ..
    } = options;

    let mut state_storage_config = StateStorageConfig::default();

    if let Some(hierarchy) = hierarchy {
        state_storage_config.hierarchy = hierarchy.parse()?;
    }

    if let Some(cache_sizes) = cache_sizes.clone() {
        state_storage_config.cache_sizes = cache_sizes;
    }

    state_storage_config.validate()?;

    let open = |name: &str| {
        Database::persistent(
            name,
            store_directory.join(name),
            MAX_DATABASE_SIZE,
            DatabaseMode::ReadOnly,
            None,
        )
    };

    let opened_at = Instant::now();
    let database = open("beacon_fork_choice")?;
    let pubkey_cache = Arc::new(PubkeyCache::load(open("pubkey_cache")?));

    println!(
        "opened {} read-only in {:.0?}",
        store_directory.display(),
        opened_at.elapsed(),
    );

    println!(
        "hierarchy {}, cache sizes {:?}",
        state_storage_config.hierarchy, state_storage_config.cache_sizes,
    );

    let storage = Storage::<P>::new(
        Arc::new(config),
        pubkey_cache,
        database,
        StorageMode::Archive,
        state_storage_config,
        None,
    )?;

    // The node hands `stored_state` the validator registry of its last finalized state, so the
    // pubkeys a reconstructed state is missing are filled in from memory. Reading it off disk
    // instead - which is what passing `None` does - re-reads and re-decodes the whole registry
    // once per hierarchy layer, and would dominate everything this is meant to measure. One
    // untimed read produces a registry to stand in for the node's.
    let primed_at = Instant::now();
    let primed_slot = slots.iter().copied().max().unwrap_or_default();

    let Some(primed_state) = storage.stored_state(primed_slot, None)? else {
        bail!("no state could be reconstructed for slot {primed_slot}");
    };

    let finalized_validators = primed_state.validators().clone_boxed();

    println!(
        "primed the validator registry ({} validators) from slot {primed_slot} in {:.0?}",
        finalized_validators.len_u64(),
        primed_at.elapsed(),
    );

    drop(primed_state);

    if let Some(timings) = timings {
        timings.drain();
    }

    println!();
    println!(
        "{:>12}  {:>4}  {:>10}  {:>10}  state root",
        "slot", "run", "read", "hash"
    );

    for slot in slots.iter().copied() {
        for iteration in 0..*iterations {
            let read_at = Instant::now();

            let Some(state) = storage.stored_state(slot, Some(&*finalized_validators))? else {
                bail!("no state could be reconstructed for slot {slot}");
            };

            let read = read_at.elapsed();

            let (hash_time, root) = if *hash {
                let hashed_at = Instant::now();
                let root = state.hash_tree_root();
                (hashed_at.elapsed(), format!("{root:?}"))
            } else {
                (Duration::ZERO, format!("(state at slot {})", state.slot()))
            };

            println!(
                "{slot:>12}  {iteration:>4}  {:>10}  {:>10}  {root}",
                format_duration(read),
                format_duration(hash_time),
            );

            if let Some(timings) = timings {
                print_timings(&timings.drain());
            }
        }
    }

    Ok(())
}

fn print_timings(timings: &[(String, Totals)]) {
    if timings.is_empty() {
        return;
    }

    println!();
    println!(
        "{:>44}  {:>7}  {:>10}  {:>10}",
        "span", "calls", "total", "self"
    );

    for (name, totals) in timings {
        println!(
            "{name:>44}  {:>7}  {:>10}  {:>10}",
            totals.calls,
            format_duration(totals.total),
            format_duration(totals.own),
        );
    }

    println!();
}

fn format_duration(duration: Duration) -> String {
    format!("{:.1}ms", duration.as_secs_f64() * 1000.0)
}
