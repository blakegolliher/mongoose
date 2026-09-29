# Building and releasing mongoose

## Build

Requires Rust 1.91+ and libnfs. Release builds always use the exact static
`libnfs.a` pinned by `packaging/libnfs.lock.json`; ordinary host builds may use
the system library through pkg-config.

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

Build the reproducible `libnfs.a` from a clean checkout at the pinned revision,
or point `LIBNFS_STAGE` at an already verified stage:

```bash
make libnfs-stage LIBNFS_SOURCE=/path/to/libnfs
```

The build maps three absolute paths to fixed names: the libnfs checkout, its
build directory, and Zig's `lib` directory, which holds the libc and compiler
headers. The archive, debug info included, therefore does not depend on where
the checkout or Zig lives. With the tool versions below, it must match
`static_artifact_sha256` in `packaging/libnfs.lock.json` byte for byte.
`make libnfs-stage`, `make release`, and CI all fail on any other digest. To
repeat the two-path check, build twice with `ZIG` pointing at copies of the
same Zig installed in two different directories, then `cmp` the two archives.

Evidence (2026-09-29; Zig 0.16.0, CMake 3.28.3, GNU ar/ranlib 2.42): the
pinned source was built with Zig at three absolute paths. These were the snap
at `/snap/zig/16117`, and the official tarball, byte-identical to the snap, at
two other directories. A shallow `git fetch` of the source in a separate
directory was also built. All four archives were byte-identical, SHA-256
`36822790290a78787cc4e8f029808d2eeef0bf62beb192bc49ec4e369ea666f0`. Before the
Zig directory was mapped, the three Zig locations gave three different digests.

The exact Rust, cargo-zigbuild, Zig, CMake, binutils, tar, gzip, and
cargo-about versions used for releases are recorded in
`packaging/release-toolchain.lock.json`.

```bash
VAMOOSE_LIBNFS_DIR=/path/to/stage \
NFS_WALKER_LIBNFS_DIR=/path/to/stage \
CARGO_ZIGBUILD_ZIG_PATH=/path/to/zig \
cargo zigbuild --release --target x86_64-unknown-linux-gnu.2.34 -p mongoose
```

## Packaging

`make release` produces, under `dist/`:

| artifact | contents |
|---|---|
| `mongoose-linux-x86_64` | the bare binary |
| `mongoose-<ver>-linux-x86_64.tar.gz` | binary, man page, README, licenses, notices, source pointer |
| `mongoose-<ver>-1.x86_64.rpm` | binary, man page, licenses, notices, source pointer |
| `mongoose_<ver>-1_amd64.deb` | binary, man page, licenses, notices, source pointer |
| `mongoose-<ver>-source.tar.gz` | exact mongoose and pinned nfs-walker source |
| `libnfs-<revision>-source.tar.gz` | complete source of the statically linked libnfs revision |
| `mongoose-<ver>-relink-kit.tar.gz` | source, vendored Cargo dependencies, build scripts, offline relink procedure |
| `LICENSES.txt`, `THIRD_PARTY_LICENSES.md`, `LIBNFS_SOURCE.md` | bare-binary companion notices |
| `RELINK-VERIFICATION.txt` | successful modified-libnfs build/test/relink evidence |
| `SHA256SUMS` | digests of every published release asset |

Packages and the tarball are portable by default. `make rpm PORTABLE=0`
packages a host build instead; do not ship those. A full release also requires
clean local checkouts of libnfs and nfs-walker at their locked revisions,
`cargo-about`, `cmake`, `rpmbuild`, `rpm`, `dpkg-deb`, `jq`, `objdump`,
`readelf`, `cargo-zigbuild`, and Zig.

## Cutting a release

1. Bump `version` in `Cargo.toml`, the `.TH` line in
   `packaging/mongoose.1`, and add a `%changelog` entry to
   `packaging/mongoose.spec`.
2. Put clean libnfs and nfs-walker checkouts at the revisions in their lock
   files. By default the Makefile expects sibling `../libnfs` and
   `../nfs-walker` directories; override `LIBNFS_SOURCE` and
   `NFS_WALKER_SOURCE` when needed.
3. `cargo test -p mongoose`, then `make release`. The release command builds
   the companion source and relink assets, performs an offline smoke test and
   portable relink with a deliberately modified libnfs, inspects all package
   contents, and fails closed before writing checksums if any compliance
   requirement is absent.
4. Commit, tag `vX.Y.Z`, push both.
5. Upload every path recorded by `dist/SHA256SUMS`, plus `SHA256SUMS` itself.
   Never publish only the executable or packages.

The release generator refuses a dirty source tree so that the archives and
evidence always describe the committed release. The relink kit's
`RELINKING.md` and `verify-relink.sh` are the recipient-facing procedure.

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
