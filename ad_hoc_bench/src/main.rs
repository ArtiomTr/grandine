use core::ops::RangeInclusive;
use std::{collections::BTreeMap, hint::black_box, path::PathBuf, sync::Arc, time::Instant};

use allocator as _;
use anyhow::{Context as _, Result};
use bytesize::ByteSize;
use clap::{Args, Parser, Subcommand, ValueEnum};
use database::{Database, DatabaseMode};
use eth2_cache_utils::{goerli, holesky, holesky_devnet, mainnet, medalla, withdrawal_devnet_4};
use fork_choice_control::AdHocBenchController;
use fork_choice_store::StoreConfig;
use itertools::Itertools as _;
use logging::info_with_peers;
use pubkey_cache::PubkeyCache;
use rand::seq::SliceRandom as _;
use ssz::{SszHash as _, SszWrite as _};
use typenum::Unsigned as _;
use types::{
    combined::{BeaconState, SignedBeaconBlock},
    config::Config as ChainConfig,
    deneb::containers::BlobSidecar,
    phase0::{
        consts::GENESIS_SLOT,
        primitives::{H256, Slot},
    },
    preset::Preset,
    traits::{BeaconState as _, SignedBeaconBlock as _},
};

#[derive(Parser)]
struct App {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Block processing throughput benchmark.
    Process(Options),
    /// Load historical states from disk and report memory usage.
    ///
    /// Used to measure the effect of structurally sharing the validator pubkey
    /// list between a loaded historical state and the finalized validator list.
    HistoricalStates(HistoricalStatesOptions),
    /// Load a single historical state from disk and break its in-memory footprint
    /// down by field, so it is clear which fields scale memory as more states are held.
    Breakdown(BreakdownOptions),
}

#[derive(Clone, Args)]
struct Options {
    #[clap(value_enum)]
    blocks: Blocks,
    #[clap(value_enum)]
    order: Order,
    #[clap(value_enum)]
    mode: Mode,
    #[clap(long)]
    unfinalized_states_in_memory: Option<u64>,
    /// Specifies the directory where benchmark database files will be stored.
    /// If not provided, a temporary directory will be used by default.
    #[clap(long)]
    database_directory: Option<PathBuf>,
    /// Number of blocks to process in batches.
    #[clap(long, default_value_t = 64)]
    batch_size: usize,
    /// A list beacon block roots that beacon node rejects unconditionally.
    /// Defaults to a list of default blacklisted blocks of the specified `Config`.
    #[clap(long)]
    blacklisted_blocks: Option<Vec<H256>>,
}

#[derive(Clone, Args)]
struct HistoricalStatesOptions {
    #[clap(value_enum)]
    blocks: Blocks,
    /// Number of distinct historical states to load from disk and retain in memory
    /// simultaneously. Peak memory usage is reported while all of them are held.
    #[clap(long, default_value_t = 16)]
    num_states: usize,
    #[clap(long)]
    unfinalized_states_in_memory: Option<u64>,
    /// Specifies the directory where benchmark database files will be stored.
    /// If not provided, a temporary directory will be used by default.
    #[clap(long)]
    database_directory: Option<PathBuf>,
    /// Number of blocks to process in batches while populating the database.
    #[clap(long, default_value_t = 64)]
    batch_size: usize,
}

#[derive(Clone, Args)]
struct BreakdownOptions {
    #[clap(value_enum)]
    blocks: Blocks,
    /// Slot of the historical state to decompose. Defaults to a disk-backed slot
    /// roughly halfway through the finalized range.
    #[clap(long)]
    slot: Option<Slot>,
    #[clap(long)]
    unfinalized_states_in_memory: Option<u64>,
    /// Specifies the directory where benchmark database files will be stored.
    /// If not provided, a temporary directory will be used by default.
    #[clap(long)]
    database_directory: Option<PathBuf>,
    /// Number of blocks to process in batches while populating the database.
    #[clap(long, default_value_t = 64)]
    batch_size: usize,
}

#[derive(Clone, Copy, ValueEnum)]
enum Blocks {
    #[clap(name = "mainnet-genesis-128")]
    MainnetGenesis128,
    #[clap(name = "mainnet-genesis-1024")]
    MainnetGenesis1024,
    #[clap(name = "mainnet-genesis-2048")]
    MainnetGenesis2048,
    #[clap(name = "mainnet-genesis-8192")]
    MainnetGenesis8192,

    #[clap(name = "mainnet-altair-128")]
    MainnetAltair128,
    #[clap(name = "mainnet-altair-1024")]
    MainnetAltair1024,
    #[clap(name = "mainnet-altair-2048")]
    MainnetAltair2048,
    #[clap(name = "mainnet-altair-8192")]
    MainnetAltair8192,

    #[clap(name = "mainnet-deneb-1024")]
    MainnetDeneb1024,

    #[clap(name = "medalla-genesis-128")]
    MedallaGenesis128,
    #[clap(name = "medalla-genesis-1024")]
    MedallaGenesis1024,

    #[clap(name = "medalla-roughtime-1024")]
    MedallaRoughtime1024,
    MedallaRoughtimeFull,

    #[clap(name = "goerli-genesis-128")]
    GoerliGenesis128,
    #[clap(name = "goerli-genesis-1024")]
    GoerliGenesis1024,
    #[clap(name = "goerli-genesis-2048")]
    GoerliGenesis2048,
    #[clap(name = "goerli-genesis-8192")]
    GoerliGenesis8192,
    #[clap(name = "goerli-genesis-16384")]
    GoerliGenesis16384,

    #[clap(name = "withdrawals-2368")]
    Withdrawals2368,
    #[clap(name = "withdrawals-2496")]
    Withdrawals2496,

    #[clap(name = "holesky")]
    Holesky,
    #[clap(name = "holesky-devnet")]
    HoleskyDevnet,
    #[clap(name = "holesky-non-finality")]
    HoleskyNonFinality,
    #[clap(name = "holesky-non-finality-full")]
    HoleskyNonFinalityFull,
}

#[derive(Clone, Copy, ValueEnum)]
enum Order {
    Forward,
    Reverse,
    Shuffle,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Mode {
    Asynchronous,
    Synchronous,
}

enum Chain {
    Mainnet,
    Medalla,
    Goerli,
    Withdrawals,
    Holesky,
    HoleskyDevnet,
}

// This could be replaced with statics from `eth2_cache_utils` if it weren't for the slot width.
// The slot width is needed to deserialize the anchor state and ultimately because `eth2-cache` uses
// an inconsistent number of digits to represent slots.
struct BlockParameters {
    first_slot: Slot,
    last_slot: Slot,
    slot_width: usize,
}

impl From<Blocks> for Chain {
    fn from(blocks: Blocks) -> Self {
        match blocks {
            Blocks::MainnetGenesis128
            | Blocks::MainnetGenesis1024
            | Blocks::MainnetGenesis2048
            | Blocks::MainnetGenesis8192
            | Blocks::MainnetAltair128
            | Blocks::MainnetAltair1024
            | Blocks::MainnetAltair2048
            | Blocks::MainnetAltair8192
            | Blocks::MainnetDeneb1024 => Self::Mainnet,
            Blocks::MedallaGenesis128
            | Blocks::MedallaGenesis1024
            | Blocks::MedallaRoughtime1024
            | Blocks::MedallaRoughtimeFull => Self::Medalla,
            Blocks::GoerliGenesis128
            | Blocks::GoerliGenesis1024
            | Blocks::GoerliGenesis2048
            | Blocks::GoerliGenesis8192
            | Blocks::GoerliGenesis16384 => Self::Goerli,
            Blocks::Withdrawals2368 | Blocks::Withdrawals2496 => Self::Withdrawals,
            Blocks::Holesky | Blocks::HoleskyNonFinality | Blocks::HoleskyNonFinalityFull => {
                Self::Holesky
            }
            Blocks::HoleskyDevnet => Self::HoleskyDevnet,
        }
    }
}

impl From<Blocks> for BlockParameters {
    #[expect(clippy::too_many_lines)]
    fn from(blocks: Blocks) -> Self {
        match blocks {
            Blocks::MainnetGenesis128 | Blocks::GoerliGenesis128 => Self {
                first_slot: GENESIS_SLOT,
                last_slot: 128,
                slot_width: 6,
            },
            Blocks::MainnetGenesis1024 | Blocks::GoerliGenesis1024 => Self {
                first_slot: GENESIS_SLOT,
                last_slot: 1024,
                slot_width: 6,
            },
            Blocks::MainnetGenesis2048 | Blocks::GoerliGenesis2048 => Self {
                first_slot: GENESIS_SLOT,
                last_slot: 2048,
                slot_width: 6,
            },
            Blocks::MainnetGenesis8192 | Blocks::GoerliGenesis8192 => Self {
                first_slot: GENESIS_SLOT,
                last_slot: 8192,
                slot_width: 6,
            },
            Blocks::MainnetAltair128 => Self {
                first_slot: 3_078_848,
                last_slot: 3_078_976,
                slot_width: 7,
            },
            Blocks::MainnetAltair1024 => Self {
                first_slot: 3_078_848,
                last_slot: 3_079_872,
                slot_width: 7,
            },
            Blocks::MainnetAltair2048 => Self {
                first_slot: 3_078_848,
                last_slot: 3_080_896,
                slot_width: 7,
            },
            Blocks::MainnetAltair8192 => Self {
                first_slot: 3_078_848,
                last_slot: 3_087_040,
                slot_width: 7,
            },
            Blocks::MainnetDeneb1024 => Self {
                first_slot: 9_481_344,
                last_slot: 9_482_368,
                slot_width: 7,
            },
            Blocks::MedallaGenesis128 => Self {
                first_slot: GENESIS_SLOT,
                last_slot: 128,
                slot_width: 4,
            },
            Blocks::MedallaGenesis1024 => Self {
                first_slot: GENESIS_SLOT,
                last_slot: 1_024,
                slot_width: 4,
            },
            Blocks::MedallaRoughtime1024 => Self {
                first_slot: 73_248,
                last_slot: 74_272,
                slot_width: 5,
            },
            Blocks::MedallaRoughtimeFull => Self {
                first_slot: 74_496,
                last_slot: 127_999,
                slot_width: 6,
            },
            Blocks::GoerliGenesis16384 => Self {
                first_slot: GENESIS_SLOT,
                last_slot: 0x4000,
                slot_width: 6,
            },
            // Chain does not finalize
            Blocks::Withdrawals2368 => Self {
                first_slot: GENESIS_SLOT,
                last_slot: 2368,
                slot_width: 6,
            },
            // Chain finalizes
            Blocks::Withdrawals2496 => Self {
                first_slot: GENESIS_SLOT,
                last_slot: 2496,
                slot_width: 6,
            },
            Blocks::Holesky => Self {
                first_slot: 49920,
                last_slot: 50016,
                slot_width: 6,
            },
            Blocks::HoleskyDevnet => Self {
                first_slot: GENESIS_SLOT,
                last_slot: 2584,
                slot_width: 6,
            },
            Blocks::HoleskyNonFinality => Self {
                first_slot: 3_710_944,
                last_slot: 3_736_998,
                slot_width: 8,
            },
            Blocks::HoleskyNonFinalityFull => Self {
                first_slot: 3_710_944,
                last_slot: 3_810_977,
                slot_width: 8,
            },
        }
    }
}

fn main() -> Result<()> {
    let data_dir = tempfile::Builder::new()
        .prefix("ad_hoc_bench_")
        .rand_bytes(10)
        .tempdir()?
        .keep();

    binary_utils::initialize_tracing_logger(module_path!(), Some(&data_dir), None, false)?;
    binary_utils::initialize_rayon()?;
    #[cfg(not(target_os = "windows"))]
    print_jemalloc_stats()?;

    match App::parse().command {
        Command::Process(options) => dispatch_process(options)?,
        Command::HistoricalStates(options) => dispatch_historical_states(options)?,
        Command::Breakdown(options) => dispatch_breakdown(options)?,
    }

    #[cfg(not(target_os = "windows"))]
    print_jemalloc_stats()?;

    Ok(())
}

fn dispatch_process(options: Options) -> Result<()> {
    match options.blocks.into() {
        Chain::Mainnet => run(
            ChainConfig::mainnet(),
            options,
            mainnet::beacon_state,
            mainnet::beacon_blocks,
            mainnet::blob_sidecars,
        ),
        Chain::Medalla => run(
            ChainConfig::medalla(),
            options,
            medalla::beacon_state,
            medalla::beacon_blocks,
            |_, _| BTreeMap::new(),
        ),
        Chain::Goerli => run(
            ChainConfig::goerli(),
            options,
            goerli::beacon_state,
            goerli::beacon_blocks,
            |_, _| BTreeMap::new(),
        ),
        Chain::Withdrawals => run(
            ChainConfig::withdrawal_devnet_4(),
            options,
            withdrawal_devnet_4::beacon_state,
            withdrawal_devnet_4::beacon_blocks,
            |_, _| BTreeMap::new(),
        ),
        Chain::Holesky => run(
            ChainConfig::holesky(),
            options,
            holesky::beacon_state,
            holesky::beacon_blocks,
            holesky::blob_sidecars,
        ),
        Chain::HoleskyDevnet => run(
            ChainConfig::holesky_devnet(),
            options,
            holesky_devnet::beacon_state,
            holesky_devnet::beacon_blocks,
            |_, _| BTreeMap::new(),
        ),
    }
}

fn dispatch_historical_states(options: HistoricalStatesOptions) -> Result<()> {
    match options.blocks.into() {
        Chain::Mainnet => run_historical_states(
            ChainConfig::mainnet(),
            options,
            mainnet::beacon_state,
            mainnet::beacon_blocks,
        ),
        Chain::Medalla => run_historical_states(
            ChainConfig::medalla(),
            options,
            medalla::beacon_state,
            medalla::beacon_blocks,
        ),
        Chain::Goerli => run_historical_states(
            ChainConfig::goerli(),
            options,
            goerli::beacon_state,
            goerli::beacon_blocks,
        ),
        Chain::Withdrawals => run_historical_states(
            ChainConfig::withdrawal_devnet_4(),
            options,
            withdrawal_devnet_4::beacon_state,
            withdrawal_devnet_4::beacon_blocks,
        ),
        Chain::Holesky => run_historical_states(
            ChainConfig::holesky(),
            options,
            holesky::beacon_state,
            holesky::beacon_blocks,
        ),
        Chain::HoleskyDevnet => run_historical_states(
            ChainConfig::holesky_devnet(),
            options,
            holesky_devnet::beacon_state,
            holesky_devnet::beacon_blocks,
        ),
    }
}

fn dispatch_breakdown(options: BreakdownOptions) -> Result<()> {
    match options.blocks.into() {
        Chain::Mainnet => run_breakdown(
            ChainConfig::mainnet(),
            options,
            mainnet::beacon_state,
            mainnet::beacon_blocks,
        ),
        Chain::Medalla => run_breakdown(
            ChainConfig::medalla(),
            options,
            medalla::beacon_state,
            medalla::beacon_blocks,
        ),
        Chain::Goerli => run_breakdown(
            ChainConfig::goerli(),
            options,
            goerli::beacon_state,
            goerli::beacon_blocks,
        ),
        Chain::Withdrawals => run_breakdown(
            ChainConfig::withdrawal_devnet_4(),
            options,
            withdrawal_devnet_4::beacon_state,
            withdrawal_devnet_4::beacon_blocks,
        ),
        Chain::Holesky => run_breakdown(
            ChainConfig::holesky(),
            options,
            holesky::beacon_state,
            holesky::beacon_blocks,
        ),
        Chain::HoleskyDevnet => run_breakdown(
            ChainConfig::holesky_devnet(),
            options,
            holesky_devnet::beacon_state,
            holesky_devnet::beacon_blocks,
        ),
    }
}

#[expect(clippy::cast_precision_loss)]
#[expect(clippy::float_arithmetic)]
#[expect(clippy::too_many_lines)]
fn run<P: Preset>(
    mut chain_config: ChainConfig,
    options: Options,
    beacon_state: impl FnOnce(Slot, usize) -> Arc<BeaconState<P>>,
    beacon_blocks: impl FnOnce(RangeInclusive<Slot>, usize) -> Vec<Arc<SignedBeaconBlock<P>>>,
    blob_sidecars: impl FnOnce(RangeInclusive<Slot>, usize) -> BTreeMap<Slot, Vec<Arc<BlobSidecar<P>>>>,
) -> Result<()> {
    #[cfg(not(target_os = "windows"))]
    print_jemalloc_stats()?;

    let Options {
        blocks,
        order,
        mode,
        unfinalized_states_in_memory,
        database_directory,
        batch_size,
        blacklisted_blocks,
    } = options;

    if let Some(blacklisted_blocks) = blacklisted_blocks {
        info_with_peers!("setting blacklisted blocks to: {blacklisted_blocks:?}");
        chain_config.blacklisted_blocks = blacklisted_blocks;
    }

    let BlockParameters {
        first_slot,
        last_slot,
        slot_width,
    } = blocks.into();

    let mut blocks = beacon_blocks(first_slot..=last_slot, slot_width).into_iter();
    let mut blobs = blob_sidecars(first_slot..=last_slot, slot_width);

    let last_block_root = blocks
        .as_slice()
        .last()
        .expect("range should contain at least one block")
        .message()
        .hash_tree_root();

    let chain_config = Arc::new(chain_config);

    let unfinalized_states_in_memory = unfinalized_states_in_memory
        .unwrap_or_else(|| StoreConfig::default().unfinalized_states_in_memory);

    let store_config = StoreConfig {
        unfinalized_states_in_memory,
        ..StoreConfig::default()
    };

    let anchor_block = blocks
        .next()
        .expect("range should contain at least one block");

    let anchor_state = beacon_state(first_slot, slot_width);

    let database_dir = database_directory
        .map(Ok::<_, anyhow::Error>)
        .unwrap_or_else(|| {
            Ok(tempfile::Builder::new()
                .prefix("ad_hoc_bench_db_")
                .rand_bytes(10)
                .tempdir()?
                .keep())
        })?;

    info_with_peers!("database dir: {}", database_dir.as_path().display());

    let database = Database::persistent(
        "ad_hoc_bench_db",
        database_dir,
        ByteSize::gib(512),
        DatabaseMode::ReadWrite,
        None,
    )?;

    let (controller, _mutator_handle) = AdHocBenchController::with_p2p_tx(
        chain_config,
        Arc::new(PubkeyCache::default()),
        store_config,
        anchor_block,
        anchor_state,
        database,
        futures::sink::drain(),
    );

    controller.on_slot(last_slot);
    controller.wait_for_tasks();

    match order {
        Order::Forward => {}
        Order::Reverse => blocks.as_mut_slice().reverse(),
        Order::Shuffle => blocks.as_mut_slice().shuffle(&mut rand::thread_rng()),
    }

    let block_count = blocks.len();
    let slot_count = last_slot.saturating_sub(first_slot);

    info_with_peers!(
        "processing {block_count} blocks in {slot_count} slots (not including anchor)"
    );

    let start = Instant::now();

    for chunk in &blocks.chunks(batch_size) {
        for block in chunk {
            let slot = block.message().slot();

            controller.on_requested_block(block, None);

            if let Some(block_blobs) = blobs.remove(&slot) {
                for blob in block_blobs {
                    controller.on_api_blob_sidecar(blob, None)
                }
            }

            if mode == Mode::Synchronous {
                controller.wait_for_tasks();
            }
        }

        if mode == Mode::Asynchronous {
            controller.wait_for_tasks();
        }
    }

    let time = start.elapsed().as_secs_f64();

    let head = controller.head().value;
    assert_eq!(head.block_root, last_block_root);
    assert_eq!(head.slot(), last_slot);

    let time_per_block = time / block_count as f64;
    let time_per_slot = time / slot_count as f64;
    let block_throughput = time_per_block.recip();
    let slot_throughput = time_per_slot.recip();

    info_with_peers!("blocks processed:         {block_count}");
    info_with_peers!("slots processed:          {slot_count}");
    info_with_peers!("time taken:               {time:.3} s");
    info_with_peers!(
        "average time per block:   {:.3} ms",
        time_per_block * 1000_f64,
    );
    info_with_peers!(
        "average time per slot:    {:.3} ms",
        time_per_slot * 1000_f64,
    );
    info_with_peers!("average block throughput: {block_throughput:.3} blocks/s");
    info_with_peers!("average slot throughput:  {slot_throughput:.3} slots/s");

    #[cfg(not(target_os = "windows"))]
    print_jemalloc_stats()?;

    Ok(())
}

/// Builds a controller, processes all blocks of the dataset to populate the database with
/// archival states (and advance finalization), then invokes `body` with the controller and the
/// `(first_slot, anchor_slot, last_slot)` of the resulting chain. Historical states are loaded
/// from disk only for slots that precede `anchor_slot`.
fn with_populated_controller<P: Preset>(
    chain_config: ChainConfig,
    block_parameters: BlockParameters,
    unfinalized_states_in_memory: Option<u64>,
    database_directory: Option<PathBuf>,
    batch_size: usize,
    beacon_state: impl FnOnce(Slot, usize) -> Arc<BeaconState<P>>,
    beacon_blocks: impl FnOnce(RangeInclusive<Slot>, usize) -> Vec<Arc<SignedBeaconBlock<P>>>,
    body: impl FnOnce(&AdHocBenchController<P>, Slot, Slot, Slot) -> Result<()>,
) -> Result<()> {
    let BlockParameters {
        first_slot,
        last_slot,
        slot_width,
    } = block_parameters;

    let mut blocks = beacon_blocks(first_slot..=last_slot, slot_width).into_iter();

    let chain_config = Arc::new(chain_config);

    let unfinalized_states_in_memory = unfinalized_states_in_memory
        .unwrap_or_else(|| StoreConfig::default().unfinalized_states_in_memory);

    let store_config = StoreConfig {
        unfinalized_states_in_memory,
        ..StoreConfig::default()
    };

    let anchor_block = blocks
        .next()
        .expect("range should contain at least one block");

    let anchor_state = beacon_state(first_slot, slot_width);

    let database_dir = database_directory
        .map(Ok::<_, anyhow::Error>)
        .unwrap_or_else(|| {
            Ok(tempfile::Builder::new()
                .prefix("ad_hoc_bench_db_")
                .rand_bytes(10)
                .tempdir()?
                .keep())
        })?;

    info_with_peers!("database dir: {}", database_dir.as_path().display());

    let database = Database::persistent(
        "ad_hoc_bench_db",
        database_dir,
        ByteSize::gib(512),
        DatabaseMode::ReadWrite,
        None,
    )?;

    let (controller, _mutator_handle) = AdHocBenchController::with_p2p_tx(
        chain_config,
        Arc::new(PubkeyCache::default()),
        store_config,
        anchor_block,
        anchor_state,
        database,
        futures::sink::drain(),
    );

    controller.on_slot(last_slot);
    controller.wait_for_tasks();

    info_with_peers!("populating database: processing blocks up to slot {last_slot}");

    for chunk in &blocks.chunks(batch_size) {
        for block in chunk {
            controller.on_requested_block(block, None);
        }

        controller.wait_for_tasks();
    }

    // Ensure pending archival writes (performed on a separate thread) are flushed to disk.
    controller.wait_for_tasks();

    let anchor_slot = controller.anchor_block().message().slot();

    info_with_peers!(
        "finalized epoch: {}, store anchor slot: {anchor_slot}",
        controller.finalized_epoch(),
    );

    body(&controller, first_slot, anchor_slot, last_slot)
}

#[expect(clippy::too_many_lines)]
fn run_historical_states<P: Preset>(
    chain_config: ChainConfig,
    options: HistoricalStatesOptions,
    beacon_state: impl FnOnce(Slot, usize) -> Arc<BeaconState<P>>,
    beacon_blocks: impl FnOnce(RangeInclusive<Slot>, usize) -> Vec<Arc<SignedBeaconBlock<P>>>,
) -> Result<()> {
    let HistoricalStatesOptions {
        blocks,
        num_states,
        unfinalized_states_in_memory,
        database_directory,
        batch_size,
    } = options;

    with_populated_controller(
        chain_config,
        blocks.into(),
        unfinalized_states_in_memory,
        database_directory,
        batch_size,
        beacon_state,
        beacon_blocks,
        |controller, first_slot, anchor_slot, _last_slot| {
            let slots_per_epoch = P::SlotsPerEpoch::U64;

            // Pick `num_states` epoch-aligned slots evenly spread across the disk-backed range
            // `(first_slot, anchor_slot)`.
            let target_slots =
                select_historical_slots(first_slot, anchor_slot, slots_per_epoch, num_states);

            if target_slots.is_empty() {
                info_with_peers!(
                    "no disk-backed historical states available in range \
                     ({first_slot}, {anchor_slot}); try a larger `blocks` dataset",
                );
                return Ok(());
            }

            info_with_peers!(
                "loading {} historical states from disk at slots: {target_slots:?}",
                target_slots.len(),
            );

            #[cfg(not(target_os = "windows"))]
            let (allocated_before, resident_before) = jemalloc_usage()?;

            let start = Instant::now();

            let mut states: Vec<Arc<BeaconState<P>>> = Vec::with_capacity(target_slots.len());

            for slot in target_slots {
                let state = controller
                    .state_at_slot_blocking(slot)?
                    .map(|with_status| with_status.value);

                match state {
                    Some(state) => {
                        info_with_peers!(
                            "loaded historical state at slot {slot} ({} validators)",
                            state.validators().len_u64(),
                        );
                        states.push(state);
                    }
                    None => info_with_peers!("no stored state found for slot {slot}"),
                }
            }

            let time = start.elapsed().as_secs_f64();

            // Keep every loaded state alive so peak memory reflects all of them held at once.
            let states = black_box(states);

            let validator_count = states
                .first()
                .map(|state| state.validators().len_u64())
                .unwrap_or(0);

            info_with_peers!("retained {} historical states in memory", states.len());
            info_with_peers!("validators per state:     {validator_count}");
            info_with_peers!("time to load states:      {time:.3} s");

            #[cfg(not(target_os = "windows"))]
            {
                let (allocated_after, resident_after) = jemalloc_usage()?;
                let allocated_delta = allocated_after.saturating_sub(allocated_before);
                let resident_delta = resident_after.saturating_sub(resident_before);

                // `allocated` is precise heap accounting and is the headline metric: the delta is
                // the memory held by the retained states. With structurally shared pubkeys this
                // should be ~`num_states` smaller (for the pubkey portion) than without sharing.
                info_with_peers!(
                    "jemalloc allocated before: {} ({allocated_before} bytes)",
                    ByteSize(allocated_before).display().si(),
                );
                info_with_peers!(
                    "jemalloc allocated after:  {} ({allocated_after} bytes)",
                    ByteSize(allocated_after).display().si(),
                );
                info_with_peers!(
                    "jemalloc allocated delta:  {} ({allocated_delta} bytes; memory held by {} retained states)",
                    ByteSize(allocated_delta).display().si(),
                    states.len(),
                );
                info_with_peers!(
                    "jemalloc resident delta:   {} ({resident_delta} bytes)",
                    ByteSize(resident_delta).display().si(),
                );

                // Reference figure: pubkeys are 48 bytes each. Without structural sharing every
                // retained state holds a full, independent copy of the pubkey list, so the
                // difference between branches is expected to be close to `pubkey bytes if unshared`.
                let pubkey_bytes_per_state = validator_count.saturating_mul(48);
                let pubkey_bytes_total = pubkey_bytes_per_state.saturating_mul(states.len() as u64);

                info_with_peers!(
                    "pubkey bytes per state:    {}",
                    ByteSize(pubkey_bytes_per_state).display().si(),
                );
                info_with_peers!(
                    "pubkey bytes if unshared:  {} (across all {} retained states)",
                    ByteSize(pubkey_bytes_total).display().si(),
                    states.len(),
                );

                print_jemalloc_stats()?;
            }

            // Touch the states once more after measuring to keep them alive past the measurement.
            info_with_peers!(
                "benchmark complete; holding {} states",
                black_box(&states).len()
            );

            Ok(())
        },
    )
}

fn run_breakdown<P: Preset>(
    chain_config: ChainConfig,
    options: BreakdownOptions,
    beacon_state: impl FnOnce(Slot, usize) -> Arc<BeaconState<P>>,
    beacon_blocks: impl FnOnce(RangeInclusive<Slot>, usize) -> Vec<Arc<SignedBeaconBlock<P>>>,
) -> Result<()> {
    let BreakdownOptions {
        blocks,
        slot,
        unfinalized_states_in_memory,
        database_directory,
        batch_size,
    } = options;

    with_populated_controller(
        chain_config,
        blocks.into(),
        unfinalized_states_in_memory,
        database_directory,
        batch_size,
        beacon_state,
        beacon_blocks,
        |controller, first_slot, anchor_slot, _last_slot| {
            let slots_per_epoch = P::SlotsPerEpoch::U64;

            let target_slot = match slot {
                Some(slot) => slot,
                // Default to a disk-backed slot roughly halfway through the finalized range.
                None => *select_historical_slots(first_slot, anchor_slot, slots_per_epoch, 1)
                    .first()
                    .context(
                        "no disk-backed historical state available; try a larger `blocks` dataset",
                    )?,
            };

            breakdown_state(controller, target_slot)
        },
    )
}

/// Loads a single historical state from disk and attributes its in-memory footprint to fields by
/// clearing each field in turn and measuring how much jemalloc-tracked memory is released.
///
/// Each field's reported footprint includes that field's Merkle cache (clearing the field frees
/// its cache too). On the structurally-shared branch, the `validators` footprint excludes pubkeys
/// that remain shared with the store's finalized validator list — which is exactly the memory that
/// does *not* grow as more historical states are held.
#[cfg(not(target_os = "windows"))]
#[expect(clippy::too_many_lines)]
fn breakdown_state<P: Preset>(controller: &AdHocBenchController<P>, target_slot: Slot) -> Result<()> {
    let (base_allocated, _) = jemalloc_usage()?;

    let mut state = controller
        .state_at_slot_blocking(target_slot)?
        .map(|with_status| with_status.value)
        .context("no stored state found at target slot")?;

    let (loaded_allocated, _) = jemalloc_usage()?;
    let total_footprint = loaded_allocated.saturating_sub(base_allocated);
    let validator_count = state.validators().len_u64();

    info_with_peers!(
        "loaded state at slot {} ({validator_count} validators); \
         per-state footprint (as loaded): {} ({total_footprint} bytes)",
        state.slot(),
        ByteSize(total_footprint).display().si(),
    );

    // Populate Merkle caches the way a held, hashed state would have them.
    let root = state.hash_tree_root();
    let (rooted_allocated, _) = jemalloc_usage()?;
    let cache_fill = rooted_allocated.saturating_sub(loaded_allocated);

    info_with_peers!(
        "computed hash_tree_root {root:?}; additional cache populated by rooting: {} ({cache_fill} bytes)",
        ByteSize(cache_fill).display().si(),
    );

    let footprint_with_caches = rooted_allocated.saturating_sub(base_allocated);

    let strong_count = Arc::strong_count(&state);
    let state_mut = Arc::get_mut(&mut state).with_context(|| {
        format!("state is not uniquely owned (strong_count = {strong_count}); cannot decompose")
    })?;

    info_with_peers!(
        "decomposing state footprint by field (each figure = data + that field's Merkle cache):",
    );

    let mut prev_allocated = rooted_allocated;
    let mut accounted = 0u64;

    // Clears a field, measures the released memory, and reports it next to the field's serialized
    // size (so the cache/overhead portion is visible as the difference).
    macro_rules! report_field {
        ($label:literal, $serialized:expr, $clear:expr) => {{
            let serialized: u64 = $serialized;
            $clear;
            let (after, _) = jemalloc_usage()?;
            let footprint = prev_allocated.saturating_sub(after);
            let overhead = footprint.saturating_sub(serialized);
            let percent = if footprint_with_caches > 0 {
                footprint.saturating_mul(100) / footprint_with_caches
            } else {
                0
            };
            info_with_peers!(
                "  {:<28} {:>10} ({:>3}%)  [data ~{}, cache+overhead ~{}]",
                $label,
                ByteSize(footprint).display().si().to_string(),
                percent,
                ByteSize(serialized).display().si(),
                ByteSize(overhead).display().si(),
            );
            prev_allocated = after;
            accounted = accounted.saturating_add(footprint);
        }};
    }

    report_field!(
        "validators",
        ssz_len(state_mut.validators().to_ssz().map(|bytes| bytes.len())),
        *state_mut.validators_mut() = Default::default()
    );
    report_field!(
        "balances",
        ssz_len(state_mut.balances().to_ssz().map(|bytes| bytes.len())),
        *state_mut.balances_mut() = Default::default()
    );

    if let Some(post_altair) = state_mut.post_altair_mut() {
        report_field!(
            "previous_epoch_participation",
            ssz_len(
                post_altair
                    .previous_epoch_participation()
                    .to_ssz()
                    .map(|bytes| bytes.len())
            ),
            *post_altair.previous_epoch_participation_mut() = Default::default()
        );
        report_field!(
            "current_epoch_participation",
            ssz_len(
                post_altair
                    .current_epoch_participation()
                    .to_ssz()
                    .map(|bytes| bytes.len())
            ),
            *post_altair.current_epoch_participation_mut() = Default::default()
        );
        report_field!(
            "inactivity_scores",
            ssz_len(
                post_altair
                    .inactivity_scores()
                    .to_ssz()
                    .map(|bytes| bytes.len())
            ),
            *post_altair.inactivity_scores_mut() = Default::default()
        );
    }

    // The auxiliary `Cache` (shuffled/ordered active indices, total active balance, PTC cache).
    // The pubkey -> index map used to live here too; it now lives inside the validator list and is
    // structurally shared, so it should no longer show up as a per-state cost.
    report_field!(
        "cache (active indices, etc.)",
        0,
        {
            *state_mut.cache_mut() = Default::default();
        }
    );

    let residual = footprint_with_caches.saturating_sub(accounted);
    let residual_percent = if footprint_with_caches > 0 {
        residual.saturating_mul(100) / footprint_with_caches
    } else {
        0
    };

    info_with_peers!(
        "  {:<28} {:>10} ({:>3}%)  [other fields + state struct + top-level cache]",
        "residual",
        ByteSize(residual).display().si().to_string(),
        residual_percent,
    );

    info_with_peers!(
        "total held-state footprint (with caches): {} ({footprint_with_caches} bytes)",
        ByteSize(footprint_with_caches).display().si(),
    );

    let _ = prev_allocated;

    // Diagnostic: drop the entire state and measure how much is actually released. If memory does
    // not return close to the baseline, part of the "footprint" lives outside the state (e.g. the
    // pubkey cache or other caches populated while loading), not in the retained state itself.
    let slot = state.slot();
    drop(state);

    let (after_drop_allocated, _) = jemalloc_usage()?;
    let freed_by_dropping_state = rooted_allocated.saturating_sub(after_drop_allocated);
    let external = after_drop_allocated.saturating_sub(base_allocated);

    info_with_peers!(
        "freed by dropping whole state: {} ({freed_by_dropping_state} bytes)",
        ByteSize(freed_by_dropping_state).display().si(),
    );
    info_with_peers!(
        "still allocated above baseline after drop: {} ({external} bytes) \
         — memory attributed to load but living outside the state (caches, etc.)",
        ByteSize(external).display().si(),
    );

    info_with_peers!("breakdown complete for slot {slot}");

    Ok(())
}

#[cfg(target_os = "windows")]
fn breakdown_state<P: Preset>(
    _controller: &AdHocBenchController<P>,
    _target_slot: Slot,
) -> Result<()> {
    info_with_peers!("breakdown is only supported on platforms with jemalloc statistics");
    Ok(())
}

/// Selects up to `count` epoch-aligned slots evenly spread across the open range
/// `(lower, upper)`. Slots are rounded down to an epoch boundary and deduplicated.
fn select_historical_slots(
    lower: Slot,
    upper: Slot,
    slots_per_epoch: u64,
    count: usize,
) -> Vec<Slot> {
    if count == 0 || upper <= lower.saturating_add(slots_per_epoch) {
        return vec![];
    }

    let span = upper.saturating_sub(lower);
    let count_u64 = count as u64;

    (1..=count_u64)
        .map(|index| {
            let raw = lower.saturating_add(span.saturating_mul(index) / count_u64.saturating_add(1));
            // Round down to an epoch boundary so loads hit persisted/archival states cheaply.
            raw - (raw % slots_per_epoch)
        })
        .filter(|slot| *slot > lower && *slot < upper)
        .dedup()
        .collect()
}

/// Resolves an SSZ-length computation into a `u64`, treating any error as zero.
#[cfg(not(target_os = "windows"))]
fn ssz_len<E>(result: Result<usize, E>) -> u64 {
    u64::try_from(result.unwrap_or(0)).unwrap_or(0)
}

/// Returns `(allocated, resident)` bytes reported by jemalloc after refreshing its statistics.
#[cfg(not(target_os = "windows"))]
fn jemalloc_usage() -> Result<(u64, u64)> {
    tikv_jemalloc_ctl::epoch::advance().map_err(anyhow::Error::msg)?;
    let allocated = tikv_jemalloc_ctl::stats::allocated::read().map_err(anyhow::Error::msg)?;
    let resident = tikv_jemalloc_ctl::stats::resident::read().map_err(anyhow::Error::msg)?;
    Ok((allocated.try_into()?, resident.try_into()?))
}

#[cfg(not(target_os = "windows"))]
fn print_jemalloc_stats() -> Result<()> {
    tikv_jemalloc_ctl::epoch::advance().map_err(anyhow::Error::msg)?;

    info_with_peers!(
        "allocated: {}, \
         active: {}, \
         metadata: {}, \
         resident: {}, \
         mapped: {}, \
         retained: {}",
        human_readable_size(tikv_jemalloc_ctl::stats::allocated::read())?,
        human_readable_size(tikv_jemalloc_ctl::stats::active::read())?,
        human_readable_size(tikv_jemalloc_ctl::stats::metadata::read())?,
        human_readable_size(tikv_jemalloc_ctl::stats::resident::read())?,
        human_readable_size(tikv_jemalloc_ctl::stats::mapped::read())?,
        human_readable_size(tikv_jemalloc_ctl::stats::retained::read())?,
    );

    Ok(())
}
#[cfg(not(target_os = "windows"))]
fn human_readable_size(result: tikv_jemalloc_ctl::Result<usize>) -> Result<bytesize::Display> {
    let size = result.map_err(anyhow::Error::msg)?;
    let size = size.try_into()?;
    Ok(ByteSize(size).display().si())
}
