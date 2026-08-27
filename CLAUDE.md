## Testing code

To run whole test suite, use `make test`. Keep in mind that running whole test
suite spawns a lot of threads, and uses lots of memory, so you probably want to
limit parallelism or memory usage, to avoid crashing.

If you want to run tests partially, use cargo test. However, the `bls` crate
needs at least one feature turned on, to enable some BLS backend, and usually
tests will fail with obscure errors. To avoid this, specify bls backend, e.g.:

```
cargo test -p diff -p bls --features bls/blst
```

`kzg_utils` has no default backend either, so crates that depend on it need one
too, e.g.:

```
cargo test -p fork_choice_control -p bls -p kzg_utils --features bls/blst,kzg_utils/blst
```

## Benchmarks

The `diff` benches run against real mainnet states, which they download from an
archival beacon node. Set `REMOTE_URL` to its debug-state endpoint, with
`{slot}` as a placeholder:

```
REMOTE_URL='http://<node>:5052/eth/v2/debug/beacon/states/{slot}' \
    cargo bench -p diff --bench beacon_state
```

Downloaded states are cached in `diff/benches/assets` (gitignored) and reused.
The `comparison` bench measures our encoder against third-party ones
(`eth-state-diff`, `qbsdiff` and `xdelta3`, declared only in `diff/Cargo.toml`):

```
REMOTE_URL='...' cargo bench -p diff --bench comparison
```

`xdelta3` is built from source, so a C toolchain is needed to build any `diff`
dev-dependency, including its tests and benches.

## Code style

Don't use one-off helpers. Don't overabstract things - keep it simple. Don't use
comments too much - use them wisely, either to explain _why_ this is needed, or
to document public API (only in cases API isn't self-descriptive, or has some
quirks that need to be accounted). Never comment historical changes - something
like "this was previously X, now we replace it to Y".
