## Storage

### Memory

By default, Grandine keeps the non-finalized part of the chain in the memory using structural sharing. This approach contributes to the high performance of Grandine because full state copies are avoided. This is a perfect approach for healthy chains (such as Ethereum Mainnet) that don't experience very long non-finalization periods. In such conditions, Grandine uses only ~1GB of memory on the Mainnet. However, during long non-finalization, this approach increases memory usage. In such cases, Grandine allows limiting the number of the latest memory stored states by settings the maximum number of the latest slots that should keep states in the memory.

### Disk

Grandine stores finalized part of the chain in the disk using an embedded key-value database `libmdbx`. Disk storage is passive and mainly used for storing/loading checkpoints, and serving historical data via API. Historical blocks and corresponding intermediate states are stored on the disk. It's possible to set the length of the intermediate states period. A higher value for this interval means lower disk usage and slower API responses for historical data.

Grandine allows starting the Beacon Node from an earlier stored checkpoint by using `--state-slot` option. In this case, Grandine will try to find and load from the disk the closest stored checkpoint before the specified `--state-slot`. This requires the block at that checkpoint to still carry its execution payload, so `--state-slot` only reaches past the latest checkpoint when the node runs with `--store-payloads`; without it Grandine warns and starts from the latest stored checkpoint instead.

### Execution payloads

Execution payloads (transactions and withdrawals) make up most of a block, and the execution client already stores them. Grandine therefore does not store them by default: finalized blocks that carry an inline payload (Bellatrix through Fulu) are stored blinded, with the payload replaced by its header. This applies to the default and `--archive-storage` modes; `--prune-storage` stores no finalized blocks at all, so the setting makes no difference there. Payload storage can be turned back on with `--store-payloads`.

The setting only takes effect as blocks are written. Enabling it on an existing database does not re-fetch the payloads of blocks already stored blinded, and disabling it does not reclaim the space taken by payloads already on disk — that space is only released by normal pruning. Blinded blocks are also stored under a key that older Grandine versions do not recognise, so a version without this feature fails to load them and refuses to start. Downgrading therefore requires resetting the beacon database.

Blinded storage is invisible to consumers. Blocks served to peers over `BeaconBlocksByRange` and `BeaconBlocksByRoot`, and blocks served by the HTTP API block endpoints, are reconstructed on demand by asking the execution client for the payload bodies of the stored payload headers. Endpoints that only need the block header, such as the blob endpoints and the validator statistics, are served straight from the blinded block. Historical state replay doesn't need payloads at all and never contacts the execution client.

The trade-off is that serving historical blocks depends on the execution client. Reconstruction uses the `engine_getPayloadBodiesByHashV1` and `engine_getPayloadBodiesByRangeV1` engine methods, so the execution client must support both of them (Grandine logs a warning at startup if it doesn't) and must still retain the bodies of the blocks being requested. An aggressively pruned execution client will make old blocks unavailable over both the p2p and the HTTP interfaces, and a node whose execution client is unreachable cannot serve historical blocks at all. Run with `--store-payloads` if the beacon node has to serve historical blocks independently of the execution client.

The `grandine export` subcommand writes full blocks and so requires `--store-payloads`; exporting a range that contains blinded blocks fails.

### Prune Mode

Grandine provides `--prune-storage` option for prune mode that only stores a single checkpoint state with the corresponding block. This mode also stores unfinalized blocks on Grandine shutdown. This mode is sufficient for staking.

### Relevant command line options

* `--archival-epoch-interval` - sets the number of epochs between stored states (default: `32`);
* `--prune-storage` - enables pruning mode that doesn't store historical states and blocks (default: disabled);
* `--state-slot` - sets the slot at which Grandine Beacon Node should start (default: latest finalized slot);
* `--store-payloads` - stores execution payloads in the database instead of storing finalized blocks blinded and reconstructing payloads from the execution client (default: disabled);
* `--unfinalized-states-in-memory` - the number of the latest slots that will store states in the memory (default: all unfinalized states stored in the memory).
