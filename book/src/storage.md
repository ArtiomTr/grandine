## Storage

### Memory

By default, Grandine keeps the non-finalized part of the chain in the memory using structural sharing. This approach contributes to the high performance of Grandine because full state copies are avoided. This is a perfect approach for healthy chains (such as Ethereum Mainnet) that don't experience very long non-finalization periods. In such conditions, Grandine uses only ~1GB of memory on the Mainnet. However, during long non-finalization, this approach increases memory usage. In such cases, Grandine allows limiting the number of the latest memory stored states by settings the maximum number of the latest slots that should keep states in the memory.

### Disk

Grandine stores finalized part of the chain in the disk using an embedded key-value database `libmdbx`. Disk storage is passive and mainly used for storing/loading checkpoints, and serving historical data via API. Historical blocks and corresponding intermediate states are stored on the disk.

Grandine allows starting the Beacon Node from an earlier stored checkpoint by using `--state-slot` option. In this case, Grandine will try to find and load from the disk the closest stored checkpoint before the specified `--state-slot`.

### State hierarchy

Finalized states are not all written as full copies. Instead they are written into a *hierarchy* of layers, configured with `--state-hierarchy` as a comma separated list of slot exponents, sorted ascending. Each exponent defines how often that layer is written: an exponent of `N` means one state every `2^N` slots. The default is `5,9,11,13,16,18,21` — a state every 32 slots (one epoch) in the deepest layer, up to a state every 2097152 slots in the shallowest one.

Only the shallowest layer — the *last* exponent in the list — is written as a full state, called a snapshot or frame. Every other layer is written as a delta against the closest state in the next shallower layer. Reading a state means loading the snapshot it descends from and applying the chain of deltas down to it. This trades a small amount of CPU on read for a large reduction in disk usage compared to writing full states.

Finalized states whose slots are not part of the hierarchy at all are not written to disk. They are reconstructed on demand by loading the closest older stored state and replaying the blocks in between, which is slower than applying deltas but costs no disk space. Unfinalized states pushed out of memory by `--unfinalized-states-in-memory` are the exception: each is written as a full snapshot regardless of the hierarchy, and finalized-state archival re-encodes it as a delta later.

The hierarchy is anchored to the later of the current phase's first slot and the slot the node was started from, so deltas are never computed across a fork boundary. On a checkpoint-synced node this means stored states fall at multiples of `2^N` slots *from the anchor block*, not on absolute epoch boundaries. Checkpoint syncing over an existing database moves the anchor; states written relative to the old anchor are kept and stay readable until pruning reaches them. Pruning computes what it must retain from the *current* anchor, so it removes states written under the old one without regard for the chains they form. A historical state whose delta parent has been pruned away is reported as absent, and reads fall back to replaying blocks from an older state.

Lowering the exponents stores states more densely: faster historical API responses, more disk usage. Raising them does the opposite. Adding layers makes delta chains longer, which makes each individual write cheaper and each read more expensive.

The deepest exponent must write states at least one full epoch apart — `2^N` has to be a whole number of slots per epoch, which means `N >= 5` on Mainnet and `N >= 3` on the minimal preset. States are loaded as anchors on startup and every anchor has to be at an epoch start, so Grandine refuses to start with a denser deepest layer.

The list itself must be non-empty, strictly increasing — deepest layer first — and every exponent at most `63`. `--state-hierarchy 21,16,5` is rejected, not silently reordered.

#### Upgrading from `--archival-epoch-interval`

Earlier releases stored one full state every `--archival-epoch-interval` epochs, `32` by default, i.e. one state every 1024 slots on Mainnet. The default hierarchy stores a state every 32 slots instead, and all but the shallowest layer as deltas. The flag is now ignored, so a node that raised it to control disk usage loses that setting on upgrade.

To reproduce the old density, set the deepest exponent to `log2(interval × SLOTS_PER_EPOCH)`: `--archival-epoch-interval 32` on Mainnet corresponds to a deepest exponent of `10`, so `--state-hierarchy 10,13,16,18,21`. Note that this is not a like-for-like comparison of disk usage — the default hierarchy stores many more states, but each one is a delta rather than a full copy.

#### Database compatibility

States written by earlier Grandine releases stay readable, and archival re-encodes them as deltas as it progresses, so upgrading needs no re-sync. The upgrade is one-way: this release writes states under a new key encoding, with zstd instead of snappy, and adds two rows recording the anchor slot and the hierarchy. No state this release writes is readable by an older one, and archival progressively re-encodes the states an older release could still read, so downgrading means discarding the beacon database with `--force-reset-beacon-db` and re-syncing.

#### Changing the hierarchy of an existing database

The hierarchy is written into the database on first use, and Grandine refuses to start if the configured `--state-hierarchy` does not match the stored one:

```
database was written with state hierarchy 5,9,11, but 5,9,13 is configured;
pass --state-hierarchy 5,9,11 to keep using this database or --force-reset-beacon-db to discard it
```

This is not a cosmetic check. Pruning derives the set of states it must retain from the configured layout. If the layout does not describe the delta chains actually on disk, pruning deletes states that other, still-retained states are encoded against, silently corrupting the database. Either keep using the stored hierarchy, or discard the database with `--force-reset-beacon-db` and re-sync.

A database written before this check existed has no stored hierarchy. Grandine adopts the configured one and records it on startup. It warns while doing so only if the database also records a state hierarchy anchor, which means it was written by an earlier build of this feature and may already hold delta chains whose layout cannot be recovered - pass the `--state-hierarchy` it was written with, or re-sync with `--force-reset-beacon-db` if that is not known. Databases from released versions hold only full snapshots, which no other state is encoded against, so they are adopted silently.

#### State caches

`--state-cache-sizes` sets how many states are kept in memory per hierarchy layer, so that repeated reads and delta computations do not have to go back to disk. It takes a comma separated list starting from the shallowest layer — the full state snapshot — in the same order `--state-hierarchy` exponents are listed in. The list may be shorter than the hierarchy; the layers it does not name are not cached. A size of `0` disables caching for that layer too. When the flag is not given, the sizes default to `5,3,3`, so changing `--state-hierarchy` alone does not require setting this too.

Shallow layers hold full or near-full states, so raising their sizes costs considerably more memory per cached entry than raising the deeper ones.

Independently of `--state-cache-sizes`, Grandine keeps the hierarchy ancestors of the most recently persisted state in memory — at most one state per layer, seven at the default hierarchy — so that forward sync can delta-encode without reading them back from disk. This is a floor: setting a layer's cache size to `0` disables its read cache but does not drop that state. Adding layers to `--state-hierarchy` therefore also raises baseline memory usage.

`--state-compression-level` sets the zstd compression level used for both snapshots and deltas (default: `3`). Higher levels shrink the database at the cost of CPU time on every state write. The level must be one zstd accepts; Grandine refuses to start otherwise.

### Archive Mode

Grandine provides `--archive-storage` option for archive mode, which disables pruning: blocks, blob sidecars, data columns and stored states are kept instead of being deleted once they fall outside the retention window. States are still written at the frequency defined by `--state-hierarchy`. This mode is mutually exclusive with `--prune-storage`.

### Prune Mode

Grandine provides `--prune-storage` option for prune mode that only stores a single checkpoint state with the corresponding block. This mode also stores unfinalized blocks on Grandine shutdown. This mode is sufficient for staking. No states are written to the hierarchy in this mode, so `--state-hierarchy` has no effect.

### Metrics

Two Prometheus histograms report on the delta state store. Both are labelled by `layer`, the number of deltas that have to be applied to reconstruct the state: `0` is a full snapshot, `1` is a delta against a snapshot, and so on. This is normally the state's depth in the hierarchy, but it is shorter when a hierarchy ancestor was missing and the state had to be encoded against a shallower one.

* `STATE_PATCH_SIZES` - serialized and compressed size in bytes of states written to the store. Snapshots are recorded here too, under layer `0`, so the buckets span from kilobyte-sized deltas to snapshots hundreds of megabytes large;
* `STATE_PATCH_COMPUTE_TIMES` - time in seconds spent computing a delta against its hierarchy ancestor.

Together they show how much space and CPU each layer costs, which is what `--state-hierarchy` and `--state-compression-level` trade against each other.

### Relevant command line options

* `--state-hierarchy` - comma separated list of slot exponents defining the state storage layout, deepest layer first (default: `5,9,11,13,16,18,21`);
* `--state-cache-sizes` - number of states cached in memory per hierarchy layer, shallowest layer first; may be shorter than the hierarchy (default: `5,3,3`);
* `--state-compression-level` - zstd compression level for stored states (default: `3`);
* `--archive-storage` - retains all blocks, blobs and stored states by disabling pruning; mutually exclusive with `--prune-storage` (default: disabled);
* `--force-reset-beacon-db` - deletes the existing beacon node databases on startup, which is the escape hatch for a `--state-hierarchy` mismatch (default: disabled);
* `--prune-storage` - enables pruning mode that doesn't store historical states and blocks (default: disabled);
* `--state-slot` - sets the slot at which Grandine Beacon Node should start (default: latest finalized slot);
* `--unfinalized-states-in-memory` - the number of the latest slots that will store states in the memory (default: all unfinalized states stored in the memory);
* `--archival-epoch-interval` - **deprecated and ignored**; use `--state-hierarchy` instead.
