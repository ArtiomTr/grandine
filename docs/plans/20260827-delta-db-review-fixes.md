# Delta DB review fixes

## Overview

Address the review notes left on the `feature/delta-db-glamsterdam` branch (commits
`25c19d37..e49e73cf` — the `diff` crate, delta-encoded state storage, and their docs).

The notes fall into four groups:

1. **Correctness** — the balance patch mode estimator is built on a wrong assumption and
   silently drops the most common case; the sub-epoch hierarchy restriction rejects valid
   configurations; the `cache_sizes` length constraint is stricter than it needs to be.
2. **Design** — `--state-hierarchy` lists exponents in the wrong direction; `ArchivalPermits`
   should be a real managed thread pool instead of an atomic counter with an inline fallback;
   `Hierarchy` should serialize itself instead of being hand-encoded in `storage.rs`;
   `StateStorageConfig` belongs in its own file.
3. **Dead weight** — the `comparison` Cargo feature, an invented "database predates hierarchy
   recording" migration path, a redundant `verify_or_record_hierarchy` call, and a pile of
   over-explanatory doc comments.
4. **Docs** — `book/src/storage.md` states the `--archival-epoch-interval` conversion
   incorrectly and presents it as a reasonable thing to do.

Two notes ask for measurement rather than a code change: verify whether `QueuePatch` beats
`PositionalPatch` for `pending_consolidations`, and re-run the comparison benchmarks on real
mainnet states after the balance-mode fix and report the numbers.

## Context (from discovery)

Files/components involved:

- `diff/Cargo.toml`, `diff/src/lib.rs` — `comparison` feature and its optional dependencies
- `diff/src/list.rs`, `diff/src/list/balances.rs`, `diff/src/list/participation.rs`,
  `diff/src/list/validators.rs` — `Unlimited` visibility, mode estimator, repeated `base_len` docs
- `diff/src/beacon_state/electra.rs:77` — `TODO(delta-db)` on `pending_consolidations`
- `diff/benches/comparison.rs`, `diff/benches/beacon_state.rs` — benches (kept; only the feature goes)
- `fork_choice_control/src/hierarchy.rs` — `Hierarchy`, exponent order, all layer algorithms
- `fork_choice_control/src/storage.rs` — `StateStorageConfig`, `MaxHierarchyDepth`,
  `verify_or_record_hierarchy`, `load`, anchor-change warning, `forward_spine` doc
- `fork_choice_control/src/storage_back_sync.rs:41` — redundant `verify_or_record_hierarchy` call
- `fork_choice_control/src/archival_permits.rs` — permit counter to replace with a thread pool
- `fork_choice_control/src/queries.rs:344` — doc comment to delete
- `fork_choice_control/src/spine.rs`, `fork_choice_control/src/frame_cache.rs` — already index
  layer `0` as the shallowest (snapshot) layer, so they align with the rotated exponent order
- `database/src/lib.rs` — five "copied out of the database" doc comments to trim
- `runtime/src/grandine_args.rs` — `--state-hierarchy`, `--state-cache-sizes` flags and tests
- `book/src/storage.md`, `book/src/cli_options.md` — user-facing docs

Related patterns found:

- `Spine` and `FrameCache` index layers shallowest-first (`0` = snapshot). Today that is the
  *reverse* of `Hierarchy::exponents()`; after the rotation the two orders coincide, which is
  what makes the `cache_sizes` relaxation natural.
- `Hierarchy` already implements `Display`/`FromStr`; adding `SszRead`/`SszWrite` follows the
  same "the type owns its encodings" shape.
- The `diff` crate has no default BLS/KZG backend concerns, but `fork_choice_control` does —
  see the test commands in `CLAUDE.md`.

Dependencies identified:

- Task 1 (rotation) must land before Tasks 2, 4 and 15 (docs), because it changes the meaning
  of `exponents()[0]` and of the `--state-hierarchy` flag string.
- Task 2 (`SszRead`/`SszWrite` on `Hierarchy`) must land before Task 6 (folding
  `verify_or_record_hierarchy` into `load`).
- Task 10 (balance mode fix) must land before Task 11 (benchmark re-run).

Decisions taken during planning:

- `--force-reset-beacon-db` **stays** — the flag, its book entry, and the error messages that
  point at it. It predates this branch (PR #585); the review note was based on it looking new.
- The `comparison` **benchmark stays**; only the Cargo *feature* gate goes away. Its
  dependencies move to `[dev-dependencies]` unconditionally, which means a C toolchain is
  required to build the dev-dependency graph of `diff`.

## Development Approach

- **Testing approach**: Regular (code first, then tests)
- Complete each task fully before moving to the next
- Make small, focused changes
- **CRITICAL: every task MUST include new/updated tests** for code changes in that task
  - tests are not optional - they are a required part of the checklist
  - write unit tests for new functions/methods
  - write unit tests for modified functions/methods
  - add new test cases for new code paths
  - update existing test cases if behavior changes
  - tests cover both success and error scenarios
- **CRITICAL: all tests must pass before starting next task** - no exceptions
- **CRITICAL: update this plan file when scope changes during implementation**
- Run tests after each change
- Maintain backward compatibility

Pure comment/doc-deletion tasks (Tasks 12-14) have no behaviour to test; for those, "run tests"
means confirming the affected crates still build and their existing suites pass.

## Testing Strategy

- **Unit tests**: required for every task that changes behaviour (see Development Approach)
- **E2E tests**: this project has no UI-based e2e suite; not applicable
- Crate-scoped commands during development (from `CLAUDE.md`):
  - `cargo test -p diff -p bls --features bls/blst`
  - `cargo test -p fork_choice_control -p bls -p kzg_utils --features bls/blst,kzg_utils/blst`
  - `cargo test -p runtime -p bls -p kzg_utils --features bls/blst,kzg_utils/blst`
- Full suite at the end: `make test` (limit parallelism — it is thread- and memory-hungry)
- Benchmarks need `REMOTE_URL` pointing at an archival node's debug-state endpoint:
  - `REMOTE_URL='http://<node>:5052/eth/v2/debug/beacon/states/{slot}' cargo bench -p diff --bench beacon_state`
  - `REMOTE_URL='...' cargo bench -p diff --bench comparison`

## Progress Tracking

- Mark completed items with `[x]` immediately when done
- Add newly discovered tasks with ➕ prefix
- Document issues/blockers with ⚠️ prefix
- Update plan if implementation deviates from original scope
- Keep plan in sync with actual work done

## What Goes Where

- **Implementation Steps** (`[ ]` checkboxes): code changes, tests, documentation updates
- **Post-Completion** (no checkboxes): benchmark runs against a real archival node, and the
  numbers to report back to the reviewer

## Implementation Steps

### Task 1: Rotate `--state-hierarchy` to list exponents shallowest-first

Review note: *"The hierarchy must be rotated - from highest point to lowest."*

The flag currently reads deepest-first (`5,9,11,13,16,18,21`). It must read
highest-to-lowest (`21,18,16,13,11,9,5`), so that the first exponent is the snapshot layer —
matching how `Spine` and `FrameCache` already index layers, and how `--state-cache-sizes` is
already ordered.

- [x] flip `Hierarchy::default()` in `fork_choice_control/src/hierarchy.rs` to `[21, 18, 16, 13, 11, 9, 5]`
- [x] change `Hierarchy::new` validation from "sorted ascending" to "strictly decreasing", keeping the
      non-empty, no-duplicates and `<= 63` rules, and update the error strings
- [x] update `contains_relative` and `parent_of_relative` to take the deepest exponent from
      `exponents.last()` and to walk levels in the new direction (the `rposition` searches become
      `position` searches over the reversed ordering)
- [x] update `is_leaf` (the "bottom level" check currently compares against index `0`) and `spine`
      (its `base` currently comes from `exponents[0]`)
- [x] update the `exponents()` doc comment and every caller that assumes deepest-first
      (`grep -rn "exponents()" --include=*.rs`)
- [x] update `Hierarchy`'s type-level doc comment: the *first* exponent is now the full-state
      snapshot layer, each later exponent is a delta layer against the previous one
- [x] update the `--state-hierarchy` clap help in `runtime/src/grandine_args.rs` to say
      "starting from the shallowest layer ... strictly decreasing"
- [x] update all existing `hierarchy([...])` fixtures in `hierarchy.rs` tests, `spine.rs` tests,
      `storage.rs` tests, `storage_back_sync.rs` tests and `grandine_args.rs` tests to the new order
- [x] write tests: `new` rejects ascending and equal exponents, accepts strictly decreasing
- [x] write tests: `display_and_from_str_round_trip` covers `21,18,16,13,11,9,5`
- [x] write tests: `parent_of`, `is_leaf`, `spine` and `contains` produce the same slot sets as
      before the rotation for an equivalent hierarchy (i.e. this is a pure notation change)
- [x] run tests - must pass before next task

### Task 2: Implement `SszRead`/`SszWrite` on `Hierarchy`

Review note: *"instead of doing this, and other things, just implement SszRead/SszWrite on
hierarchy itself, and move this out of the file."*

- [x] implement `SszSize`, `SszWrite` and `SszRead<C>` for `Hierarchy` in
      `fork_choice_control/src/hierarchy.rs`, encoding the exponents as a variable-length
      `u8` list and running the same validation as `Hierarchy::new` on read
- [x] delete `type MaxHierarchyDepth = U64;` from `fork_choice_control/src/storage.rs` and the
      now-unused `typenum::U64` import
- [x] replace every `ByteList::<MaxHierarchyDepth>` round-trip at the `StateHierarchyKey` call
      sites with direct `Hierarchy` (de)serialization
- [x] update `Error::StateHierarchyMismatch` to format the stored hierarchy via `Display`
      instead of joining raw bytes with `itertools`
- [x] write tests for SSZ round-trip of the default hierarchy and of a one-layer hierarchy
- [x] write tests: decoding rejects an empty list, non-decreasing exponents and an exponent above 63
- [x] run tests - must pass before next task

### Task 3: Move `StateStorageConfig` into its own file

Review note: *"Config needs to be moved into its own, separate file."*

- [ ] create `fork_choice_control/src/state_storage_config.rs` holding `StateStorageConfig`,
      its `Default` and `impl` blocks, and `MAX_STATE_CACHE_SIZE`
- [ ] register the module in `fork_choice_control/src/lib.rs` and re-export `StateStorageConfig`
      from the same place it is exported today
- [ ] update imports in `storage.rs`, `storage_back_sync.rs`, `runtime/src/grandine_args.rs`,
      `runtime/src/misc.rs`, `http_api/src/context.rs` and any other caller
- [ ] move the existing `StateStorageConfig` tests along with it
- [ ] run tests - must pass before next task

### Task 4: Relax the `cache_sizes` length constraint

Review note: *"after you change hierarchy config exponent order, this can be changed too -
cache_sizes no longer have to be the same length, as hierarchy exponents. Instead, they can be
padded with zeros, if necessary, relaxing constraint to simply
`self.cache_sizes.len() <= self.hierarchy.depth()`."*

- [ ] change the `ensure!` in `StateStorageConfig::validate` to `cache_sizes.len() <= hierarchy.depth()`
      with a message naming both counts
- [ ] pad `cache_sizes` with zeros up to `hierarchy.depth()` where the layers are built, so
      `FrameCache::new` still receives one entry per layer
- [ ] simplify `default_cache_sizes` now that trailing zeros are implied — it can return `[5, 3, 3]`
      truncated to `depth`, without the `repeat(0)` chain
- [ ] write tests: a shorter-than-depth `cache_sizes` is accepted and pads to depth
- [ ] write tests: a longer-than-depth `cache_sizes` is rejected with the expected message
- [ ] write tests: `--state-cache-sizes 5` on the default hierarchy starts successfully
      (update `state_cache_sizes_must_match_the_hierarchy_depth` in `grandine_args.rs`)
- [ ] run tests - must pass before next task

### Task 5: Replace the `MAX_STATE_CACHE_SIZE` hard error with a memory warning

Review note: *"Remove this check - I wouldn't call it absurd, we probably just emit a warning -
estimated cache size is more than 80% of currently available RAM."*

- [ ] delete the `MAX_STATE_CACHE_SIZE` constant, its `ensure!` and its explanatory comment
- [ ] estimate the cache footprint from the configured sizes and a per-state size estimate, and
      warn when it exceeds 80% of total system memory, naming the estimate and the available RAM
      and saying the node will be OOM-killed once the caches fill
- [ ] use the memory-probing dependency already in the workspace rather than adding a new one
      (`grep -rn "sysinfo\|available_memory\|total_memory" --include=*.rs --include=Cargo.toml`);
      if none exists, keep the check purely relative to a documented per-state constant and note
      the deviation here with a ⚠️
- [ ] write tests: an oversized configuration validates successfully (no longer an error)
- [ ] write tests: the estimator crosses the threshold at the expected size
- [ ] run tests - must pass before next task

### Task 6: Fold hierarchy recording into `load` and delete the invented migration path

Review notes: *"this doesn't have to be its own method - instead, it should be a part of load()
directly"* and *"It is not possible to have StateAnchorKey but not StateHierarchyKey - AT NO
POINT IN TIME, THERE WAS A DELTA DATABASE WITHOUT HIERARCHY KEY WRITTEN. THIS MEANS WE DON'T
NEED TO HANDLE THIS CASE."*

- [ ] delete the `StateAnchorKey`-present branch and its `warn_with_peers!` entirely: a database
      with no `StateHierarchyKey` simply records the configured hierarchy
- [ ] inline the remaining record-or-verify logic into `Storage::load` and delete
      `verify_or_record_hierarchy`
- [ ] delete the method's doc comment (*"Record the configured state hierarchy in a database that
      names none, or verify that it matches..."*)
- [ ] keep the mismatch `ensure!` fatal, with the message that names both hierarchies and
      `--force-reset-beacon-db`
- [ ] update the `verify_or_record_hierarchy` tests in `storage.rs` to drive `load` instead
- [ ] write tests: a fresh database records the configured hierarchy on load
- [ ] write tests: a database with a different stored hierarchy fails to load with the mismatch error
- [ ] run tests - must pass before next task

### Task 7: Drop the hierarchy check from `archive_back_sync_states`

Review note: *"needs double checking - probably this thing isn't necessary, because either way,
store is initialized through .load() first."*

- [ ] trace every path reaching `archive_back_sync_states` and confirm `Storage::load` always ran
      first; record the finding in this task's notes
- [ ] if confirmed, delete the call at `fork_choice_control/src/storage_back_sync.rs:41`;
      if a path is found that bypasses `load`, keep the check and note the path here with a ⚠️
- [ ] update `storage_back_sync.rs` tests that pre-seed `StateHierarchyKey` for this call
- [ ] write tests: back-sync archival succeeds on a store initialized through `load`
- [ ] run tests - must pass before next task

### Task 8: Remove the anchor-change warning

Review note: *"That is absolutely idiotic explanation for a user - user don't care about it.
No need to emit warning at all."*

- [ ] delete the `stored_hierarchy_anchor.is_some_and(...)` warning block in `Storage::load`
- [ ] drop the `warn_with_peers` import if nothing else in the file uses it
- [ ] trim the matching paragraph in `book/src/storage.md` so it no longer promises a warning
      (the behaviour it describes — old-anchor states stay readable until pruning reaches them —
      is still true and worth keeping)
- [ ] write tests: loading over a database with a different anchor still succeeds
- [ ] run tests - must pass before next task

### Task 9: Replace `ArchivalPermits` with a managed archival thread pool

Review notes: *"this needs to be replaced with simple semaphore"* and *"this needs much more
reworking - specifically, we need to do a managed threadpool implementation instead. It should:
1. Scale up to `num_cores / 2`; 2. All of these threads have a shared channel, that acts as a
queue for input tasks, which need to be offloaded; 3. Once queue is empty, threadpool has to be
downscaled back to smaller size."*

- [ ] replace `ArchivalPermits`/`ArchivalPermit` with a pool type holding a shared task channel
      and a worker count bounded by `num_cores / 2`
- [ ] submitting work pushes onto the channel and spawns an additional worker only while the
      worker count is below the cap and the queue is non-empty
- [ ] workers exit after the queue drains, so the pool downscales back to its idle size
- [ ] delete `MAX_CONCURRENT_ARCHIVAL_THREADS`, `spawn_or_run`, the inline-fallback behaviour and
      the doc comment explaining it; callers in the fork choice mutator submit and return
- [ ] rename the module and its `fork_choice_control/src/lib.rs` registration to match
- [ ] update both mutator call sites
- [ ] write tests: submitted work all runs, and the worker count never exceeds the cap
- [ ] write tests: the pool downscales after the queue drains
- [ ] write tests: work submitted from several threads concurrently is not dropped
- [ ] run tests - must pass before next task

### Task 10: Fix the balance patch mode estimator

Review note: *"This algorithm is incorrect, because it relies on wrong assumption - 'mode is
encoded as Gwei, which leaves no room for a sign'. But it leaves - mode just has to be zigzag
encoded. The precise, fixed algorithm must look like this: iterate through all counts, skip
cases where balances don't change, or after == 0 (set to zero operation); for all other
balances, compute delta, and zigzag encode it; choose the most repeated value. When applying
mode, it should be zigzag decoded."*

- [ ] rewrite `BalancesPatch::estimate_mode` in `diff/src/list/balances.rs`: skip unchanged
      balances and skip `after == 0`, compute the signed `after - before` delta for the rest,
      zigzag-encode it, and return the most repeated zigzagged value
- [ ] store the mode zigzag-encoded in the `mode: Gwei` field and unzigzag it in both `diff` and
      `apply`, so decreases can be the mode
- [ ] delete the `estimate_mode` doc comment — the review asks for it to go, and it documents
      the assumption being removed
- [ ] keep the deterministic tie-break, restated in terms of the zigzagged value
- [ ] write tests: a uniform *decrease* across balances is picked as the mode and encodes to
      minimal deltas (this is the case the old algorithm missed entirely)
- [ ] write tests: `after == 0` entries never contribute to the mode and still round-trip as the
      set-to-zero opcode
- [ ] write tests: diff/apply round-trips for mixed increases and decreases, and for a base
      shorter than the changed list
- [ ] run tests - must pass before next task

### Task 11: Measure the balance-mode fix and the `pending_consolidations` patch choice

Review notes: *"after implementing correct mode computation algorithm, please re-run comparison
benchmarks on real data, and report measurement results"* and *"run an experiment & verify if
QueuePatch takes less space, on real states."*

Requires `REMOTE_URL` pointing at an archival beacon node — see Post-Completion if unavailable.

- [ ] run `cargo bench -p diff --bench beacon_state` before and after the Task 10 change and
      record delta sizes and timings
- [ ] run `cargo bench -p diff --bench comparison` against `eth-state-diff`, `qbsdiff` and
      `xdelta3` and record the table
- [ ] swap `pending_consolidations` in `diff/src/beacon_state/electra.rs` from
      `Compressed<PositionalPatch<PendingConsolidation>>` to `Compressed<QueuePatch<...>>` and
      benchmark both variants on real states
- [ ] keep whichever variant is smaller/faster and delete the `TODO(delta-db)` comment either way
- [ ] record all measurements in this plan under this task, so the numbers can be reported
- [ ] write tests: round-trip test for whichever `pending_consolidations` patch type is kept
- [ ] run tests - must pass before next task

### Task 12: Remove the `comparison` Cargo feature

Review notes: *"remove this feature please"*, *"move these into dev-dependencies"*,
*"Comparison feature isn't needed - prune it from everywhere"*. The benchmark itself stays.

- [ ] move `eth-state-diff`, `qbsdiff`, `rkyv` and `xdelta3` from `[dependencies]` to
      `[dev-dependencies]` in `diff/Cargo.toml`, dropping `optional = true`
- [ ] keep the one-line per-dependency comments explaining what each one is; delete the paragraph
      about optionality and the C toolchain
- [ ] delete the `[features]` section and the `required-features = ['comparison']` line from the
      `comparison` bench target
- [ ] delete the `#[cfg(feature = "comparison")]` re-exports and their comment in `diff/src/lib.rs`,
      moving whatever `diff/benches/comparison.rs` needs into the bench itself
- [ ] update the benchmark section of `CLAUDE.md`: `cargo bench -p diff --bench comparison` no
      longer needs `--features comparison`, and the C toolchain is now needed for any
      `diff` dev-dependency build
- [ ] run `cargo bench -p diff --bench comparison --no-run` and `cargo test -p diff -p bls --features bls/blst`
      to confirm both still build
- [ ] run tests - must pass before next task

### Task 13: Narrow `diff::list::Unlimited` to `pub(crate)`

Review note: *"this should be pub(crate) instead - not published to everything."*

- [ ] change `pub type Unlimited` to `pub(crate) type Unlimited` in `diff/src/list.rs`
- [ ] check whether `Unlimited` appears in any public signature (`grep -rn "Unlimited" diff/`);
      if it does, keep those types private too or note the blocker here with a ⚠️
- [ ] run `cargo test -p diff -p bls --features bls/blst` to confirm the crate still builds
- [ ] run tests - must pass before next task

### Task 14: Delete the over-explanatory doc comments

Every note in this group is a plain deletion.

- [ ] `database/src/lib.rs:370`, `:484`, `:537` — delete the three
      *"The keys are copied out of the database, for the same reason `Database::next_raw` copies."* lines
- [ ] `database/src/lib.rs:425-428` — delete the *"Both key and value are copied out..."* paragraph,
      keeping the rest of the doc comment
- [ ] `database/src/lib.rs:715-718` — delete the *"The data is copied out of the database.
      `Cow::Borrowed` values..."* paragraph
- [ ] `diff/src/list/balances.rs:18-21`, `diff/src/list/participation.rs:14`,
      `diff/src/list/validators.rs:24` — delete the repeated *"Length of the base this patch was
      computed against..."* comments in all three files
- [ ] `fork_choice_control/src/queries.rs:344-345` — delete the
      *"Like `Self::state_at_slot_blocking`, but rejects slots too far in the future..."* comment
- [ ] `fork_choice_control/src/storage.rs:174-175` — delete the
      *"The spine tracking states persisted by forward sync..."* comment on `forward_spine`
- [ ] `grep -rn "for the same reason" database/ diff/ fork_choice_control/` to confirm none remain
- [ ] run tests - must pass before next task

### Task 15: Relax the sub-epoch deepest-exponent restriction

Review note: *"this check probably needs to be relaxed a little bit - this currently fails for
values, like 1, 2, 4, 8, 16 - perfectly valid ones. Specifically, you may want to save states
'inside of epoch', and that will still imply, that you're saving state 'at the beginning of
epoch'. So no, it doesn't need to be whole number of epochs - it may be smaller than epoch.
Storage loading algorithm probably has to be updated too."*

This is the largest correctness change; it lands last because it touches the read path.

- [ ] delete the `deepest_interval.is_multiple_of(slots_per_epoch)` `ensure!` from
      `StateStorageConfig::validate` and its explanatory comment
- [ ] audit every read path that treats a stored state as an anchor and assumes it sits at an
      epoch start (`grep -rn "is_epoch_start\|compute_start_slot_at_epoch" fork_choice_control/src/storage*.rs`)
- [ ] update those paths to accept a mid-epoch anchor: loading a state at an arbitrary hierarchy
      slot must still find its parent frame and replay from it correctly
- [ ] confirm the deepest exponent no longer needs to be `>= 5` on Mainnet / `>= 3` on minimal,
      and that `--state-hierarchy 21,16,4` (or similar) starts and round-trips states
- [ ] write tests: `validate` accepts sub-epoch deepest exponents (`4`, `3`, `2`, `1`, `0`)
- [ ] write tests: store and read back a state at a mid-epoch hierarchy slot
- [ ] write tests: the pruning-retention property test in `hierarchy.rs` also holds for a
      sub-epoch deepest exponent
- [ ] update `runtime/src/grandine_args.rs`'s `state_hierarchy_deepest_layer_must_be_epoch_aligned`
      test to assert the new, permissive behaviour (and rename it)
- [ ] run tests - must pass before next task

### Task 16: Verify acceptance criteria

- [ ] verify every review note in the Overview has a corresponding landed change
- [ ] verify edge cases are handled (one-layer hierarchy, sub-epoch deepest exponent, empty
      `cache_sizes`, mid-epoch anchor)
- [ ] run the full test suite: `make test` with limited parallelism
- [ ] run `cargo clippy --workspace --all-targets` - all issues must be fixed
- [ ] run `cargo fmt --check`
- [ ] verify test coverage of the changed modules meets the project standard

### Task 17: [Final] Update documentation

- [ ] rewrite the `--archival-epoch-interval` section of `book/src/storage.md`. The current text
      is wrong: because the old code stored a state at **every** epoch start once forward-synced
      (`(store.is_forward_synced() && is_epoch_start) || is_archival_epoch_start`), the interval
      only ever throttled back-fill. The old effective density is therefore one state per epoch,
      i.e. `--state-hierarchy 5`, not `10`. Say that you *can* replicate it that way, but that a
      single-layer hierarchy makes every stored state a full copy — far more disk than the
      default hierarchy, and slower reads because every read decompresses a whole state — so it
      is not recommended.
- [ ] update the `--state-hierarchy` prose and every example in `book/src/storage.md` to the
      rotated, shallowest-first order (`21,18,16,13,11,9,5`), including the
      *"must be non-empty, strictly increasing"* sentence and the `21,16,5` rejection example
      (which is now the *valid* form)
- [ ] update the `--state-cache-sizes` paragraph: the list may now be shorter than the hierarchy
      and is zero-padded, and it is no longer in the reverse order of `--state-hierarchy`
- [ ] update the deepest-exponent paragraph for the relaxed sub-epoch rule (Task 15)
- [ ] update the "database written before this check existed" paragraph — that path no longer
      exists (Task 6)
- [ ] regenerate/refresh `book/src/cli_options.md` so the `--state-hierarchy` help text matches
      the rotated order; leave the `--force-reset-beacon-db` entry in place
- [ ] update `CLAUDE.md` if the benchmark or test invocations changed

*Note: ralphex automatically moves completed plans to `docs/plans/completed/`*

## Technical Details

**Hierarchy exponent order.** `Hierarchy { exponents: Vec<u8> }` currently holds a strictly
ascending list where index `0` is the deepest (most frequent) layer. After Task 1 it holds a
strictly *descending* list where index `0` is the shallowest layer — the full-state snapshot.
Every algorithm keyed on `exponents[0]` (`contains_relative`, `spine`'s `base`, `is_leaf`'s
bottom-level check) moves to `exponents.last()`, and the `rposition` level searches invert.
`Spine` and `FrameCache` already index layer `0` as the snapshot, so after the rotation the
exponent index and the layer index are the same number.

**Balance mode encoding.** `BalancesPatch::mode: Gwei` currently stores an unsigned increase.
After Task 10 it stores a zigzagged `i64`: `zigzag(delta) = (delta << 1) ^ (delta >> 63)`. Each
per-balance delta is `zigzag(after - before)` compared against the mode; entries with
`after == 0` keep using opcode `0` and are excluded from mode estimation. `apply` unzigzags the
mode before adding it back.

**Hierarchy SSZ encoding.** The `StateHierarchyKey` row currently stores a
`ByteList<MaxHierarchyDepth>` of raw exponents. After Task 2 it stores the `Hierarchy`'s own
SSZ encoding. The byte layout is unchanged (a variable-length list of `u8`), so databases
written by earlier builds of this branch stay readable — but the *values* change meaning with
the Task 1 rotation, so a database written before Task 1 will now fail the mismatch check.
That is acceptable: this branch is unreleased.

**Archival thread pool.** Replaces the `AtomicUsize` permit counter. Shape: an unbounded
channel of boxed closures, worker threads spawned lazily up to `num_cpus / 2`, each worker
looping on `recv` and exiting when the queue is empty so the pool downscales. The inline
fallback that ran archival work on the fork choice mutator thread goes away — work is queued
instead, so the mutator never blocks.

## Post-Completion

**Benchmark runs requiring external infrastructure:**

Task 11 needs `REMOTE_URL` pointing at an archival beacon node's `/eth/v2/debug/beacon/states/{slot}`
endpoint. States are cached in `diff/benches/assets` and reused. If no node is reachable during
implementation, mark Task 11 with ⚠️, land Tasks 10 and 12 anyway, and run the benchmarks
afterwards — the balance-mode measurement and the `QueuePatch` vs `PositionalPatch` decision are
both explicitly requested by the reviewer and must be reported.

**Manual verification:**

- Sync a node against a real network with the rotated `--state-hierarchy` and confirm states are
  written at the expected slots and read back correctly
- Confirm the relaxed sub-epoch hierarchy (Task 15) serves historical state API requests
  correctly on a real chain
- Watch archival thread pool behaviour under load: worker count should rise toward `num_cpus / 2`
  during archival and fall back once the queue drains
