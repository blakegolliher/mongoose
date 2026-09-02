# Building and releasing mongoose

## Build

Requires Rust 1.91+, and libnfs (static `libnfs.a` preferred; see
`crates/migration-mover/build.rs`). `packaging/libnfs.lock.json` pins
the known-good libnfs source commit.

```bash
cargo build --release -p mongoose
```

x86_64 builds need `-C target-feature=+aes,+sse2` (the walker's gxhash
dependency); the workspace `.cargo/config.toml` sets it, but an
exported `RUSTFLAGS` overrides that file, so keep the flag if you set
your own.

The scanner is [nfs-walker](https://github.com/blakegolliher/nfs-walker),
compiled in as a library and pinned in `crates/mongoose/Cargo.toml`
(the same commit `packaging/nfs-walker.lock.json` pins).

## Portable build (what the release ships)

Release artifacts are built with `cargo-zigbuild` against glibc 2.34
using a libnfs stage sha256-verified against
`packaging/libnfs.lock.json`, then gated on the binary's maximum
`GLIBC_*` symbol version. They run on any x86_64 Linux with glibc
2.34 or newer and depend only on libc and libm.

Build `libnfs.a` and `libnfs.so` from the pinned source with
`zig cc -target x86_64-linux-gnu.2.34`, place the pair in
`packaging/libnfs-stage/` (gitignored), or point `LIBNFS_STAGE` at an
existing stage. The toolchain versions used for releases are recorded
in `packaging/release-toolchain.lock.json`.

```bash
VAMOOSE_LIBNFS_DIR=/path/to/stage \
NFS_WALKER_LIBNFS_DIR=/path/to/stage \
cargo zigbuild --release --target x86_64-unknown-linux-gnu.2.34 -p mongoose
```

## Packaging

`make release` produces, under `dist/`:

| artifact | contents |
|---|---|
| `mongoose-linux-x86_64` | the bare binary |
| `mongoose-<ver>-linux-x86_64.tar.gz` | binary, man page, README, LICENSE |
| `mongoose-<ver>-1.x86_64.rpm` | binary + man page |
| `mongoose_<ver>-1_amd64.deb` | binary + man page |
| `SHA256SUMS` | digests of the above |

Packages and the tarball are portable by default. `make rpm PORTABLE=0`
packages a host build instead; do not ship those. Requires `rpmbuild`,
`dpkg-deb`, `jq`, `objdump`, `cargo-zigbuild`, and `zig`.

## Cutting a release

1. Bump `version` in `Cargo.toml`, the `.TH` line in
   `packaging/mongoose.1`, and add a `%changelog` entry to
   `packaging/mongoose.spec`.
2. `cargo test -p mongoose`, then `make release`.
3. Commit, tag `vX.Y.Z`, push both.
4. `gh release create vX.Y.Z dist/mongoose-linux-x86_64 dist/*.tar.gz dist/*.rpm dist/*.deb dist/SHA256SUMS --title "mongoose X.Y.Z" --notes-file <notes>`

The README's install snippet fetches
`releases/latest/download/mongoose-linux-x86_64`, so the bare-binary
asset name must stay exactly that.

## Workspace layout

| crate | role |
|---|---|
| `mongoose` | the CLI: `copy` and `sync`, work dir, delta emission |
| `migration-resync` | hash-partitioned scan-vs-scan change classifier |
| `migration-mover` | libnfs data path: context pools, raw-FH fast path, failure/downgrade records |
| `migration-worker` | shard processor reused as the copy engine (batching, inflight limits, hardlink groups, dir attrs) |
| `migration-core` | canonical shard schema, records, shared invocation of the walker/rewrite |
| `mig-walker-rewrite` | walker output to canonical shards, as a library |
| `migration-coord`, `migration-control-protocol` | transitive dependencies of the shard processor; mongoose runs no coordinator |

The engine crates are extracted from the vamoose distributed-migration
workspace; mongoose is the single-host packaging of that engine with
S3, claims, fleet coordination, and the TUI removed, plus the resync
layer on top. The engine exposes many more knobs than mongoose does
(context pairs, per-size-class inflight limits, raw-FH vs path-based
copy, commit mode, RPC timeouts, shard sizing); mongoose fixes all of
them (see `crates/mongoose/src/copy.rs`, `mover_params`) and exposes
only `--parallel`.
