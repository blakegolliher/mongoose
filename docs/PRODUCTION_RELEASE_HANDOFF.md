# Production release readiness handoff

Status: **release blocked**

This document turns the 2026-09-28 production-readiness review into
independently assignable work items. It is intended for a mixed team of human
and LLM implementers. Each item states the correctness problem, the required
outcome, and the evidence needed before it can be closed.

The release should remain blocked until PR-01 through PR-10 are closed. PR-11
is the release-system gate that makes those fixes repeatable.

## Ground rules for every item

- Preserve byte-oriented POSIX paths. Do not convert migration paths to UTF-8
  merely to simplify validation or comparison.
- Preserve the atomic destination publish rule: a destination file becomes
  visible only through the final `.partial` to final-name rename.
- Treat a successful command as a correctness statement. If the tool cannot
  prove that statement, fail or narrow the documented statement explicitly.
- Add regression tests at the lowest practical layer and an integration test
  at the mongoose layer. Tests that need an NFS server must be recorded as a
  named hardware gate rather than silently omitted.
- Update README, man page, and reference documentation whenever observable
  CLI behavior or guarantees change.
- Do not weaken a check merely because live filesystems are noisy. Any
  opt-out that permits incomplete results must be explicit, difficult to use
  accidentally, and reflected in the final exit status.

## Suggested execution lanes

The lanes can proceed mostly in parallel:

| Lane | Work items | Suggested lead |
|---|---|---|
| Scanner completeness | PR-01, PR-02 | LLM implementation with human review of walker behavior |
| State and recovery | PR-05, PR-06, PR-08, PR-09 | LLM-suitable, with crash/concurrency test review by a human |
| Migration correctness | PR-03, PR-04, PR-07 | Human-led design, then mixed implementation |
| Distribution | PR-10, PR-11 | Human release/legal owner with LLM implementation support |

Important dependencies:

- PR-01 and PR-02 modify or replace behavior in the pinned `nfs-walker`.
  Update the git revision in `crates/mongoose/Cargo.toml` and the walker lock
  metadata together.
- PR-06 should land before relying on fixed-name local temporary files in any
  new implementation. Unique temporary names are still preferred.
- PR-03 must not rely on PR-07 as its only evidence: cutover verification must
  inspect the destination independently.
- PR-11 is closed last, after it enforces the completed gates.

---

## PR-01 — Make `--exclude` real and define its matching semantics

Priority: **P0 / release blocker**

Status (2026-09-28): implemented on branch `pr-01-exclude-globs` plus
nfs-walker commit 2dded4c on `embed-libnfs-override` (pinned in
`crates/mongoose/Cargo.toml` and `packaging/nfs-walker.lock.json`).
Contract chosen: a glob on the directory's own name; the directory
and its whole subtree are omitted, including its own row. The walker
gained `--exclude-dir GLOB` (`config::compile_dir_glob`,
`config::excluded_entry`, applied in both worker loops before a
directory is queued or emitted); its `--exclude` stays a path regex
and the help says which is which. mongoose passes only
`--exclude-dir`, validates every pattern with the walker's compiler
before writing `run.json` and before creating a scan attempt, keeps
the set in `run.json`, and reads it back for every sync and cutover
scan. Contract in `docs/REFERENCE.md` "Excludes". **Hardware gate
outstanding:** the exclude check against a real export.

Suggested lead: LLM implementer in the walker repository; human review of the
operator-facing matching contract.

### Problem

`mongoose copy --exclude ...` passes patterns to the embedded walker through
`crates/mongoose/src/scan.rs`, but the pinned walker only compiles the patterns
and never consults them while scheduling or emitting entries. Its own source
currently notes that `exclude_patterns` is not consulted by the worker.

As a result, the documented recommendation to exclude `.snapshot`, `.zfs`,
and similar trees is ineffective. A migration can copy every snapshot as live
data, causing extreme space amplification and an incorrect destination.

There is a second contract mismatch: mongoose documents `GLOB`, while the
walker currently compiles each value as a regular expression.

### Required outcome

1. Select and document one matching contract. The preferred mongoose contract
   is a glob applied to each directory basename, matching the current CLI text
   and the common `--exclude .snapshot` use case.
2. Apply the matcher before an excluded directory is queued for recursion and
   before its row is emitted. Define explicitly whether the excluded directory
   row itself is absent; the recommended behavior is to omit the whole subtree,
   including that row.
3. Apply the same rules to the initial copy and every sync/cutover scan.
4. Reject invalid patterns before mounting or writing the work directory.
5. Keep the exclude set in `run.json` as immutable job identity.
6. Update the pinned walker revision and `packaging/nfs-walker.lock.json`
   together. Remove the existing comment that the lock and compiled revision
   intentionally differ once that is no longer true.

### Acceptance tests

- A walker-level fixture containing `.snapshot/a`, `.zfs/b`, and ordinary
  siblings emits only the ordinary siblings for the documented patterns.
- An excluded directory is neither emitted nor traversed.
- Multiple patterns work, and order does not change the result.
- Invalid patterns fail before a scan attempt is created.
- A mongoose integration test proves the same exclude set is used in pass 0
  and a later sync pass.
- A hardware NFS smoke test verifies the behavior against a real export.

### Documentation to update

- `README.md`
- `packaging/mongoose.1`
- `docs/REFERENCE.md`
- Walker CLI help, so it uses the same word—glob or regex—as mongoose

---

## PR-02 — Fail incomplete scans instead of checkpointing them as complete

Priority: **P0 / release blocker**

Status (2026-09-28): implemented on branch `pr-02-fail-incomplete-scans`
plus nfs-walker commit on `embed-libnfs-override`, which was then
brought up to nfs-walker main (0.2.0) by resolving the conflicts on
nfs-walker PR #10; mongoose pins the merge commit in
`crates/mongoose/Cargo.toml` and `packaging/nfs-walker.lock.json`.
The 0.2.0 walker dropped `--parquet-file-size-mb` (its built-in part
size is the same 512 MiB), so the shared invocation no longer passes
it.
Walker: bounded retry with backoff for transient failures, stale
handles re-resolved by path, confirmed disappearances counted as
`vanished` rather than errors, retries only before any row of the
directory was emitted, every failure in `scans/<id>/errors.jsonl`,
and `WalkerError::ScanIncomplete` (binary exit 3) instead of `Ok`
with a counter. mongoose: `scan::accept_scan` / `checkpoint_reusable`
refuse any errors, the attempt is kept and recorded in `scan.json`
with `complete: false`, the next run scans afresh, no permissive
mode. Contract in `docs/REFERENCE.md` "Scan completeness".
**Hardware gate outstanding:** deny access to one source directory
and verify nonzero exit, the diagnostic, and no manifest/cutover
success.

Suggested lead: LLM implementer; human review of which walker errors are
recoverable inside the walker versus fatal to the scan as a whole.

### Problem

The walker returns `WalkStats { errors, completed }`. Mongoose prints
`stats.errors` but writes a complete `scan.json` whenever `completed` is true.
The pinned walker increments this error count for permission failures,
READDIRPLUS failures, lookup failures, submit failures, and connection-level
failures while continuing the scan.

A failed directory can therefore disappear from the index while the command
continues successfully. If the failure repeats on later scans, source-to-source
classification can appear stable and cutover can succeed without that subtree.

### Required outcome

1. A scan with any unresolved walker error must not produce a reusable complete
   checkpoint.
2. Preserve the failed attempt directory and walker progress log for diagnosis.
   A rerun must create a fresh attempt.
3. Prefer bounded retry inside the walker for explicitly transient failures.
   If retries are exhausted, return a structured fatal error rather than only
   incrementing a counter.
4. Do not add a permissive mode in the first fix. If an incomplete-scan mode is
   later required, it must use a distinct non-success exit status, persist the
   exact error set, and be forbidden for cutover.
5. Include scan error counts and the diagnostic log path in the final error.

### Acceptance tests

- A pure mongoose test feeds completed stats with `errors > 0` into the scan
  acceptance logic and verifies rejection and absence of a complete checkpoint.
- Walker tests inject READDIR, lookup, and permission failures and verify they
  become an unsuccessful scan after retry policy is exhausted.
- Rerunning after a failed attempt selects a new attempt directory.
- Cutover cannot consume or create an incomplete scan checkpoint.
- Hardware test: deny access to one source directory and verify nonzero exit,
  a useful diagnostic, and no manifest/cutover success.

---

## PR-03 — Replace the source-quiescence gate with truthful destination verification

Priority: **P0 / release blocker**

Status (2026-09-28): implemented on branch `pr-03-cutover-verification`.
Product decision: keep the strong `--cutover` name and implement full
byte verification; no sampled or metadata-only mode. The verification
contract, including what is deliberately outside it (directory
mtimes, atimes, hardlink topology, symlink attrs, root attrs), is
documented in `docs/REFERENCE.md` "Cutover verification". Software
acceptance tests are in `crates/mongoose/src/verify/` and
`crates/mongoose/tests/sync_pipeline.rs`. **Hardware gate outstanding:**
real NFSv3 exports, at least one multi-gigabyte file, concurrent
directory fan-out, and the excluded-subtree check on a real export.

Suggested lead: human design owner, with LLM implementation support.

### Problem

The current `sync --cutover` classifier compares the latest source scan with
the preceding source scan. Zero NEW/DIRTY/DELETED rows proves only that the
source appears unchanged since the last baseline. The destination is never
scanned or read.

The command can therefore report convergence after a destination file was
deleted, modified, incompletely copied, or given the wrong metadata. This does
not satisfy the README and man-page claim that a clean exit means the trees
match.

### Required outcome

First make an explicit product decision and record it in the implementation
PR:

- If `--cutover` continues to mean “the trees match,” it must compare source
  and destination namespace, types, relevant metadata, and file contents.
- If byte verification is intentionally deferred, rename/narrow the guarantee
  to “metadata-converged” everywhere and make it impossible to mistake that
  result for byte equality. The production recommendation is to retain the
  strong cutover name and implement byte verification.

Implementation requirements:

1. Scan source and destination after source writers are stopped. Use the same
   exclude rules on both sides.
2. Detect missing and extra destination paths, file-type mismatches, size and
   required metadata mismatches, and unresolved pending failures/downgrades.
3. Define content verification. A full read/hash is the definitive mode. Any
   sampled or metadata-only mode must have a different name and result field.
4. Verify symlink target bytes. Define how owner, mode, timestamps, hardlink
   topology, and the migration root are evaluated.
5. Do not advance `baseline.json`, prune evidence, or print success when a
   verification mismatch exists.
6. Persist a machine-readable verification report containing counts and a
   bounded set of mismatch samples.
7. Ensure destination entries intentionally retained by the no-delete policy
   are reported as extras and cause strong cutover to fail.

### Acceptance tests

- Clean source and destination pass.
- Missing, extra, modified-same-size, truncated, type-changed, and wrong-symlink
  destination entries each fail.
- Wrong mode/owner/time behavior matches the documented verification contract.
- A destination modification made after the last successful sync is detected.
- Excluded subtrees do not participate in either scan.
- Verification mismatch leaves the previous baseline and evidence intact.
- Hardware acceptance covers at least one multi-gigabyte file and concurrent
  directory fan-out on real NFSv3 exports.

### Documentation to update

- `README.md`
- `packaging/mongoose.1`
- `docs/REFERENCE.md`
- `docs/work-items/MONGOOSE_RESYNC.md`, whose current design records only a
  source-to-source zero-drift gate

---

## PR-04 — Prove endpoint separation across aliases before any destination write

Priority: **P0 / release blocker because failure can destroy source data**

Status (2026-09-28): implemented on branch `pr-04-endpoint-identity`.
Three layers (`crates/mongoose/src/endpoint.rs`, `identity.rs`):
canonical spelling, name resolution, and mounted identity (peer
address, root filehandle, `(fsid, fileid)`, ancestor chain, a
probe for filehandle matches across apparently different servers,
and a source-index search for an aliased destination root). Re-run
from recorded job state on every `copy`, `sync`, and cutover;
hand-edited manifests go through the same parser; the per-file
check now compares server-absolute paths whenever the servers may be
one, with no override. Contract and residual gap in
`docs/REFERENCE.md` "Endpoint separation". **Hardware gate
outstanding:** two names for one NFS server, sibling paths on one
export, two exports on one server, and the no-CREATE assertion.

Suggested lead: human design owner familiar with NFSv3 identity and libnfs;
mixed implementation.

### Problem

The startup and per-file self-target checks treat unequal URL strings as
different servers. Common spellings of the same endpoint—hostname versus IP,
two DNS aliases, explicit versus default port, or differing URL options—bypass
both checks. The code comments record that overlapping endpoints previously
truncated source files.

String inequality is not proof that two NFS URLs are disjoint.

### Required outcome

1. Canonicalize equivalent URL spellings, including default port and option
   normalization where semantics are known.
2. Resolve hostnames and conservatively detect intersecting address sets.
3. Add a mounted-endpoint identity check. The design should use the strongest
   identity available from libnfs/NFSv3, such as connected peer identity plus
   export/root filehandle information, rather than trusting DNS alone.
4. When the endpoints may refer to the same server/export, prove that their
   effective roots are disjoint before creating any destination file.
5. If separation cannot be proved, refuse by default. Any escape hatch must be
   explicitly named as destructive-risk acceptance and must not disable the
   per-file check.
6. Re-run the strong check from recorded job state on every resume, not only
   during initial prepare.
7. Parse and validate hand-edited manifests through the same endpoint layer;
   the current weaker `migration_core::overlap::check` is insufficient for
   whole paths encoded in mongoose URLs.

### Acceptance tests

- Same export addressed by hostname/IP, two aliases, default/explicit port,
  case variants, IPv6 forms, and benign query variation is rejected when paths
  overlap.
- Disjoint sibling paths on a proven identical export remain allowed if the
  design can prove them safe.
- Truly different servers remain allowed.
- A hand-edited overlapping manifest is rejected before pools mount for copy.
- Hardware test uses two names for the same NFS server and verifies that no
  destination CREATE occurs.

---

## PR-05 — Make result records durable before committing shard progress

Priority: **P0 / release blocker for crash-safe resume**

Suggested lead: LLM implementer; human review of filesystem ordering.

### Problem

Per-shard failure and downgrade JSONL files are written with
`std::fs::write`. They are not written atomically, `sync_all` is not called,
and their parent directory is not synced. The code then atomically writes and
fsyncs `progress.json`, marking the shard complete.

After a host crash or power loss, progress can be durable while the failure
file is absent or torn. The next sync will not force those failed paths into
the pending set, so they may never be retried.

### Required outcome

1. Introduce a durable atomic byte/JSONL writer: write a same-directory unique
   temporary file, flush, `sync_all`, rename, then fsync the parent directory.
2. Use it for non-empty per-shard failure and downgrade files.
3. When removing a stale result file after a clean retry, fsync the parent
   directory before marking progress complete.
4. Preserve the ordering invariant: both result streams are durable before the
   shard path is appended to durable `progress.json`.
5. On any result persistence error, do not mark the shard complete.
6. Prefer unique temporary names even after PR-06 adds a process lock.

### Acceptance tests

- Write, replacement, and stale removal round-trip correctly.
- An injected write/fsync/rename failure leaves the shard incomplete.
- A crash/fault-injection harness demonstrates that every durable completed
  shard has either its complete result file or a durably recorded empty result.
- Sync pending collection retries every failure that was committed before an
  injected crash.

---

## PR-06 — Enforce one live process per work directory

Priority: **P1 / required before production**

Suggested lead: LLM implementer.

### Problem

There is no work-directory lock. Two `copy` or `sync` processes can choose the
same scan attempt or pass number and race on scan output, canonical shards,
classification directories, delta shards, fixed `.partial` checkpoint files,
failure records, and progress.

Atomic rename prevents a torn individual JSON document; it does not make two
multi-file state machines safe to run concurrently.

### Required outcome

1. Acquire a nonblocking OS advisory lock associated with the root work
   directory before the first state write.
2. Hold the lock across the complete command. For `copy`, this means one lock
   spans prepare and copy rather than being released between stages.
3. Store diagnostic owner information—PID, hostname, command, and start time—
   in or beside the lock without treating that text as the locking mechanism.
4. On contention, fail immediately with the owner information and a clear
   instruction. Do not automatically delete a “stale” lock file; the kernel
   lock, not file existence, determines liveness.
5. Apply the same lock to future repair/doctor commands that mutate a work dir.
6. Protect creation of a previously absent work directory against two initial
   invocations racing.

### Acceptance tests

- Two file handles in one process cannot obtain the exclusive lock
  simultaneously.
- A subprocess holding the lock causes both `copy` and `sync` to fail before a
  checkpoint changes.
- Lock release on normal exit, error, panic unwind, SIGINT, and SIGTERM is
  verified where the OS permits graceful handling.
- A leftover lock file without a live kernel lock does not block recovery.

---

## PR-07 — Add torn-copy detection to the mover mongoose actually uses

Priority: **P1 / required before production**

Suggested lead: mixed implementation; human review and hardware validation of
new raw libnfs operations.

### Problem

Mongoose configures `use_bucketed_pool: false` and `use_raw_fh: true`. That
selects the synchronous/raw mover. Its code explicitly returns
`MoveOutcome::torn = false` and has no pre/post source-stat bracket. Torn-copy
detection exists only in the async bucketed mover.

The sync pass nevertheless carries `TornCopy` downgrade records forward as
pending work. For the selected mover, those records are not produced. A file
modified while it is read can therefore commit mixed data without the promised
forced retry, especially when the change is invisible at the later scan's
timestamp granularity.

### Required outcome

Choose one approach and document the performance/safety decision:

- Add pre/post source `fstat` bracketing to the raw synchronous filehandle path
  and propagate a torn outcome through `MoveOutcome`; or
- Move mongoose to the async bucketed path after its resource, performance,
  and real-hardware gates pass.

In either approach:

1. Compare at least size, mtime, and ctime before and after the read.
2. Commit semantics may remain at-least-once, but a torn commit must emit a
   durable `TornCopy` record and increment `files_torn`.
3. The next sync must force that path DIRTY until a clean copy succeeds.
4. A clean cutover must contain no unresolved torn rows.
5. Avoid stamping stale scan timestamps in a way that masks the torn status.

### Acceptance tests

- A pure result-classification test covers clean and changed pre/post tuples.
- A mover integration test mutates a sufficiently large source during copy and
  observes `files_torn` plus a `TornCopy` record.
- The following sync recopies the path even if its scan tuple otherwise matches
  the baseline.
- A clean retry removes the pending status.
- Required raw/async libnfs smoke and throughput gates pass on real hardware.

---

## PR-08 — Validate manifests and enforce recorded shard digests at use time

Priority: **P1 / required before production**

Suggested lead: LLM implementer.

### Problem

Manifest construction records a SHA-256 digest for every canonical and delta
shard. The copy loop later checks only the file size. Same-size corruption or
tampering can therefore change paths, row metadata, or copy instructions
without being detected by the recorded digest.

Manifest paths are also joined directly to the work directory without a
central validation pass, leaving hand-edited absolute or `..` paths outside the
intended state tree.

### Required outcome

1. Add one manifest validation function used by copy, sync classification, and
   cutover verification.
2. Validate format version, run identity, endpoint identity, totals, duplicate
   shard entries, row counts where practical, and safe work-dir-relative shard
   paths. Reject absolute paths and all `..` traversal.
3. Verify size and SHA-256 before consuming every non-completed shard. Verify
   all baseline/current shards before classification or verification.
4. Decide whether completed copy shards need rehashing on resume; at minimum,
   any shard that will be read again must be verified.
5. Return a corruption error that identifies the shard and expected/actual
   digest. Never silently rebuild from untrusted bytes.

### Acceptance tests

- Same-length byte mutation is rejected.
- Missing, duplicate, absolute, and traversal paths are rejected.
- Incorrect totals and a manifest from another run are rejected.
- Both canonical and delta manifests use the same checks.
- A valid existing work directory still resumes without state conversion.

---

## PR-09 — Return a non-success status for interrupted work

Priority: **P1 / required for safe automation**

Suggested lead: LLM implementer.

### Problem

SIGINT and SIGTERM stop at a safe batch boundary but currently return exit code
0. Service managers, shell scripts, and orchestration systems therefore cannot
distinguish a complete migration from one that must be resumed.

### Required outcome

1. Preserve graceful batch-boundary shutdown and resumability.
2. Track whether SIGINT or SIGTERM requested the stop.
3. Return a documented nonzero incomplete/interrupted status. Conventional
   `130` for SIGINT and `143` for SIGTERM are preferred if the CLI can preserve
   the signal reason; otherwise reserve one stable mongoose-specific code.
4. Keep exit code 2 for a completed pass with per-file failures unless the
   revised exit-code design explicitly replaces it.
5. Update all user documentation and any release smoke scripts.

### Acceptance tests

- Pure exit-mapping tests cover success, recorded failures, SIGINT, SIGTERM,
  and fatal error.
- Subprocess tests signal a running command, observe the documented code, and
  verify that rerunning resumes safely.
- An interrupted sync does not advance its baseline.

---

## PR-10 — Resolve static libnfs LGPL distribution requirements

Priority: **P0 / distribution blocker**

Status: **Implemented locally on 2026-09-28; owner review required before publication.**

Suggested lead: human release/legal owner. This item requires legal review;
implementation work can be delegated after the distribution model is chosen.

Decision (2026-09-28): the project owner selected the static-link model to
preserve the single-binary user experience. `docs/LGPL_COMPLIANCE.md` is the
normative policy, `AGENTS.md` makes it a repository-wide implementation rule,
and `make release` now runs a fail-closed artifact gate. The embedded license
command, exact source bundles, offline vendored relink kit, package notices,
static-only build path, and automated modified-libnfs relink exercise are now
implemented. Distribution remains blocked whenever any one of them fails.

### Problem

The portable binary contains libnfs code and has no dynamic `libnfs` dependency.
The Makefile and README describe the walker link as static, while
`crates/migration-mover/build.rs` says libnfs must be dynamically linked.

The RPM declares only `MIT`; the DEB and tarball install only the project's MIT
license; and the repository does not contain the `THIRD_PARTY_LICENSES.md`
named by `docs/CORRECTNESS_RULES.md`. This is not a complete, internally
consistent distribution posture for an LGPL-2.1-or-later component.

### Required outcome

Have counsel or the responsible open-source compliance owner choose one model:

- Dynamically link and package/declare a compatible libnfs runtime dependency;
  or
- Keep static linking and ship all notices, license texts, corresponding source
  and/or durable source offer, build instructions, and relinkable material
  required for recipients to replace libnfs.

Then:

1. Make build comments, README, package metadata, and actual ELF linkage agree.
2. Include generated third-party license/notice material in the tarball, RPM,
   DEB, and release page/bare-binary distribution as required by the selected
   model.
3. Record libnfs source revision and patches reproducibly.
4. Add an artifact-content check so future releases cannot omit the required
   material.

### Acceptance evidence

- Written approval of the chosen distribution model from the responsible
  human owner.
- `readelf -d`/`ldd` evidence matches the documented static or dynamic model.
- Automated inspection of every artifact confirms required notices, license
  texts, source/relink information, and package license metadata.
- A clean-room relink or dynamic replacement exercise succeeds if required by
  the selected model.

---

## PR-11 — Establish a repeatable CI and release gate

Priority: **P1 / required before declaring production-ready**

Suggested lead: human release owner with LLM implementation support.

### Problem

There is no checked-in CI or release workflow. The documented release process
runs only `cargo test -p mongoose` before packaging.

During the readiness review:

- mongoose tests passed: 37 unit tests plus one integration test;
- migration-resync's eight tests passed;
- workspace Clippy with warnings denied passed;
- full workspace tests failed because two migration-worker tests reference a
  missing `examples/worker.toml`;
- `cargo deny check advisories` failed for RUSTSEC-2026-0258,
  RUSTSEC-2026-0285, RUSTSEC-2026-0098, RUSTSEC-2026-0099, and
  RUSTSEC-2026-0104, plus two unmaintained dependency advisories;
- several advisory paths enter mongoose through S3, HTTP, TLS, and coordinator
  dependencies even though mongoose claims not to run those subsystems;
- the pinned libnfs stage digest and the existing artifact's GLIBC 2.34 ceiling
  passed inspection.

### Required outcome

1. Add CI for formatting, Clippy with warnings denied, mongoose tests,
   migration-resync tests, full workspace tests, dependency advisories, license
   policy, and source policy.
2. Fix or deliberately remove the stale tests that require the absent
   `examples/worker.toml`; the full checked-in workspace must have a green
   default test command.
3. Add a reviewed `deny.toml`. Update vulnerable dependencies or feature-gate
   unused S3/coordinator/HTTP/TLS code out of the mongoose dependency graph.
   Do not waive an advisory merely because dead-code elimination is expected.
4. Build with `--locked`; verify the actual Rust, cargo-zigbuild, and Zig
   versions against `packaging/release-toolchain.lock.json` rather than merely
   recording them.
5. Build once per target and package that exact verified binary. Fix package
   architecture labels to derive from the build target rather than host
   `uname`/`dpkg` output.
6. Gate the artifact on maximum GLIBC symbol version, dynamic dependencies,
   version output, help smoke, package install/uninstall smoke, SHA-256 list,
   and required license contents.
7. Add signed checksums or equivalent release provenance so checksums fetched
   from the same release channel are not the only authenticity mechanism.
8. Keep real-NFS tests as an explicit protected hardware workflow. Record the
   server/environment, exact binaries, results, and approval for each release.

### Acceptance tests and evidence

- A clean clone passes the exact CI commands without untracked fixtures.
- The advisory and license gates are green under the checked-in policy.
- The portable artifact runs on the oldest declared GLIBC platform and an
  additional supported distribution.
- CPU requirements such as AES-NI are either removed, detected before use, or
  stated prominently and exercised on the oldest supported hardware/VM model.
- RPM, DEB, tarball, and bare binary all derive from the same digest-identified
  executable.
- The release workflow publishes checksums/provenance only after every software
  and hardware gate succeeds.

---

## Production release exit criteria

The release owner can remove the block only when all of the following are true:

- PR-01 through PR-10 are implemented and reviewed.
- CI from PR-11 is green from a clean clone at the release tag.
- Exclusion and scan-error hardware tests prove that no source subtree can be
  silently omitted.
- Cutover independently verifies the destination according to its final,
  accurately documented contract.
- Alias-based overlap tests prove no destination write occurs when endpoint
  separation cannot be established.
- Crash and concurrent-process tests protect work-dir state and pending
  failures.
- Torn-copy behavior is exercised on the mover configuration shipped by
  mongoose, not only on an unused engine path.
- The libnfs distribution model has explicit human approval and all artifacts
  contain the required compliance material.
- Dependency, portability, package, and real-NFS gates are attached to the
  release record.

Until then, release candidates may be useful for controlled lab evaluation,
but should not be described as production-safe or as proving source/destination
parity.
