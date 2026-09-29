# PR-11A handoff — green workspace and baseline CI

Status: **ready for implementation**

Suggested implementer: Luna or another lower-cost implementation model, with
human review of the workflow permissions and final required-check settings.

This is the first, deliberately narrow slice of PR-11 in
`docs/PRODUCTION_RELEASE_HANDOFF.md`. It makes the checked-in workspace green
from a clean clone and runs that baseline on every pull request. It does not
claim that mongoose is ready to release.

## Outcome

Land one focused pull request that:

1. restores the public worker configuration example that two existing tests
   require;
2. makes `cargo test --workspace --locked` pass from a clean clone;
3. adds fail-closed CI for formatting, Clippy, workspace tests, shell scripts,
   and the repository-level LGPL policy; and
4. uses and verifies the Rust and Cargo versions recorded in
   `packaging/release-toolchain.lock.json`.

The workflow must never publish artifacts or describe its output as a release.
Release artifact construction, the modified-libnfs relink proof, package
inspection, signing, and hardware qualification remain separate gates.

## Verified starting state

The following was rechecked after production-hardening PR #1 merged on
2026-09-29:

- There is no checked-in `.github/workflows/` directory.
- There is no `examples/worker.toml`.
- `cargo test --workspace --locked` compiles the workspace and then fails in
  exactly these existing tests:
  - `config::tests::examples_worker_toml_parses`
  - `config::tests::canonical_config_with_cli_only_sections_still_parses_for_mig_worker`
- Both failures are reads of the missing `examples/worker.toml`; do not delete,
  ignore, or weaken the tests to make the command green.
- Tests in `migration-coord` bind a loopback listener on `127.0.0.1:0`. They
  pass with normal host permissions and must not be skipped in CI.
- `make compliance-check` passes.
- The current release toolchain lock records Rust and Cargo 1.98.0.
- Ordinary development builds may use `libnfs` through `pkg-config`. Only
  release artifacts are required to use the digest-pinned static archive.

The broader PR-11 security audit is not green. As of the same date,
`cargo deny check advisories` reports five vulnerabilities (`h2`, `rustls`, and
three `rustls-webpki` advisories) plus the unmaintained `number_prefix` and
`paste` crates. Those findings belong to PR-11B and must not be waived in this
PR.

## Scope

### 1. Restore the canonical worker example

Add `examples/worker.toml` as a safe, public example with placeholder hosts and
no credentials, tokens, private addresses, or environment-specific paths.

The example must:

- parse as `migration_worker::config::Config`;
- use these values required by the existing contract tests:
  - `run.bucket = "vamoose"`
  - `run.endpoint = "https://s3.example.com"`
  - `run.region = "us-east-1"`
- omit `run.profile`, leaving it as `None`;
- preserve verified TLS as the default; do not set `verify_tls = false`;
- provide placeholder `mover.src_url` and `mover.dst_url` NFSv3 URLs;
- preserve the established defaults checked by the tests:
  - strategy label `libnfs_io_uring`
  - pipeline depth `8`
  - io_uring queue depth `256`
  - fixed buffer count `256`
  - fixed buffer size `1 MiB`
  - server-side copy `off`; and
- omit `[coord]`, so the optional coordinator configuration remains `None`.

Prefer omitting defaulted values when that is what the example is intended to
teach. If a default is shown explicitly for operator clarity, keep the value
identical to the Rust default. Add short comments making clear that all hosts
and paths are placeholders.

Do not move the fixture into the test module. It is referenced by
`crates/migration-mover/MANUAL_VERIFY.md` as an operator-facing example and is
part of the published configuration contract.

### 2. Add the baseline GitHub Actions workflow

Add `.github/workflows/ci.yml`. It must run for:

- every pull request; and
- pushes to `main`.

Use least-privilege workflow permissions (`contents: read`) and a concurrency
group that cancels an obsolete run for the same branch or pull request. Give
every job a finite timeout. Pin every action referenced by `uses:` to a full
immutable commit SHA and leave a comment naming its human-readable release
version.

The workflow may use one job or a small number of jobs, but the required gates
must be obvious in the GitHub UI. A single Linux validation job is acceptable
for this slice.

The Linux runner must install the ordinary developer-build prerequisites:

- `libnfs-dev`
- `pkg-config`
- `jq`
- `shellcheck`

Print `pkg-config --modversion libnfs` before compiling so failures identify the
native dependency in use. Do not build, upload, or cache a release binary in
this workflow. A Cargo compilation cache is optional; correctness must not
depend on a warm cache, and any cache action must also be SHA-pinned.

Read the expected Rust and Cargo versions from
`packaging/release-toolchain.lock.json`. Install that exact Rust toolchain with
the `rustfmt` and `clippy` components, select it for all later steps, and fail
if `rustc --version` or `cargo --version` does not match the lock. Do not
duplicate an unverified version constant only in the workflow.

Run these gates without `continue-on-error`:

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

### 3. Update the handoff status

After the implementation and hosted workflow are green, update the PR-11
section of `docs/PRODUCTION_RELEASE_HANDOFF.md` to record:

- the PR-11A implementation commit or pull request;
- the exact CI commands now enforced;
- that the missing-example workspace-test failure is closed; and
- that PR-11B, PR-11C, and real-NFS hardware qualification remain release
  blockers.

Do not change the top-level `Status: release blocked` line in that document.

## Required implementation sequence

1. Create `examples/worker.toml`.
2. Run the two previously failing `migration-worker` tests directly.
3. Run the full local acceptance command set below.
4. Add the CI workflow using the same commands.
5. Push the branch and observe a hosted run from a clean GitHub runner.
6. Fix root causes of any hosted-only failure; do not weaken a gate.
7. Update the PR-11 handoff status only after the hosted run is green.

## Local acceptance

Run from the repository root with no uncommitted generated fixtures:

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
`target/`, a generated static library, credentials, or local test state.

## Hosted acceptance

The PR is complete only when all of the following are true:

- A GitHub Actions run starts automatically for the pull request.
- The run uses the exact locked Rust and Cargo versions.
- Formatting, Clippy with warnings denied, the complete workspace test suite,
  ShellCheck, and `make compliance-check` all pass.
- The run succeeds without pre-existing repository or Cargo caches.
- Removing or corrupting `examples/worker.toml` on a throwaway branch makes the
  workspace-test gate fail.
- Introducing a deliberate shell error on a throwaway branch makes ShellCheck
  fail.
- No workflow step has write permissions, publishes artifacts, creates a
  release, or uses `continue-on-error`.
- The repository owner configures the resulting CI check as required for
  `main`, or records why branch protection cannot yet be enabled.

## Non-goals

Do not include any of the following in PR-11A:

- dependency upgrades, feature-gating, or RustSec advisory waivers;
- a new `deny.toml` or a required `cargo deny` job;
- release builds, RPM/DEB/tar creation, or GitHub Release publication;
- static-libnfs rebuilds or changes to the LGPL distribution model;
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
- the hosted workflow-run link;
- the exact Rust, Cargo, and system `libnfs` versions observed;
- the result of every local and hosted acceptance command;
- any tests that remain ignored and why they require external infrastructure;
- confirmation that no secrets or real infrastructure addresses entered the
  example; and
- a concise list of PR-11B/PR-11C/hardware work deliberately left untouched.
