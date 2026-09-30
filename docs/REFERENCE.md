# mongoose reference

The README covers everyday use. This is the rest: what mongoose keeps
on disk, how resume works, what it guarantees, and what it does not do.

## How a copy works

`mongoose copy` runs three stages, each checkpointed under the work dir:

1. **Scan** the source with the embedded nfs-walker (metadata only).
   The scan is checkpointed only if every directory was read; see
   [Scan completeness](#scan-completeness).
2. **Index**: rewrite the scan into canonical parquet shards and write
   `manifest.json`. The raw scan output is deleted once the index is
   verified.
3. **Copy** the shards in order. Shards are processed one at a time;
   files within a shard copy concurrently, with in-flight limits per
   size class (small < 1 MiB, medium up to 1 GiB, large above) scaled
   from `--parallel`.

`mongoose sync` runs one converging pass: rescan, rewrite (the rescan
becomes the next baseline), classify every entry against the previous
baseline on `(file_type, size, mtime, ctime)`, emit the new and changed
rows as delta shards, copy them with the same copy loop, then advance
the baseline. Rows that failed or tore in the previous pass are forced
into the delta until they succeed. Design notes:
[work-items/MONGOOSE_RESYNC.md](work-items/MONGOOSE_RESYNC.md).

`mongoose sync --cutover` runs the same rescan and classification as
a gate (the source must show nothing to copy and no deletions), then
scans the destination and verifies it against the source index; see
[Cutover verification](#cutover-verification). Only a clean
verification advances the baseline.

## Work-dir layout

```
<work-dir>/
  run.json                      job identity: source, dest, excludes
                                (a re-run with different values is refused)
  scan.json                     scan checkpoint
  canonical/part-NNNN.parquet   canonical shards (the index)
  rewrite.json                  rewrite checkpoint
  manifest.json                 run plan (shard list, totals)
  progress.json                 copy progress + completed-shard list
  failures/part-NNNN.jsonl      per-file failures, per shard
  downgrades/part-NNNN.jsonl    per-file metadata downgrades, per shard, and
                                one SPECIAL_NOT_COPIED record for every fifo,
                                socket, or device node that was not copied
  baseline.json                 which pass the next sync diffs against
  passes/pass-NNNN/             one sync pass: same layout as above, plus
    classify/                   keep lists, deleted.jsonl, classify.json
    delta-manifest.json         the new+changed subset that was copied
    dest/                       (--cutover) the destination index: scan
                                checkpoint, canonical shards, manifest
    verify/                     (--cutover) namespace.json, the content
                                work list and its progress checkpoint,
                                mismatches.jsonl (every mismatch)
    verify.json                 (--cutover) the verification report
  passes/pass-NNNN-failed-UTC/  a cutover pass whose verification
                                failed, moved aside with its evidence
```

The two most recent completed passes are kept; older pass dirs are
pruned when the baseline advances. Failed cutover passes are never
pruned; delete them by hand once their reports are no longer needed.

## Resume

- Re-running `copy` skips a completed scan, resumes the rewrite, and
  skips every shard listed in `progress.json`. An interrupted shard is
  reprocessed from its first row; copies are idempotent (`.partial`
  then rename), so this is safe.
- A scan attempt that could not read every directory is never reused:
  the next run scans afresh into a new `scan/attempt-NNNN/`. The
  failed attempt stays on disk as evidence.
- Re-running `sync` resumes the in-flight pass at the same
  granularity. The baseline advance is the commit point; a pass
  interrupted before it re-runs against the old baseline, which at
  worst recopies redundantly.
- SIGINT/SIGTERM stops at the next batch boundary; no file is ever
  interrupted mid-copy. The first handled signal determines the exit
  status (SIGINT `130`, SIGTERM `143`); later signals do not change it.
- Re-running `sync --cutover` after an interruption reuses both scans
  and resumes the content read-back at its last durable checkpoint
  (`verify/content-progress.json`, written every 15 seconds). A
  cutover whose verification *failed* is not resumed: its pass dir is
  moved to `passes/pass-NNNN-failed-<utc>/` and the next run starts
  pass NNNN afresh, so a fix on either side is re-scanned.
- `--parallel` is not part of the job identity; change it freely
  between runs.

## Correctness posture

- Regular files are copied over the raw NFSv3 filehandle path: attrs
  stamped at CREATE, FILE_SYNC writes, one SETATTR for times, then an
  atomic `.partial` to final-name RENAME. A crash can never leave a
  torn file visible under its final name.
- Each regular-file read is bracketed by source attributes taken from the
  exact open filehandle. The default raw path issues NFSv3 GETATTR before and
  after the read; the path-based fallback uses `nfs_fstat64`. Size, mtime, and
  ctime (including nanoseconds) must all remain stable. A missing post-stat
  fails the row without publishing it. A changed bracket may still publish
  under the at-least-once policy, but it increments `files_torn`, writes a
  durable `TORN_COPY` downgrade, and forces the path into the next sync even
  when its later scan tuple matches. Cutover refuses unresolved torn rows.
- This safety check costs two source metadata RPCs per non-empty regular file.
  It is intentional for the synchronous/raw mover mongoose ships; release
  qualification must include the raw-mover throughput gate on real hardware.
- Source/destination overlap is refused at `copy` (same server, same
  path or nested either way) and re-proved on every `copy` and `sync`
  from the recorded manifest, in three layers; see
  [Endpoint separation](#endpoint-separation).
- Owner, mode, and times are preserved. A source attribute the
  destination will not accept (for example chown without root on the
  destination) is recorded as a downgrade, not a failure.
- Hardlink groups copy sequentially within a shard micro-batch; the
  first row is fully published before the rest link to it.
- Failures and downgrades are separate JSONL streams, written after
  every shard.
- A fifo, socket, or device node is recognized and **not copied**.
  It is neither a copied file nor a failure: `copy` and `sync` count
  it as `special NOT copied`, record it in
  `downgrades/part-NNNN.jsonl` as `SPECIAL_NOT_COPIED` with the node
  kind and the raw path (base64), complete the shard, and do not
  retry it. `progress.json` carries the total as
  `files_special_not_copied`. The final summary prints the count and
  where the records are. `copy` and a plain `sync` still exit 0;
  `sync --cutover` fails while any such node is missing from the
  destination.
- An entry whose type the scan could not establish is never copied as
  a file and never skipped: the scan fails as incomplete. A shard
  whose type and mode disagree is rejected as corrupt.

## Excludes

`--exclude GLOB` names directories to leave out of the job. The
contract:

- A pattern is a glob matched against a directory's **own name**:
  `*` any run of characters, `?` one character, `[...]` a class
  (`[!...]` negated), `\x` a literal `x`. `.snapshot` matches a
  directory called exactly `.snapshot`, not `mysnapshots`; `*.tmp`
  matches `build.tmp`. It is never matched against a path (a pattern
  containing `/` is refused) and never against a file.
- A matching directory is dropped where its parent is listed: its own
  row is not emitted and it is not descended into, so the whole
  subtree is absent from the index.
- The set is job identity, recorded in `run.json` at `copy` and fixed
  for the life of the job. Every scan applies it unchanged: the
  initial copy, every sync, and the cutover's destination scan, so an
  excluded tree is absent from both indexes and never shows up as
  new, deleted, missing, or extra.
- An invalid pattern is refused before anything is mounted or
  written, by the same compiler the walker uses to apply it.

The walker's own `--exclude` flag is a regular expression over the
full path and is not what mongoose passes; mongoose uses the walker's
`--exclude-dir`, which is the glob above.

## Scan completeness

An index with a missing subtree is worse than no index: the copy would
skip the subtree, every later sync would classify the source as
unchanged there, and a cutover could pass without it. So a scan counts
only when the walker read every directory.

- The walker retries transient failures (timeouts, connection or RPC
  trouble, "try again" from the server, stale handles, which are
  re-resolved by path first) with exponential backoff, up to three
  retries per directory, and only while nothing from that directory
  has been written to the index, so a retry can never duplicate rows.
- A directory that disappears between its parent's listing and its
  own read is confirmed by LOOKUP and recorded as **vanished**. On a
  live source that is a race, not a hole: its own entry was already
  listed, and the next sync sees whatever replaced it. It is not an
  error, and it cannot happen once writers are stopped.
- Permission denials and other non-transient errors fail the
  directory at once.

Any directory still unreadable after that fails the scan. mongoose
keeps the attempt directory (`scan/attempt-NNNN/`: the part files
written so far, `walker-progress.jsonl`, and
`walk.parquet/scans/<id>/errors.jsonl` with one JSON record per
unreadable or vanished directory), records the attempt in `scan.json`
with `complete: false` and the counts, prints the first ten
unreadable directories with their error and attempt count, and exits
1. Nothing else changes: no index, no manifest, no baseline advance,
no cutover result. The next `copy`, `sync`, or `sync --cutover` scans
afresh into a new attempt directory. There is no flag to accept an
incomplete scan.

## Endpoint separation

A destination that is the source, or inside it, truncates source
files (the mover creates with `O_TRUNC`). Two URL strings that differ
are never taken as proof of two servers. Separation is established in
three layers, each on every run:

1. **Canonical spelling.** Host names are lowercased and stripped of
   a trailing dot; IP literals take one text form (an IPv4-mapped
   IPv6 literal is its IPv4 address); the default port 2049 is the
   absent port; libnfs `?options` are sorted and never part of
   identity. Two URLs with one canonical host are one server, whatever
   their port or options say, and their paths must be disjoint (not
   equal, neither a prefix of the other).
2. **Name resolution.** Two different names are resolved; if their
   address sets intersect they are one server and the same path rule
   applies. A name that does not resolve is an error, never
   "different".
3. **Mounted identity** (`copy`, the `sync` delta copy, and the
   cutover read-back — everything that mounts). For each mounted root
   mongoose records the connected peer address, the root filehandle
   from MNT, the root's `(fsid, fileid)`, and its ancestor chain
   (LOOKUP `..` until the server returns the same object, an error,
   or 4096 steps). The two roots are compared for equality and for
   one being an ancestor of the other: by filehandle bytes on any
   server, and by `(fsid, fileid)` once the servers are known to be
   one (same name, shared address, or same peer). A filehandle match
   between apparently different servers is settled by a probe: an
   empty directory is created under the destination root and looked
   up on the *source* connection through the destination's own
   filehandle, then removed. Visible means one server (refuse);
   ENOENT means two servers that happen to hand out equal bytes
   (cloned images do); anything else means separation cannot be
   proved, and the job is refused. On one server the copy also
   searches the source index for a directory whose fileid is the
   destination root's and confirms it by LOOKUP, which catches a
   destination export that is a bind mount or second export of a
   directory inside the source tree even when `..` cannot leave the
   export.

The per-file self-target check in the mover stays armed whenever the
two servers may be one, and compares server-absolute paths (export +
path), so two mounts of one server compare correctly. There is no
flag that disables any of this; the recorded evidence is printed as
`endpoints:` at the start of every copy and verification.

Not covered: a *source* export that is a differently named alias of a
directory inside the destination tree. The copy then writes beside
the source rather than over it, and the per-file check still refuses
any path collision it can see. Sibling paths on one server, and two
distinct exports on one server, are allowed once proved disjoint.

## Cutover verification

A clean `mongoose sync --cutover` is the statement "the destination
matches the source". It is proved in two independent gates, and the
baseline advances only when both pass:

1. **Source quiescence.** The rescan classifies against the previous
   baseline and must find nothing to copy (no NEW, DIRTY, or pending
   rows) and no deletions. This proves the last sync caught every
   change and that no failed or torn copy is outstanding. It says
   nothing about the destination.
2. **Destination verification.** The destination is scanned with the
   same embedded walker and the same excludes, rewritten to canonical
   shards under `passes/pass-NNNN/dest/`, and joined against the
   source index by path. Then every file is read in full from both
   servers and its SHA-256 compared, and every symlink is READLINKed
   on both sides. The read-back uses one libnfs context pair per
   `--parallel`, so it costs roughly one full read of the tree from
   each server.

What is compared, per entry present in the source index:

| entry     | compared |
|-----------|----------|
| file      | present on the destination, same type, size, mode bits (`mode & 07777`), owner, mtime (to the microsecond `utimes` carries), and SHA-256 of the bytes |
| directory | present, same type, mode bits, owner |
| symlink   | present, same type, target bytes |
| fifo, socket, device | mongoose does not copy these. One that is missing from the destination is reported as `special_not_copied` and fails the cutover. One you recreated on the destination with the same type matches, and is then checked like a directory: type, mode bits, owner. A different type at that path is a `file_type` mismatch. To clear the report, recreate the node on the destination or remove it from the source |

and every destination path must exist in the source; an extra fails,
including entries the no-delete policy left behind and any stale
`.partial` file from an interrupted copy.

Mode, owner, and mtime are compared only when the job preserved them
(all three are on by default); the report records which were in
force. A source attribute the walker could not read (null uid, gid,
or mtime) was never applied and is not compared. A metadata mismatch
does not suppress the content read, so one run reports everything.

Deliberately outside the contract, because the copy engine does not
guarantee them:

- directory mtimes (any later commit into a directory bumps it; the
  final restamp is best-effort);
- atimes;
- hardlink topology (links that span shards copy as separate files;
  the bytes at each path are still verified);
- a symlink's own mode, owner, and times (NFSv3 has no lchmod, and
  lutimes is best-effort);
- the migration root's own attributes (the walker emits no row for
  it; both roots were scanned, so both exist).

On failure the pass dir is moved to `passes/pass-NNNN-failed-<utc>/`
and the command exits 1 without advancing the baseline. Inside it,
`verify.json` holds the status, the contract in force, per-kind counts
(`missing`, `extra`, `special_not_copied`, `file_type`, `size`,
`mode`, `owner`, `mtime`, `symlink_target`, `content`, `read_error`),
and the first 100 mismatch records; `verify/mismatches.jsonl` holds
every record, one JSON object per line with `path_b64` (lossless),
`path_lossy` (display only), `kind`, and `expected`/`actual`/`detail`
where they apply. A `read_error` means one side could not be read and
nothing was proved about that path. The report's `mode` field is
`full`; there is no sampled or metadata-only variant.

Preconditions the tool cannot check: source writers must really be
stopped, and the destination must not be written by anything else.
A file created on the source after the rescan is in neither index and
is not detected.

## Limitations (deliberate)

- Change detection is scan-diff on `(file_type, size, mtime, ctime)`,
  with whole-file recopy on any mismatch. A content rewrite with
  identical size inside the server's ctime granularity is missed by
  `sync`; it is caught by the byte verification of `--cutover`.
- Deletions are recorded (`classify/deleted.jsonl`) but never
  propagated to the destination. Renames therefore copy as
  delete+create.
- Hardlink fidelity is shard-scoped: links that span shards copy as
  separate files. Cutover verifies the bytes at every path, not the
  sharing.
- A directory whose children land in a different shard can end with a
  bumped mtime; the migration root itself is re-stamped at the end of
  a complete run. Directory mtimes are outside the cutover contract.
- Fifos, sockets, and device nodes are not copied. Every one is
  counted and recorded (see [Correctness posture](#correctness-posture)), and cutover fails until it
  exists on the destination with the same type or is gone from the
  source. mongoose never creates one: doing that safely needs decisions
  about privileges, device numbers, and socket ownership that belong to
  the operator.
- Single host, NFSv3 via libnfs only, Linux, root.

## Exit codes

| code | meaning |
|------|---------|
| 0    | success |
| 1    | fatal error (bad flags, missing index, unreachable export, corrupt shard), or a `--cutover` whose verification found mismatches |
| 2    | copy completed but recorded per-file failures (see `failures/`) |
| 130  | interrupted by handled SIGINT; re-run to resume |
| 143  | interrupted by handled SIGTERM; re-run to resume |

Fatal errors take precedence over other outcomes. When a signal interrupts
work, its status takes precedence over per-file failures already recorded in
the incomplete pass. Exit 2 applies only after the pass completes. A sync
baseline advances only after delta copy and requested cutover verification
complete, so an interrupted pass remains based on the previous baseline.

## Environment

- `RUST_LOG` overrides `-v` entirely (tracing-subscriber env-filter
  syntax).
- `LIBNFS_USE_ALL_RESERVED` is set to `1` by mongoose so libnfs uses
  the full reserved port range (without it `/etc/services` name
  registrations throttle a host to about 55 context pairs). Export it
  as `0` to opt out.
