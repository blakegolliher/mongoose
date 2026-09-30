# Special-file and type-safety implementation handoff

Status: **implemented in software on 2026-09-30; real-NFS qualification is
still outstanding and remains a release blocker.** The walker half is
nfs-walker PR #18 (commit `4dcaee499f6301673f97ad57c1d0e165a08cf29a`); the
mongoose half is the change that carries this status line, which pins that
walker commit. Neither has run against a real NFS server: see "Real-NFS
qualification" below.

The hardlink prerequisite below landed on both repositories' `main` branches
on 2026-09-30.

This is the next production-readiness slice after byte-safe paths and hardlink
safety. It closes two related correctness holes:

1. `mig-walker-rewrite` currently maps every walker type other than
   `"directory"` and `"symlink"` to a canonical regular file. A FIFO, socket,
   block device, character device, or unknown value can therefore be sent to
   the regular-file data path.
2. The mover's generic `Strategy::Skip` returns `Ok(())`. The shard processor
   consequently increments `files_ok` even though no destination entry was
   created.

The result must fail closed on an unrecognized type and must describe every
intentionally uncopied special entry truthfully and durably. This slice does
not create special nodes. It establishes the safe type boundary needed before
the walker can emit the complete canonical schema without the rewrite shim.

## Repository and merge order

This work crosses two repositories:

1. `nfs-walker`: resolve entries whose READDIRPLUS attributes are absent, and
   make the source file-type classification an explicit, regression-tested
   input contract.
2. `mongoose`: translate every supported type, reject unknown types, and
   replace the generic successful skip with an explicit omission outcome.

Merge the walker PR first with a regular merge commit. Then pin that reachable
walker commit in mongoose and update all release locks and evidence.

### Prerequisite: hardlink safety on `main` (resolved 2026-09-30)

Both hardlink PRs were first merged into their `byte-safe-paths` base
branches, not into `main`: nfs-walker PR #14 (hardlink commit
`86d7d0a8c9996ce9fe6a17e669a0e1f322b84d0e`) and mongoose PR #16 (hardlink
commit `b3fa17a928726bf3bb9ae3a3d86aecde89d8ce14`). Corrective PRs landed
both commits on `main` with regular merge commits, walker first:

- nfs-walker PR #15, merge `c94ead639790463f26333d7800f3b0ef63bfe941`;
- mongoose PR #17, merge `cd227752ce02ce27798b30030188fe45a3c55fc7`.

nfs-walker PR #16 (merge `3d33d8dca3deaf97c08e3a512c7e345a7e895cfa`) then
fixed pre-existing `cargo fmt` drift, so `cargo fmt --all --check` passes on
walker `main`.

Both of these commands exit 0:

```bash
# In nfs-walker after syncing main:
git merge-base --is-ancestor 86d7d0a8c9996ce9fe6a17e669a0e1f322b84d0e main

# In mongoose after syncing main:
git merge-base --is-ancestor b3fa17a928726bf3bb9ae3a3d86aecde89d8ce14 main
```

Cut the special-file branches from the current `main` branches, not from
`byte-safe-paths`, `hardlink-safety`, or an old worktree. When stacking PRs in
future, retarget the dependent PR to `main` before merging it.

## Locked behavior and terminology

These are implementation requirements, not open design questions.

- Supported canonical types are exactly `Regular`, `Dir`, `Symlink`, `Fifo`,
  `Socket`, `BlockDev`, and `CharDev`, with numeric values 1 through 7 from
  `SCHEMA_CONTRACT.md`.
- `Unknown = 0` is an in-memory sentinel only. It must never be written to a
  canonical shard or passed to the mover.
- An unknown walker string, an out-of-range numeric tag, a null required type,
  or disagreement between a type tag and `mode & S_IFMT` is shard corruption.
  Fail the rewrite or shard read with the row identified. Never guess
  `Regular`.
- Mongoose does not create FIFOs, sockets, block devices, or character devices
  in this slice. Doing so safely requires separate policy for privileges,
  device major/minor numbers, socket ownership, and destination-side effects.
- A recognized special row is a **processed omission**, not a copied file and
  not a transient copy failure. It advances shard progress, is not retried,
  moves zero bytes, does not increment `files_ok`, and produces a durable
  raw-path record.
- `mongoose copy` and a non-cutover sync may finish after reporting processed
  omissions. Their final summaries must prominently report the count and the
  record location. This preserves resumability instead of retrying an
  intentional omission forever.
- `mongoose sync --cutover` remains the release/cutover safety boundary. A
  missing destination special entry yields `special_not_copied` and a nonzero
  cutover result. If an operator safely recreates the same type on the
  destination with matching enforced attributes, namespace verification may
  pass that path. Removing the source entry also removes the blocker on the
  next complete scan.
- Raw path bytes are authoritative throughout. No new type-handling path may
  convert a path through UTF-8.

Use the term `special_not_copied` consistently in machine-readable records,
counters, logs, summaries, and verification output. Do not retain a generic
`Skip` name that could later hide a different unimplemented type.

## Verified starting state

### Walker already knows all seven NFS types

`nfs-walker`'s `EntryType` and READDIRPLUS handling already distinguish:

- regular file;
- directory;
- symlink;
- block device;
- character device;
- FIFO; and
- socket.

Its current `file_type` analytics column emits the exact strings `file`,
`directory`, `symlink`, `block_device`, `char_device`, `fifo`, `socket`, and
`unknown`. The values come from NFS attributes, not MIME sniffing, despite the
old `file_type_mime` terminology in the migration contract.

The legacy analytics enum numbers are not canonical numbers. In walker,
`File = 0`, `Directory = 1`, and so on; in the migration contract,
`Unknown = 0`, `Regular = 1`, and so on. Never serialize `EntryType as u8` into
the canonical `file_type` column.

### Walker emits `unknown` whenever READDIRPLUS omits attributes

`unknown` is not a hypothetical input. NFSv3 makes both the attributes
(`post_op_attr`) and the file handle (`post_op_fh3`) of a READDIRPLUS entry
optional. In `src/nfs/connection.rs`, an entry with `attributes_follow == 0`
becomes `(EntryType::Unknown, None)`, and there is no GETATTR or LOOKUP
fallback. `src/walker/simple.rs` then:

- decides whether to descend by `entry_type == EntryType::Directory`, so a
  directory returned without attributes is never traversed and its whole
  subtree is silently absent from the scan;
- writes the row with `file_type = "unknown"`, size 0, and null mode,
  ownership, times, and `fsid`.

Today's rewrite turns that row into an empty regular file, so the copy creates
a zero-byte file where a directory or special node should be. Failing the
rewrite on `unknown` alone would be safe but would block every migration from
a server that omits attributes for even one entry. The walker must resolve the
attributes instead; see step 1.

### The rewrite loses that information

In `crates/mig-walker-rewrite/src/lib.rs`,
`file_type_tag_from_mime()` recognizes only `directory` and `symlink` and maps
everything else to `Regular`. `translate_batch()` similarly uses `S_IFDIR`,
`S_IFLNK`, or `S_IFREG` only. The crate README documents this as a production
limitation.

The walker `permissions` column contains only permission/special bits masked
to `0o7777`; it does not contain `S_IFMT`. The rewrite must synthesize type bits
from the independently classified walker type.

### The mover calls an omission a success

`crates/migration-mover/src/strategy.rs` sends `Unknown`, FIFO, socket, and
both device types to `Strategy::Skip`. `Mover::execute()` returns a clean
zero-byte success for it. `ShardProcessor::record_outcome()` classifies every
`Ok(())` as `files_ok`. The local copy path then persists and prints that
inflated result.

`ShardReader` already rejects canonical `file_type` values outside `1..=7`.
Preserve that defense and add mode/type consistency validation.

### Verification is close, but its documentation is contradictory

Namespace verification already has `MismatchKind::SpecialNotCopied`. It emits
that mismatch when a special source entry has no destination entry. If a
destination entry exists with the same special type, `compare()` checks mode
and owner and can accept it.

`docs/REFERENCE.md` currently says special entries “can never match” and then
instructs operators to recreate them. The code implements the useful policy:
missing special entries block cutover, while correctly recreated entries can
match. Update the wording and add tests that lock it down.

## Scope and design

Implement this as a focused safety slice. Do not attempt the full native
canonical walker schema in the same PR. In particular, do not rename the
walker's analytics `path` or `file_type` columns here. The complete native
schema requires coordinated work on relative paths, `row_id`, Parquet footer
metadata, legacy dashboard queries, and deletion of the rewrite shim.

The existing walker strings contain enough type information to close the
production bug safely. This slice formalizes and exhaustively consumes that
interface. A later native-schema PR will replace the string translation with
canonical `mode: UInt32` and `file_type: UInt8` columns as specified in
`SCHEMA_CONTRACT.md`.

### Required translation table

The legacy rewrite must use this exact table:

| Walker `file_type` | Canonical tag | Canonical value | `mode` type bits |
| --- | --- | ---: | --- |
| `file` | `Regular` | 1 | `S_IFREG` |
| `directory` | `Dir` | 2 | `S_IFDIR` |
| `symlink` | `Symlink` | 3 | `S_IFLNK` |
| `fifo` | `Fifo` | 4 | `S_IFIFO` |
| `socket` | `Socket` | 5 | `S_IFSOCK` |
| `block_device` | `BlockDev` | 6 | `S_IFBLK` |
| `char_device` | `CharDev` | 7 | `S_IFCHR` |

`unknown`, the empty string, MIME-like values such as `text/plain`, different
case, and every other unrecognized string are errors. They must not fall back
to `Regular`.

For each output row:

```text
mode = (permissions as u32 & 0o7777) | type_bits
file_type = canonical_tag as u8
```

Validate that `mode & S_IFMT` agrees with `file_type` before writing the
canonical batch. The canonical shard reader must perform the same check at its
trust boundary so hand-built, stale, or malicious shards cannot bypass the
rewrite validation.

## Implementation sequence

### 1. Correct and lock the walker input contract

In nfs-walker:

1. Resolve every READDIRPLUS entry's attributes before classifying it, in
   this order:
   1. Use the READDIRPLUS attributes when present.
   2. If the attributes are absent but the entry's file handle is present,
      issue GETATTR on that handle.
   3. If the handle is also absent, issue LOOKUP with the parent directory's
      handle and the raw entry name bytes, then GETATTR on the returned handle
      if LOOKUP did not return attributes.
   4. Apply the existing bounded retry (`walker::retry::plan` and the scan's
      `RetryPolicy`) only to errors that `FailureKind` already classifies as
      transient. Do not add a separate retry policy.
   5. Treat an entry that is confirmed gone as **vanished**, the same way
      the walker already treats a directory that disappears mid-scan. When
      resolution returns `NotFound`, or a stale handle whose re-resolution
      finds nothing, confirm by resolving the entry's path from the export
      root, the same call that confirms a vanished directory
      (`Action::CheckGone` in `walker::retry`). A confirmed-gone
      entry is recorded with `FailureLog::record_vanished`, emits no row, is
      not descended into, and does not make the scan incomplete: on a live
      source it is a race, not a hole in the index. Without that
      confirmation the entry is unresolved.
   6. If the type still cannot be resolved, count a scan error: record a
      structured failure in the scan's failure log (`errors.jsonl`) and make
      the scan end in `WalkerError::ScanIncomplete` (the walker CLI exits 3).
      Mongoose's `scan.rs` already maps that error to an incomplete scan it
      refuses to consume. Do not emit an `Unknown` row, and do not descend
      speculatively into an entry that might be a directory.

   Resolve in the walker worker after the page is received, not inside the
   libnfs callback, and apply it in both READDIRPLUS consumer paths in
   `src/walker/simple.rs`. Resolved attributes replace every field the
   READDIRPLUS attributes would have supplied (type, size, mode, ownership,
   link count, times, and `fsid`), so a resolved row is indistinguishable from
   an ordinary one. A directory resolved this way is queued for traversal with
   its handle like any other directory. A resolved row takes its identity
   from the resolved attributes too: its `inode` is the resolved `fileid`,
   not the number in the READDIRPLUS entry. The two differ legitimately. The
   Linux server omits attributes and the handle for every mountpoint, and the
   LOOKUP then returns the root of the mounted filesystem, whose `fileid` is
   not the mounted-on directory's. Taking everything from one reply keeps the
   row about one object, the one the name refers to now. Do not compare the
   two numbers, and do not fail on a difference. Count fallback GETATTR and
   LOOKUP resolutions in the scan summary so operators can see a server that
   omits attributes.
2. Add a single typed conversion function from `EntryType` to the stable
   analytics string. Keep the existing strings and schema unchanged.
3. Ensure the Parquet builder uses that conversion and cannot obtain a type
   string from unrelated content/MIME logic.
4. Add a table-driven test containing all seven supported `EntryType` values.
   Assert the exact strings listed above.
5. `EntryType::Unknown` must never reach the Parquet builder. After step 1 it
   cannot arise from READDIRPLUS. Still, make the builder return an error, not
   panic, if it is handed one, and add a negative test for that. Never write
   the string `unknown` and continue.
6. Add a Parquet-builder test, not only a helper test. Decode the produced
   `file_type` column and prove the seven row values survive into the shard.
7. Keep the dashboard schema and query behavior unchanged.

### 2. Make the rewrite exhaustive and fail closed

In `crates/mig-walker-rewrite`:

1. Replace `file_type_tag_from_mime() -> FileTypeTag` with a fallible function
   that returns both the canonical tag and its `S_IFMT` bits, or use two
   functions sharing one exhaustive match. Rename it to reflect that these are
   walker entry-type strings, not arbitrary MIME values.
2. Include shard/file context and row offset in conversion errors. The final
   error should name the offending value without assuming it is safe or valid
   Unicode beyond the fact that the legacy Arrow column is `Utf8`.
3. Emit all seven canonical tags and matching mode bits from the table above.
4. Preserve the byte-safe `path_bytes` preference and the post-hardlink `fsid`
   handling. This PR must not regress either.
5. Update the rewrite README: remove the claim that special files become
   regular files; state that all seven known types are represented, unknown
   types fail the rewrite, and special nodes are reported but not created by
   mongoose.
6. Remove tests that bless “anything else means regular” and replace them with
   exhaustive positive and negative tests.

The rewrite must fail before atomically activating the output shard. Its
checkpoint/report must not mark a failed shard complete, and `--resume` must
rewrite it after the input is corrected.

### 3. Validate canonical type/mode agreement

In `migration-core`'s shard reader:

1. Keep rejecting tag 0 and values above 7.
2. Derive the expected `S_IFMT` value from the validated tag.
3. Reject a row when `mode & S_IFMT` differs from that expected value. Include
   `row_id`, the tag, the observed mode, and the expected type in the error.
4. Reject a mode with no recognized type bits. Do not repair it in the reader.

This is a trust-boundary check, not duplicated business logic. Put the mapping
next to `FileTypeTag` in `migration-core::schema` so the rewrite, reader, and
tests use one canonical definition where practical.

### 4. Replace generic `Skip` with an explicit special omission

In `migration-mover` and `migration-worker`:

1. Rename `Strategy::Skip` to `Strategy::SpecialNotCopied`, or introduce an
   equivalently explicit variant. It may be selected only for FIFO, socket,
   block device, and character device.
2. `Unknown`, `Regular`, `Dir`, and `Symlink` must never fall through to this
   branch. Use an exhaustive match so adding a future tag causes a compiler or
   test failure rather than a silent skip.
3. Keep the move result successful at the execution/retry layer so the shard
   can complete, but classify the result as an omission before the general
   `Ok(())` branch.
4. Add `DowngradeKind::SpecialNotCopied` and write one downgrade JSONL record
   for every omitted row. This is a non-fatal fidelity downgrade: its
   `path_b64` must round-trip the original bytes, and the record must carry the
   row and shard identifiers. Update the downgrade type/module documentation,
   which currently assumes the destination file always exists.
5. Add `files_special_not_copied` to `ProcessOutcome`. A special omission must
   increment that counter and `rows_done`, but not `files_ok`, `files_failed`,
   `files_fenced`, `bytes_moved`, or throughput.
6. Propagate the counter through `LivePending`, durable progress records, the
   local `CopyProgress`/`CopySummary`, shard-completion arithmetic, heartbeat
   snapshots, and terminal/log summaries. Add `#[serde(default)]` to persisted
   or wire structures so existing progress files remain readable.
7. Do not include the omission count in the backpressure failure ratio. It is
   neither an NFS failure nor something a retry can heal.
8. Ensure interrupted-row and completed-shard arithmetic includes omissions:

   ```text
   processed = files_ok + files_failed + files_special_not_copied
   ```

   Fenced rows retain their existing reclaim semantics.
9. Emit a prominent warning when a shard or run has omissions, including the
   durable downgrade directory/object location and the statement that cutover
   remains blocked until the entries are recreated or removed at source.

Using the downgrade sink is deliberate for this slice because it already has
the required per-shard durability ordering in local and distributed modes.
Do not put these rows in the failure sink: that would make an intentional,
non-retryable omission look transient and can create endless retries.

### 5. Lock down cutover behavior and operator documentation

In mongoose verification and documentation:

1. Test that a missing FIFO, socket, block device, or character device emits
   exactly one `special_not_copied` mismatch with the raw path preserved.
2. Test that a destination entry of the same special type is not reported as
   `special_not_copied`; normal enforced mode/owner checks still apply.
3. Test that a destination regular file at a source special path produces a
   `file_type` mismatch, not `special_not_copied` and not content work.
4. Keep cutover nonzero whenever `special_not_copied` is present.
5. Correct `docs/REFERENCE.md` to say missing special nodes are reported and
   block cutover, while correctly recreated nodes can match. Remove “can never
   match.”
6. Correct any broad README claim that “everything is preserved.” Say exactly
   that file contents, directories, symlinks, and configured metadata are
   copied; special nodes require operator action and are enforced at cutover.

Do not weaken full destination verification or add an option that ignores
special entries.

### 6. Pin and package the walker result

After the walker PR is merged to walker `main` with a regular merge commit:

1. Pin its full commit SHA in `crates/mongoose/Cargo.toml`.
2. Regenerate `Cargo.lock` without unrelated dependency updates.
3. Rebuild the portable embedded walker using the command recorded in
   `packaging/nfs-walker.lock.json`.
4. Update `source_git_sha`, `artifact_sha256`, and any changed version/evidence
   fields in that lock together.
5. Prove the pinned commit is an ancestor of walker `main`.
6. Run `make compliance-check` and the full release dry run. The static libnfs
   policy and exact-source/relink materials must remain intact.

The source bundle, SBOM, and provenance must identify the newly pinned walker
revision. A green unit-test run is not enough if the release lock or SBOM
still names the old commit.

The `artifact_sha256` values recorded for walker revisions up to `86d7d0a`
came from builds that used `-C target-cpu=native`, so they do not reproduce on
another machine. nfs-walker PR #17 replaces that flag with explicit
`+aes,+sse2`. Pin a walker commit that includes it, and expect the new digest
to differ from every earlier lock entry. The digest still depends on the build
account's `~/.cargo` and `~/.rustup` paths, so treat it as evidence from one
machine and account, not as a cross-machine gate.

## Required tests

### nfs-walker

- table test for all seven `EntryType` to string mappings;
- a decoded Parquet batch containing all seven types;
- regression: a directory whose READDIRPLUS entry has no attributes is
  resolved by GETATTR, emitted as `directory`, and traversed; the test must
  assert that the directory's descendants appear in the output;
- the same directory case with the handle also absent, resolved through
  LOOKUP and traversed;
- a non-directory resolved through fallback has the correct type, size,
  mode, ownership, and `fsid`;
- a transient GETATTR or LOOKUP error is retried within the existing bound and
  then succeeds, while a non-transient error is not retried;
- an entry deleted before it can be resolved, and confirmed gone by LOOKUP,
  is recorded as vanished, emits no row, and leaves the scan complete;
- a `NotFound` that the confirming LOOKUP contradicts (the name still
  exists) is unresolved, not vanished;
- an entry that stays unresolved records a structured failure, ends the scan
  in `ScanIncomplete`, emits no `Unknown` row, and is not descended into;
- an entry whose resolved `fileid` differs from the READDIRPLUS entry's, as
  a mountpoint's does, is resolved, and its row carries the resolved `inode`
  and `fsid`;
- the builder returns an error, rather than writing a row, when handed
  `EntryType::Unknown`;
- byte-path and fsid regression tests still pass;
- all-feature and no-default-feature builds/tests pass;
- dashboard query tests remain unchanged and green.

### Rewrite and canonical reader

- all seven legacy walker strings produce the exact canonical tags and mode
  type bits;
- permission bits survive unchanged under `mode & 0o7777`;
- `unknown`, empty, MIME-looking, case-changed, and arbitrary strings fail;
- canonical tags 0 and greater than 7 fail;
- every tag/mode mismatch fails, including “special tag plus `S_IFREG`” and
  “regular tag plus `S_IFIFO`”;
- a failed rewrite does not activate a partial shard or complete its report;
- non-UTF-8 `path_bytes` remain byte-identical in positive and error records;
- fsid remains populated when the new walker column is present;
- an old, valid walker shard remains readable through the compatibility path.

### Mover and worker

- each of the four special types selects only `SpecialNotCopied`;
- regular, empty regular, directory, symlink, and hardlink rows retain their
  existing strategies;
- no test may construct an `Unknown` row and expect a skip;
- a special row performs no destination RPC and moves zero bytes;
- it emits one `SPECIAL_NOT_COPIED` record with byte-exact `path_b64`;
- it increments `files_special_not_copied` and `rows_done`, not `files_ok` or
  `files_failed`;
- it does not affect throughput or failure-rate backpressure;
- shard completion, interruption reporting, resume, local progress, and
  distributed heartbeat serialization include the new counter without
  double-counting;
- old progress JSON without the counter deserializes as zero.

### End-to-end mongoose regression

Build a synthetic walker Parquet shard containing at least one regular file
and all four special types, including one non-UTF-8 special path. Prove:

1. prepare/rewrite emits correct canonical types and modes;
2. copy creates the regular file and no special entries;
3. copy reports one success and four `special_not_copied` omissions, not five
   successes;
4. the durable records preserve all paths byte-for-byte;
5. the shard is complete and a rerun does not retry it;
6. cutover reports four `special_not_copied` mismatches and exits nonzero;
7. a same-type destination fixture removes the corresponding omission from
   verification, while a wrong-type fixture yields `file_type`.

Tests must not require root or create real device nodes. Construct canonical
Parquet rows and verifier entries directly for block/character-device cases.

## Negative tests that must be demonstrated

Before requesting review, deliberately prove these fail:

1. Restore the old rewrite wildcard (`_ => Regular`): an exhaustive type test
   must fail.
2. Change one special tag's synthesized mode to `S_IFREG`: the reader or
   rewrite consistency test must fail.
3. Make `SpecialNotCopied` fall through the generic success classifier: the
   counter test must catch `files_ok == 1`.
4. Remove the durable record call: the end-to-end test must fail.
5. Supply canonical `Unknown = 0`: shard reading must fail before mover
   dispatch.
6. Remove the walker's GETATTR/LOOKUP fallback, restoring
   `(EntryType::Unknown, None)`: the directory-without-attributes regression
   test must fail because the subtree is missing.

Record the exact test names and failure excerpts in the PR report. Revert each
mutation before committing.

## Validation commands

Use repository-local instructions if they have become stricter. At minimum:

### nfs-walker

```bash
cargo fmt --all --check
NFS_WALKER_LIBNFS_DIR=/absolute/path/to/mongoose/packaging/libnfs-stage \
  cargo clippy --all-targets --all-features -- -D warnings
NFS_WALKER_LIBNFS_DIR=/absolute/path/to/mongoose/packaging/libnfs-stage \
  cargo test --all-features
NFS_WALKER_LIBNFS_DIR=/absolute/path/to/mongoose/packaging/libnfs-stage \
  cargo test --no-default-features
scripts/check-attribution.sh commits origin/main..HEAD
```

Also build the locked portable walker with the exact target and toolchain from
mongoose's `packaging/nfs-walker.lock.json`, record its SHA-256, and verify its
maximum GLIBC symbol remains 2.34.

### mongoose

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo deny --locked check advisories licenses bans sources
shellcheck scripts/*.sh packaging/relink-kit/*.sh
make compliance-check
scripts/check-attribution.sh commits origin/main..HEAD
```

Then run the release workflow's read-only dry run or the documented local
equivalent from a clean committed revision. It must rebuild and gate the exact
artifact set, including SBOM validation and the LGPL release gate. Do not
create a release tag or publish anything for this work.

## Real-NFS qualification

The synthetic tests are merge gates. Real-NFS qualification remains an
explicit release blocker and needs a writable source export and a destination
export owned by this project.

On hardware:

1. Create a regular control file and a FIFO on the source. Create a bound Unix
   socket pathname if the NFS server/protocol exposes it. Test device nodes
   only on an approved environment with the required privileges; never use
   another operator's export.
2. Scan and inspect the walker Parquet output. Confirm each available special
   node has the expected exact string, permissions, raw path, inode, and fsid.
3. Prepare and inspect the canonical shard. Confirm tag and `S_IFMT` agree.
4. Copy. Confirm the control file is created, special nodes are absent, no
   source special node was opened as a regular data file, counters are
   truthful, and durable omission records decode to exact path bytes.
5. Run cutover and confirm it fails only for the absent special nodes.
6. Where safe, manually recreate a same-type FIFO on the destination with the
   enforced mode/owner and confirm the next cutover no longer reports that
   path as `special_not_copied`.

Preserve a qualification record containing repository commits, binary and
walker digests, server/export identities, commands, decoded type evidence,
counters, cutover result, and any server limitation. Hardware qualification
must be tied to the exact release candidate; an earlier build is not enough.

## Non-goals

- Creating FIFOs, sockets, or device nodes automatically.
- Capturing device major/minor numbers.
- Copying socket runtime state.
- Adding a flag to ignore special entries at cutover.
- Treating unknown types as regular files or as successful omissions.
- Completing the full native canonical walker schema or deleting the rewrite
  shim.
- Xattr/ACL preservation.
- Changing libnfs linkage, the static-link policy, release signing, or
  provenance design.
- Publishing a release or creating a `v*` tag.

## Required implementation report

Return all of the following before owner review:

- walker PR URL, head commit, regular merge commit, and proof the head is
  reachable from walker `main`;
- mongoose PR URL and head commit;
- the final exhaustive mapping and where its single source of truth lives;
- before/after examples for FIFO, socket, block device, character device,
  unknown input, a directory returned without READDIRPLUS attributes, and a
  non-UTF-8 path;
- exact counter and durable-record behavior;
- all validation command results and test totals;
- negative-test evidence;
- old and new pinned walker revisions and artifact digests;
- `make compliance-check` and full release dry-run evidence from the clean
  committed revision;
- hosted CI status;
- real-NFS qualification evidence, or an explicit statement that it remains a
  release blocker because no approved exports were available;
- confirmation that no release tag was created and nothing was published.

Do not describe the project or artifact as production-releasable solely
because this slice is green. The exact release candidate still needs the full
software release gate and the recorded real-hardware qualification.
