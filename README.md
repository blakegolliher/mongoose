# mongoose

Single-host NFS-to-NFS data mover: scan the source with an embedded
parallel walker, build a canonical parquet index, copy over raw NFSv3
with libnfs, then resync incrementally while the source stays live and
finish with a verified cutover. One static-leaning binary, no external
tools, no coordinator, no S3.

The full CLI walkthrough — `prepare` / `copy` / `run` / `sync
[--cutover]`, work-dir layout, resume semantics, correctness posture,
and v1 limitations — lives in
[`crates/mongoose/README.md`](crates/mongoose/README.md). The resync
design (change classification, convergence argument, deletion policy)
is [`docs/work-items/MONGOOSE_RESYNC.md`](docs/work-items/MONGOOSE_RESYNC.md).

## Crates

| crate | role |
|---|---|
| `mongoose` | the CLI: stages, work dir, delta emission, sync driver |
| `migration-resync` | hash-partitioned scan-vs-scan change classifier |
| `migration-mover` | libnfs data path: context pools, raw-FH fast path, failure/downgrade records |
| `migration-worker` | shard processor reused as the copy engine (batching, inflight limits, hardlink groups, dir attrs) |
| `migration-core` | canonical shard schema, records, shared invocation of the walker/rewrite |
| `mig-walker-rewrite` | walker output → canonical shards, as a library |
| `migration-coord`, `migration-control-protocol` | transitive dependencies of the shard processor; mongoose runs no coordinator |

The scanner is [nfs-walker](https://github.com/blakegolliher/nfs-walker),
compiled in as a library and pinned in `crates/mongoose/Cargo.toml`
(the same commit `packaging/nfs-walker.lock.json` pins).

## Build

Requires libnfs (static `libnfs.a` preferred; see
`crates/migration-mover/build.rs`). `packaging/libnfs.lock.json` pins
the known-good libnfs source commit.

```bash
cargo build --release -p mongoose
```

Cross-building for an older glibc (e.g. 2.34 targets) with
`cargo-zigbuild`: build libnfs.a from the pinned source with
`zig cc -target x86_64-linux-gnu.2.34`, stage it in a directory, and
point both link overrides at it:

```bash
VAMOOSE_LIBNFS_DIR=/path/to/stage \
NFS_WALKER_LIBNFS_DIR=/path/to/stage \
cargo zigbuild --release --target x86_64-unknown-linux-gnu.2.34 -p mongoose
```

x86_64 builds need `-C target-feature=+aes,+sse2` (the walker's gxhash
dependency); the workspace `.cargo/config.toml` sets it, but an
exported `RUSTFLAGS` overrides that file — keep the flag if you set
your own.

## Packaging

`make release` produces an RPM, a DEB, and a tarball under `dist/`,
each installing the binary and the `mongoose(1)` man page. Release
artifacts are **portable by default**: built with `cargo-zigbuild`
against glibc 2.34 using a libnfs stage sha256-verified against
`packaging/libnfs.lock.json` (place the pinned `libnfs.a` +
`libnfs.so` pair in `packaging/libnfs-stage/`, or point
`LIBNFS_STAGE` elsewhere), then gated on the binary's maximum
`GLIBC_*` symbol version. `make rpm PORTABLE=0` packages a host build
instead — don't ship those.

## Provenance

The engine crates are extracted from the vamoose distributed-migration
workspace; mongoose is the single-host packaging of that engine with
S3, claims, fleet coordination, and the TUI removed, plus the resync
layer on top.
