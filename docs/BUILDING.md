# Building and releasing mongoose

## Build

Requires Rust 1.91+ and the exact static `libnfs.a` pinned by
`packaging/libnfs.lock.json`. Distro libnfs packages are not supported: some
pass a version check but omit the raw NFSv3 task symbols mongoose uses.

From a fresh clone, fetch the pinned libnfs source and build its verified
archive first. The archive build requires Zig 0.16.0 and CMake; it uses the
`ar` and `ranlib` subcommands supplied by that pinned Zig toolchain so host
binutils versions do not change the archive. The lock file remains the source
of truth for the repository and revision:

```bash
libnfs_url=$(jq -r .source_url packaging/libnfs.lock.json)
libnfs_sha=$(jq -r .source_git_sha packaging/libnfs.lock.json)
git init -q ../libnfs
git -C ../libnfs fetch --depth 1 "$libnfs_url" "$libnfs_sha"
git -C ../libnfs checkout --detach FETCH_HEAD
make libnfs-stage LIBNFS_SOURCE=../libnfs
make
```

`make` verifies the staged archive against the lock before compiling and sets
both native consumers to link it statically. A bare `cargo build` is rejected
unless `VAMOOSE_LIBNFS_DIR` and `NFS_WALKER_LIBNFS_DIR` name that stage.

```bash
./target/release/mongoose --version
ldd ./target/release/mongoose
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

Path evidence (2026-09-29; Zig 0.16.0 and CMake 3.28.3): the
pinned source was built with Zig at three absolute paths. These were the snap
at `/snap/zig/16117`, and the official tarball, byte-identical to the snap, at
two other directories. A shallow `git fetch` of the source in a separate
directory was also built. All four archives were byte-identical, SHA-256
`36822790290a78787cc4e8f029808d2eeef0bf62beb192bc49ec4e369ea666f0`. Before the
Zig directory was mapped, the three Zig locations gave three different digests.
Switching archive creation from host GNU ar/ranlib to Zig's pinned LLVM
ar/ranlib preserved that digest in two independent builds.

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
| `mongoose-<ver>-sbom.cdx.json` | CycloneDX 1.5 SBOM of the binary: its Cargo crates, plus the statically linked libnfs |
| `SHA256SUMS` | digests of every asset above |
| `SHA256SUMS.sigstore.json` | Sigstore signature bundle for `SHA256SUMS`; added by the release workflow, not by `make release` |

Packages and the tarball are portable by default. `make rpm PORTABLE=0`
packages a host build instead; do not ship those. Package and asset
architecture labels come from the build target, not from the host. A full
release also requires:

- clean local checkouts of libnfs and nfs-walker at their locked revisions;
- `cargo-about`, `cmake`, `rpmbuild`, `rpm`, `dpkg-deb`, `jq`, `objdump`,
  `readelf`, `cargo-zigbuild`, and Zig, at exactly the versions in
  `packaging/release-toolchain.lock.json`. `make toolchain-check` verifies them,
  and every portable build runs that check first;
- podman, with network access to pull the digest-pinned test images and
  qemu-user.

## Cutting a release

Releases are built, signed, and published by `.github/workflows/release.yml`,
not on a workstation. `make release` produces the same artifacts locally as a
rehearsal, but nothing it builds is published.

1. Bump `version` in `Cargo.toml`, the `.TH` line in
   `packaging/mongoose.1`, and add a `%changelog` entry to
   `packaging/mongoose.spec`. Merge that to `main` through a pull request.
2. Tag that commit on `main` as `vX.Y.Z` and push the tag. The workflow
   refuses to continue unless all of these hold:
   - the tag, the Cargo version, and the checked-out commit agree;
   - the commit is on `main`;
   - no release exists for the tag yet.
3. The **build** job sets up the locked toolchain, including
   `make toolchain-check`, fetches the pinned libnfs and nfs-walker sources,
   and runs `make release`. That builds every artifact once, generates and
   validates the SBOM, writes `SHA256SUMS` over all of them, and then runs
   the LGPL gate and `scripts/check-release-artifacts.sh`. It then freezes
   exactly the files `SHA256SUMS` lists, and hands them to the later jobs
   with the digest of `SHA256SUMS`.
4. The **sign** job re-verifies those bytes, then does three things:
   - attests build provenance for every file and for `SHA256SUMS`;
   - attests the SBOM for the binary;
   - signs `SHA256SUMS` with Sigstore (cosign, keyless).
5. The **verify** job runs `scripts/verify-release.sh`, the procedure in
   "Verifying a release" below, on a fresh runner.
6. The **publish** job waits for the owner's approval in the protected
   `release` environment. After approval, it re-verifies the bytes once more
   and creates the GitHub release from exactly those files plus
   `SHA256SUMS.sigstore.json`.

Nothing is rebuilt or regenerated between the gates, signing, and
publication. A manual run of the workflow, or a pull request that touches the
release machinery, is a dry run: it builds, gates, freezes, and checks the
hand-off. It gets read-only permissions, and never signs, attests, or
publishes.

`make release` refuses a dirty source tree, so the archives and evidence
always describe a committed revision. The relink kit's `RELINKING.md` and
`verify-relink.sh` are the recipient-facing LGPL procedure.

The README's install snippet fetches
`releases/latest/download/mongoose-linux-x86_64`, so the bare-binary
asset name must stay exactly that.

## Verifying a release

On any machine, with [cosign](https://github.com/sigstore/cosign) v3 and the
GitHub CLI 2.49 or newer (logged in, or with `GH_TOKEN` set), download the
release assets into an empty directory, then run the following, replacing
`vX.Y.Z` with the release tag:

```sh
tag=vX.Y.Z
repo=blakegolliher/mongoose

# SHA256SUMS was signed by this repository's release workflow, for this tag.
cosign verify-blob SHA256SUMS --bundle SHA256SUMS.sigstore.json \
  --certificate-identity "https://github.com/$repo/.github/workflows/release.yml@refs/tags/$tag" \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com

# Every asset matches SHA256SUMS.
sha256sum --check --strict SHA256SUMS

# Each asset was built by that workflow, at that tag, on a GitHub-hosted runner.
gh attestation verify mongoose-linux-x86_64 --repo "$repo" \
  --signer-workflow "$repo/.github/workflows/release.yml" \
  --source-ref "refs/tags/$tag" --deny-self-hosted-runners

# The published SBOM is the one attested for that binary.
gh attestation verify mongoose-linux-x86_64 --repo "$repo" \
  --signer-workflow "$repo/.github/workflows/release.yml" \
  --source-ref "refs/tags/$tag" --predicate-type https://cyclonedx.org/bom
```

`scripts/verify-release.sh --tag vX.Y.Z --dir DIR` runs all of these,
checking provenance for every asset. The release workflow runs that script on
a fresh runner before anything is published.

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
