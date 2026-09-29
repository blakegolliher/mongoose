# PR-11A handoff — green workspace and baseline CI

Status: **ready for implementation** (amended 2026-09-29: CI links the pinned
libnfs fork; minimal worker fixture specified)

Suggested implementer: Luna or another lower-cost implementation model, with
human review of the workflow permissions and final required-check settings.

This is the first, deliberately narrow slice of PR-11 in
`docs/PRODUCTION_RELEASE_HANDOFF.md`. It makes the checked-in workspace green
from a clean clone and runs that baseline on every pull request. It does not
claim that mongoose is ready to release.

## Outcome

Land one focused pull request that:

1. adds the minimal worker configuration fixture that two existing tests
   require;
2. makes `cargo test --workspace --locked` pass from a clean clone;
3. builds the pinned libnfs fork for CI and links every Cargo gate against it;
4. adds fail-closed CI for formatting, Clippy, workspace tests, shell scripts,
   and the repository-level LGPL policy; and
5. uses and verifies the Rust and Cargo versions recorded in
   `packaging/release-toolchain.lock.json`.

The workflow must never publish artifacts or describe its output as a release.
The libnfs archive it builds is disposable test input: it is never uploaded,
packaged, or compared with the locked release digest. Release artifact
construction, the modified-libnfs relink proof, package inspection, signing,
and hardware qualification remain separate gates.

## Owner prerequisites

- **nfs-walker PR #10.** mongoose pins nfs-walker commit `2dded4c`, which
  currently exists only on the `embed-libnfs-override` branch of open PR #10.
  Merge PR #10 with a regular merge commit (not squash or rebase) before
  deleting that branch, so the pinned commit stays permanently reachable.
  Otherwise the pin in `crates/mongoose/Cargo.toml` and
  `packaging/nfs-walker.lock.json` must be updated.
- **Branch protection is not an implementation blocker.** Land a green
  workflow first; the owner then marks the resulting check required on `main`.

## Verified starting state

The following was rechecked after production-hardening PR #1 merged on
2026-09-29, on the development host and in a clean `ubuntu:24.04` container:

- There is no checked-in `.github/workflows/` directory.
- There is no `examples/worker.toml`, and none exists anywhere in this
  repository's history.
- `cargo test --workspace --locked` compiles the workspace and then fails in
  exactly these existing tests:
  - `config::tests::examples_worker_toml_parses`
  - `config::tests::canonical_config_with_cli_only_sections_still_parses_for_mig_worker`

  A plain run stops at that first failing test binary, so the `mongoose`
  crate's tests and all doc-tests do not run. A diagnostic `--no-fail-fast`
  run confirmed there are no other failures.
- Both failures are reads of the missing `examples/worker.toml`; do not delete,
  ignore, or weaken the tests to make the command green. With the fixture from
  Scope §1 in place, the full suite passed locally: 732 passed, 0 failed,
  28 ignored.
- Tests in `migration-coord` bind a loopback listener on `127.0.0.1:0`. They
  pass with normal host permissions and must not be skipped in CI.
- `cargo fmt --all -- --check`, Clippy with warnings denied, ShellCheck, and
  `make compliance-check` pass.
- The release toolchain lock records Rust and Cargo 1.98.0 and Zig 0.16.0.
- **Distro libnfs cannot link the workspace.** With Ubuntu 24.04's
  `libnfs-dev` 5.0.2, Clippy passes but the `migration-mover` and `mongoose`
  library test binaries fail to link. Nine `rpc_nfs3_*_task` symbols
  (readdirplus, lookup, read, write, commit, create, mkdir, rename, setattr)
  required by mongoose are absent from that distro package and present in the
  pinned fork. The development host works only because `/usr/local` carries a
  fork build. Linking the pinned static archive through `VAMOOSE_LIBNFS_DIR`
  and `NFS_WALKER_LIBNFS_DIR`, with no system libnfs installed, makes Clippy
  and every test link; the only failures left are the two fixture tests above.
- **The locked archive digest is path-dependent.** Rebuilding the locked
  libnfs source with the locked tool versions in a clean container produced a
  different `libnfs.a` SHA-256 from `packaging/libnfs.lock.json`. The machine
  code is identical; Zig's installation path leaks into debug metadata. PR-11C
  owns that fix. PR-11A must not enforce, change, or re-pin the digest.
- mongoose, nfs-walker, and the libnfs fork are public repositories, so CI
  needs no secrets or deploy keys.

The broader PR-11 security audit is not green. As of the same date,
`cargo deny check advisories` reports five vulnerabilities (`h2`, `rustls`, and
three `rustls-webpki` advisories) plus the unmaintained `number_prefix` and
`paste` crates. Those findings belong to PR-11B and must not be waived in this
PR.

## Scope

### 1. Add the minimal worker fixture

Create `examples/worker.toml` with exactly these keys:

```toml
[run]
bucket = "vamoose"
endpoint = "https://s3.example.com"
region = "us-east-1"

[mover]
src_url = "nfs://source.example.com/export"
dst_url = "nfs://destination.example.com/export"
```

Leave `profile`, `[coord]`, and any insecure TLS setting absent. The remaining
values the tests check (verified TLS, strategy label, pipeline depth, io_uring
queue depth, fixed buffer count and size, server-side copy `off`, and the
60000 ms RPC timeout) come from Rust defaults; do not restate them. Do not
substitute real hosts, credentials, private addresses, or environment-specific
paths, and do not copy the larger vamoose `worker.toml`, which documents
vamoose-only commands and paths.

Do not move the fixture into the test module.
`crates/migration-mover/MANUAL_VERIFY.md` refers operators to it.

### 2. Build the pinned libnfs fork for CI

Use the pinned static libnfs fork for CI; do not use the distro `libnfs-dev`.

1. Read `source_url` and `source_git_sha` from `packaging/libnfs.lock.json`.
2. Fetch that exact commit into `$RUNNER_TEMP/libnfs` and fail unless
   `HEAD` equals the locked SHA. GitHub serves a shallow fetch by commit SHA:

   ```bash
   url=$(jq -r .source_url packaging/libnfs.lock.json)
   sha=$(jq -r .source_git_sha packaging/libnfs.lock.json)
   git init -q "$RUNNER_TEMP/libnfs"
   git -C "$RUNNER_TEMP/libnfs" fetch --depth 1 "$url" "$sha"
   git -C "$RUNNER_TEMP/libnfs" checkout -q FETCH_HEAD
   test "$(git -C "$RUNNER_TEMP/libnfs" rev-parse HEAD)" = "$sha"
   ```

3. Install and verify Zig at the version in
   `packaging/release-toolchain.lock.json` (`.zig`, currently 0.16.0). Verify
   the download against a SHA-256 committed in the workflow, or use a setup
   action pinned by full commit SHA. The build script also refuses any other
   `zig version`.
4. Build the CI-only archive with the unmodified release script:

   ```bash
   EXPECTED_ZIG_VERSION="$(jq -r .zig packaging/release-toolchain.lock.json)" \
   ZIG=/path/to/zig \
   ./scripts/build-libnfs-static.sh \
     --source "$RUNNER_TEMP/libnfs" \
     --output "$RUNNER_TEMP/libnfs-stage"
   ```

   The script also needs CMake (libnfs requires 3.16 or newer), GNU `ar`, and
   `ranlib`. Printing the resulting archive's SHA-256 is fine as information.
5. Export these for every Cargo gate, at job level or through `$GITHUB_ENV`,
   then run Clippy and tests:

   ```bash
   VAMOOSE_LIBNFS_DIR="$RUNNER_TEMP/libnfs-stage"
   NFS_WALKER_LIBNFS_DIR="$RUNNER_TEMP/libnfs-stage"
   ```

Do not pass `EXPECTED_LIBNFS_SHA256` in PR-11A. The disposable CI archive
varies because Zig's installation path leaks into debug metadata, and it is
never published. The real `make release` path must remain unchanged and must
continue enforcing the locked archive digest and the full LGPL gate. Do not
modify `scripts/build-libnfs-static.sh`, the Makefile's `libnfs-stage`,
`stage-check`, or release targets, `packaging/libnfs.lock.json`, or
`scripts/check-lgpl-compliance.sh`.

Caching the built archive is optional. If it is cached, key the cache on the
locked libnfs SHA and Zig version, SHA-pin the cache action, and make sure
correctness never depends on a warm cache.

### 3. Add the baseline GitHub Actions workflow

Add `.github/workflows/ci.yml`. It must run for:

- every pull request; and
- pushes to `main`.

Run on `ubuntu-24.04` rather than `ubuntu-latest`, so runner-image upgrades
happen by review. Use least-privilege workflow permissions (`contents: read`)
and a concurrency group that cancels an obsolete run for the same branch or
pull request. Give every job a finite timeout. Pin every action referenced by
`uses:` to a full immutable commit SHA and leave a comment naming its
human-readable release version.

The workflow may use one job or a small number of jobs, but the required gates
must be obvious in the GitHub UI. A single Linux validation job is acceptable
for this slice.

The runner must provide `jq`, `shellcheck`, CMake, Zig (Scope §2), and GNU
binutils. Do not install `libnfs-dev`. Do not build, upload, or cache a release
binary in this workflow. A Cargo compilation cache is optional; correctness
must not depend on a warm cache, and any cache action must also be SHA-pinned.
A cold workspace build taking several minutes is expected.

Read the expected Rust and Cargo versions from
`packaging/release-toolchain.lock.json`. Install that exact Rust toolchain with
the `rustfmt` and `clippy` components, select it for all later steps, and fail
if `rustc --version` or `cargo --version` does not match the lock. Do not
duplicate an unverified version constant only in the workflow.

With both libnfs variables from Scope §2 set, run these gates without
`continue-on-error`:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
shellcheck scripts/*.sh packaging/relink-kit/*.sh
make compliance-check
```

The workspace tests intentionally exclude tests already marked `#[ignore]`.
Do not add new ignores, package exclusions, `--no-fail-fast`, retry wrappers,
or filters to conceal a deterministic failure. Normal Cargo parallelism is
acceptable.

`make compliance-check` is the fast repository-policy check. It is not the
release artifact gate and must not be replaced with a mocked or reduced
command. The repository's static-libnfs rules in `AGENTS.md` and
`docs/LGPL_COMPLIANCE.md` remain controlling.

### 4. Update the handoff status

After the implementation and hosted workflow are green, update the PR-11
section of `docs/PRODUCTION_RELEASE_HANDOFF.md` to record:

- the PR-11A implementation commit or pull request;
- the exact CI commands now enforced, and that CI links the pinned libnfs fork;
- that the missing-example workspace-test failure is closed; and
- that PR-11B, PR-11C, and real-NFS hardware qualification remain release
  blockers.

Do not change the top-level `Status: release blocked` line in that document.

## Required implementation sequence

1. Create `examples/worker.toml`.
2. Run the two previously failing `migration-worker` tests directly.
3. Build the CI-only libnfs archive as in Scope §2, using a temporary
   directory outside the repository in place of `$RUNNER_TEMP`, and export
   both libnfs variables in your shell.
4. Run the full local acceptance command set below.
5. Add the CI workflow using the same commands.
6. Push the branch, open the pull request, and observe a hosted run from a
   clean GitHub runner.
7. Run the negative tests on throwaway draft pull requests. The workflow runs
   only for pull requests and pushes to `main`, so pushing a bare branch
   triggers nothing. Close each throwaway PR and delete its branch afterwards.
8. Fix root causes of any hosted-only failure; do not weaken a gate.
9. Update the PR-11 handoff status only after the hosted run is green.

## Local acceptance

Run from the repository root with no uncommitted generated fixtures, with
`VAMOOSE_LIBNFS_DIR` and `NFS_WALKER_LIBNFS_DIR` exported as in step 3 above:

```bash
cargo test --locked -p migration-worker \
  config::tests::examples_worker_toml_parses
cargo test --locked -p migration-worker \
  config::tests::canonical_config_with_cli_only_sections_still_parses_for_mig_worker
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
shellcheck scripts/*.sh packaging/relink-kit/*.sh
make compliance-check
git diff --check
git status --short
```

The final `git status --short` may list only the intended source,
documentation, example, and workflow changes. It must not contain `dist/`,
`target/`, the libnfs checkout or archive, credentials, or local test state.

## Hosted acceptance

The PR is complete only when all of the following are true:

- A GitHub Actions run starts automatically for the pull request.
- The run uses the exact locked Rust, Cargo, and Zig versions.
- libnfs is built from the exact locked fork commit, both libnfs variables are
  set for every Cargo gate, `libnfs-dev` is not installed,
  `EXPECTED_LIBNFS_SHA256` is not passed, and the archive is not uploaded.
- Formatting, Clippy with warnings denied, the complete workspace test suite,
  ShellCheck, and `make compliance-check` all pass.
- The run succeeds without pre-existing repository or Cargo caches.
- Removing or corrupting `examples/worker.toml` on a throwaway draft PR makes
  the workspace-test gate fail.
- Introducing a deliberate shell error on a throwaway draft PR makes ShellCheck
  fail.
- No workflow step has write permissions, publishes artifacts, creates a
  release, or uses `continue-on-error`.

After the check is green on `main`, the repository owner marks it required
for `main`. That is not a condition for merging this PR.

## Non-goals

Do not include any of the following in PR-11A:

- dependency upgrades, feature-gating, or RustSec advisory waivers;
- a new `deny.toml` or a required `cargo deny` job;
- release builds, RPM/DEB/tar creation, or GitHub Release publication;
- changes to the release libnfs build, the locked archive digest, the LGPL
  compliance gate, or the LGPL distribution model (building the pinned fork for
  CI tests is in scope; making the archive digest host-independent belongs to
  PR-11C);
- changes to the libnfs version probe in `crates/migration-mover/build.rs`,
  whose `atleast_version("4.0.0")` floor accepts distro libnfs that cannot
  link (record it as a separate follow-up);
- signing, artifact attestations, SBOM publication, or provenance work;
- package install/uninstall smoke tests;
- real-NFS, performance, oldest-GLIBC, or CPU-compatibility qualification;
- changes to copy, sync, cutover, manifest, mover, or recovery behavior; or
- broad cleanup of the extracted distributed-engine crates.

If a required baseline gate exposes another defect, stop and report it rather
than expanding the PR silently. A small deterministic build/test repair may be
proposed separately with the failing evidence attached.

## Handoff report expected from the implementer

The final implementation report must include:

- commit and PR links;
- the hosted workflow-run link and the throwaway negative-test PR links;
- the exact Rust, Cargo, and Zig versions observed;
- the libnfs commit built and the CI archive's SHA-256 (informational);
- the result of every local and hosted acceptance command;
- any tests that remain ignored and why they require external infrastructure;
- confirmation that no secrets or real infrastructure addresses entered the
  example; and
- a concise list of PR-11B/PR-11C/hardware work deliberately left untouched.
