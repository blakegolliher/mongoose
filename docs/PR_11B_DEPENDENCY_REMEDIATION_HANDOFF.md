# PR-11B handoff — dependency and advisory remediation

Status: **ready for boundary design** (Phase 1). Phase 2 starts only after
the owner approves the Phase 1 design.

Suggested implementers:

- Phase 1 (feature-boundary design and legacy-TLS removal): a stronger
  implementation model, with owner review of the design before merge.
- Phase 2 (mechanical dependency updates and the CI job): Luna or another
  lower-cost implementation model.

This is the second slice of PR-11 in `docs/PRODUCTION_RELEASE_HANDOFF.md`
(required outcome 3). It makes the dependency policy enforceable and removes
code mongoose does not use from the shipped dependency graph. It is more
architectural than PR-11A because it crosses Cargo features and the shared
engine crates. It does not claim that mongoose is ready to release.

## Outcome

Land reviewed changes that:

1. add a reviewed `deny.toml`;
2. remove or feature-gate the AWS/S3, coordinator HTTP, and TLS dependencies
   that mongoose does not use, so the shipped `mongoose` graph no longer
   contains them;
3. clear the five active vulnerabilities (`h2`, `rustls`, and three
   `rustls-webpki`) and the two unmaintained dependency paths (`number_prefix`,
   `paste`) without any advisory `ignore` entry;
4. add `cargo deny --locked check advisories licenses bans sources` to CI, but
   only after it is green; and
5. prove the lean mongoose graph still passes workspace tests, Clippy,
   `make compliance-check`, and the static-libnfs release gate.

## Prerequisites

- **PR-11A is merged**, so the CI workflow exists and builds the pinned libnfs
  fork. The `cargo deny` job is added to that workflow.
- **nfs-walker PR #10 is merged with a regular merge commit** (done
  2026-09-29, merge `f37cf92`), so the pinned walker commit `2dded4c` is
  reachable from walker `main`. The walker changes this PR needs land on
  walker `main` through their own PR, also merged with a regular merge
  commit, before mongoose pins them.

## Verified starting state

Rechecked on 2026-09-29 against `origin/main` (`e9fa2e7`) with the pinned
walker `2dded4c`, using cargo-deny 0.19.9. There is no `deny.toml`.

### Advisories

`cargo deny --locked check advisories` fails with seven findings:

| Advisory | Kind | Locked crate | Enters through | Fix |
| --- | --- | --- | --- | --- |
| RUSTSEC-2026-0258 | vulnerability | `h2` 0.3.27 | `aws-smithy-http-client` `hyper-014` feature (hyper 0.14) | none on 0.3; fixed in ≥0.4.16. The locked `h2` 0.4.18 is not affected. |
| RUSTSEC-2026-0285 | vulnerability | `rustls` 0.23.40 | `aws-smithy-http-client`, `hyper-rustls` 0.27, `reqwest`, `tokio-rustls` 0.26 | semver-compatible 0.23.45. A dry run of `cargo update -p rustls@0.23.40` also moves `aws-lc-rs` 1.16.3→1.18.1 and `aws-lc-sys` 0.40.0→0.45.0. |
| RUSTSEC-2026-0098, -0099, -0104 | vulnerability | `rustls-webpki` 0.101.7 | `rustls` 0.21.12 ← `hyper-rustls` 0.24, `tokio-rustls` 0.24, `aws-smithy-http-client` `legacy-rustls-ring`, all from `migration-core` | none on 0.101; fixed in ≥0.103.12 (0104 needs ≥0.103.13) |
| RUSTSEC-2025-0119 | unmaintained | `number_prefix` 0.4.0 | `indicatif` 0.17.11 in nfs-walker, used only by walker `src/progress.rs` | `indicatif` ≥0.18.0 depends on `unit-prefix` instead |
| RUSTSEC-2024-0436 | unmaintained | `paste` 1.0.15 | `parquet` 54.3.1 in mongoose's workspace crates and in nfs-walker | `parquet` ≥59.2.0 has no `paste` dependency |

The legacy stack in rows 1 and 3 exists for one reason. The workspace enables
`aws-smithy-http-client`'s `hyper-014` and `legacy-rustls-ring` features and
adds `hyper-rustls` 0.24 and `rustls` 0.21 (`dangerous_configuration`) only to
support `verify_tls = false` in `crates/migration-core/src/s3.rs`, through
the deprecated `hyper_014::HyperClientBuilder`. Removing that path from the
workspace is expected to remove `h2` 0.3, `rustls` 0.21, and `rustls-webpki`
0.101. Confirm that with `cargo tree -i` after the change.

The version targets do not raise the workspace MSRV (`rust-version = "1.91.1"`):
`parquet` 59.2.0 declares 1.85, `parquet`/`arrow` 60.0.0 declare 1.88, and
`indicatif` 0.18.6 declares 1.85.

### Licenses, bans, and sources

These three checks already pass under this draft configuration:

- the `[licenses] allow` list copied from `about.toml`'s `accepted` list;
- `[bans] multiple-versions = "warn"`, `wildcards = "deny"`, and
  `allow-wildcard-paths = true`;
- `[sources] unknown-registry = "deny"`, `unknown-git = "deny"`, and
  `allow-git = ["https://github.com/blakegolliher/nfs-walker"]`.

Bans reports 24 duplicate-version warnings. Without `allow-wildcard-paths`,
bans fails on the internal path dependencies. Every internal crate is already
`publish = false`, which that setting relies on. Only advisories is red.

### The shipped mongoose graph

`cargo tree -p mongoose -e normal` lists 317 unique crates. Thirty-nine of them
are network, TLS, or crypto-backend crates. They include `aws-sdk-s3`,
`aws-config`, and the other AWS/smithy crates; both `rustls` 0.21 and 0.23;
both crypto backends (the `aws-lc-sys` C library and `ring`); hyper 0.14 and
1.x; `reqwest`; and `tower-http`. mongoose's own sources import no S3, HTTP,
or TLS API. These crates enter through:

- **`migration-core`**: unconditional `aws-sdk-s3`, `aws-config`,
  `aws-smithy-http-client`, `hyper-rustls`, and `rustls`. The S3 code is in
  `s3.rs`. `errors.rs` has `Error::S3(#[from] aws_sdk_s3::Error)`. `claim.rs`
  defines the `ClaimStore` trait, whose production implementation is the S3
  client. mongoose uses `records`, `shard`, `schema`, and `prepare_tools`.
- **`migration-worker`**: unconditional `reqwest`. `coord_client.rs` is the
  HTTP client. `coord_driver.rs` builds that client and also defines
  `EventEmitter`. `orchestrator.rs` uses `S3Client`. mongoose imports
  `shard_processor::ShardProcessor`, `mover_factory::{self, MoverParams}`,
  `heartbeat::LivePending`, `throughput::ThroughputCounter`, `caps`, and
  `coord_driver::EventEmitter`, but calls only `EventEmitter::disabled()`.
  `heartbeat` uses the `ClaimStore` trait, not the S3 client.
- **`migration-coord`** (`axum`, `axum-server`, AWS) is reachable from the
  worker only as a dev-dependency.
- **nfs-walker** is already `default-features = false`, which drops its
  `datafusion`/`axum`/`rust-embed` dashboard. It contributes none of the
  network or TLS crates, only `indicatif` 0.17 and `parquet` 54.

No Arrow or Parquet type crosses the walker/mongoose API boundary. Both
repositories still need `parquet` ≥59.2.0 to drop `paste`, and matching
versions avoid shipping two Parquet implementations.

## Owner decisions for Phase 1

Approved 2026-09-29:

1. **Remove the insecure execution mode.** Keep parsing `verify_tls` for
   configuration compatibility, but reject `false` with a clear startup error.
   Do not rebuild an insecure, lab-only S3 path on the current TLS stack. The
   hyper-0.14 and rustls-0.21 stack leaves the workspace, and no advisory is
   ignored.
2. **Audit the whole workspace.** `cargo deny` checks every workspace member,
   not only the mongoose graph, with no `ignore` entries. Known-vulnerable code
   in another checked-in binary is still a repository failure.
3. **Keep the engine crates behind features.** Retain `mig-worker`,
   `migration-coord`, and the S3 claim store, with their dependencies enabled
   only by the features that need them. Do not delete the extracted engine
   crates in PR-11B.

## Phase 1 — boundary design (stronger model)

Deliver a short design note, in the PR description or appended to this file,
then the structural changes:

- Define additive Cargo features so S3 and coordinator HTTP code compile only
  when requested, for example `s3` on `migration-core` and `coord` on
  `migration-worker`. The names, and whether they are on by default, are the
  designer's choice. The engine binaries enable what they need. mongoose
  enables none of them, using `default-features = false` where needed.
- Decide where `EventEmitter` lives so mongoose can use
  `EventEmitter::disabled()` without the coordinator HTTP client.
- Gate `Error::S3` and the S3 `ClaimStore` implementation without breaking
  exhaustive matches or the engine crates' own tests. The `ClaimStore` trait
  itself can stay ungated.
- Apply owner decision 1, and remove `hyper-014`, `legacy-rustls-ring`,
  `hyper-rustls` 0.24, and `rustls` 0.21 from the workspace.
- Keep `cargo test --workspace --locked` building and running every engine
  test. Do not add ignores, package exclusions, or filters.

Workspace-wide commands unify features across all members. They can compile
mongoose with S3 enabled even when mongoose alone would not build, so the
per-package commands in the acceptance section are required.

## Phase 2 — mechanical updates (after the design is approved)

1. **rustls.** If `rustls` 0.23 is still in the workspace graph, run
   `cargo update -p rustls@0.23.40` to reach ≥0.23.45.
2. **nfs-walker.** In an nfs-walker PR against walker `main`, bump
   `indicatif` 0.17→0.18 (only `src/progress.rs` uses it) and `arrow`/`parquet`
   54→the version mongoose adopts (≥59.2.0). Build and test the walker with
   `--no-default-features`, keep it Clippy-clean with `-D warnings`, and merge
   with a regular merge commit. Then bump the `rev` in
   `crates/mongoose/Cargo.toml` and `packaging/nfs-walker.lock.json` together.
   The lock's artifact hash comes from `cargo zigbuild` with the recorded
   command.
3. **arrow/parquet in mongoose.** Move the workspace `arrow` and `parquet`
   pins from 54 to the same version (≥59.2.0). `SCHEMA_CONTRACT.md` and the
   existing schema and contract-drift tests must pass unchanged. Work
   directories written by the current build must still load, so add a test
   that reads a shard written with `parquet` 54 if none exists.
4. **`deny.toml`.** Add it with the draft settings above, plus
   `[graph] targets = ["x86_64-unknown-linux-gnu"]`, matching `about.toml`.
   Unmaintained advisories must fail the check (no scope narrowing), and
   `[advisories]` has no `ignore` entries. Review the license allow-list
   against the lean graph and drop entries that no longer apply.
5. **CI job.** Only after
   `cargo deny --locked check advisories licenses bans sources` is green
   locally, add it to the PR-11A workflow for pull requests and pushes to
   `main`. Pin cargo-deny's version (currently 0.19.9) in the same way as the
   other tools, with `contents: read`, a finite timeout, and SHA-pinned
   actions. The advisory database changes independently of this repository,
   so a new advisory can turn an unrelated PR red. The response is to fix it
   or escalate to the owner, never to add an `ignore` without owner approval,
   a written reason, and a review date.

## Acceptance

With `VAMOOSE_LIBNFS_DIR` and `NFS_WALKER_LIBNFS_DIR` exported as in PR-11A:

```bash
cargo deny --locked check advisories licenses bans sources
# Must print nothing:
cargo tree -p mongoose -e normal --prefix none --locked | sed 's/ (\*)//' | sort -u |
  grep -E '^(aws-[a-z-]+|hyper(-[a-z]+)?|h2|rustls(-[a-z]+)?|tokio-rustls|reqwest|axum(-[a-z]+)?|tower-http|ring|aws-lc-(rs|sys)|webpki[a-z-]*) v'
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy -p mongoose --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo test -p mongoose --locked
shellcheck scripts/*.sh packaging/relink-kit/*.sh
make compliance-check
```

Also build and test every gated crate on its own, with its new features off,
using the commands the design note specifies.

Release gate (owner, on the release host): build the release artifacts through
the normal `make` targets and run
`scripts/check-lgpl-compliance.sh --release-dir ...` for the exact artifact
set. The regenerated third-party license notices must reflect the lean graph.

Hosted:

- The CI run for the PR is green, including the new `cargo deny` job.
- On a throwaway draft PR, reintroducing a known-vulnerable version (for
  example reverting the `rustls` update) makes the `cargo deny` job fail.
  Close the PR and delete its branch afterwards.

## Non-goals

- Changes to copy, sync, cutover, manifest, mover, or recovery behavior.
- Deleting the distributed-engine crates (owner decision 3).
- Release workflows, signing, SBOMs, attestations, or provenance (PR-11C).
- Making the locked `libnfs.a` digest independent of the Zig install path
  (PR-11C).
- libnfs, static-link, relink-kit, or LGPL distribution changes.
- Advisory `ignore` entries, or scoping the check down without owner approval.

If a gate exposes another defect, stop and report it rather than expanding
the PR silently.

## Report expected from the implementers

- The Phase 1 design note and the owner's approval.
- Commits and PR links in both repositories, including the walker merge commit
  and the updated pin and lock.
- `cargo tree -p mongoose -e normal` unique-crate counts before (317) and
  after, and the empty output of the acceptance `grep`.
- The full `cargo deny` output.
- The result of every local and hosted acceptance command.
- The release-gate evidence, or a note that it is waiting on the owner.
- A concise list of PR-11C and hardware work left untouched.
