# mongoose reference

The README covers everyday use. This is the rest: what mongoose keeps
on disk, how resume works, what it guarantees, and what it does not do.

## How a copy works

`mongoose copy` runs three stages, each checkpointed under the work dir:

1. **Scan** the source with the embedded nfs-walker (metadata only).
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
  downgrades/part-NNNN.jsonl    per-file metadata downgrades, per shard
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
- Re-running `sync` resumes the in-flight pass at the same
  granularity. The baseline advance is the commit point; a pass
  interrupted before it re-runs against the old baseline, which at
  worst recopies redundantly.
- SIGINT/SIGTERM stops at the next batch boundary; no file is ever
  interrupted mid-copy. A second signal is ignored (SIGKILL abandons
  the batch).
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
- Source/destination overlap is refused at `copy` (same server, same
  path or nested either way) and re-checked at every copy from the
  recorded manifest.
- Owner, mode, and times are preserved. A source attribute the
  destination will not accept (for example chown without root on the
  destination) is recorded as a downgrade, not a failure.
- Hardlink groups copy sequentially within a shard micro-batch; the
  first row is fully published before the rest link to it.
- Failures and downgrades are separate JSONL streams, written after
  every shard.

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
| fifo, socket, device | reported as `special_not_copied`; mongoose does not copy them, so they can never match. Recreate them on the destination or remove them from the source |

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
- Fifos, sockets, and device nodes are not copied; cutover reports
  them.
- Single host, NFSv3 via libnfs only, Linux, root.

## Exit codes

| code | meaning |
|------|---------|
| 0    | success, including a deliberate SIGINT/SIGTERM stop (re-run to resume) |
| 1    | error (bad flags, missing index, unreachable export, corrupt shard), or a `--cutover` whose verification found mismatches |
| 2    | copy completed but recorded per-file failures (see `failures/`) |

## Environment

- `RUST_LOG` overrides `-v` entirely (tracing-subscriber env-filter
  syntax).
- `LIBNFS_USE_ALL_RESERVED` is set to `1` by mongoose so libnfs uses
  the full reserved port range (without it `/etc/services` name
  registrations throttle a host to about 55 context pairs). Export it
  as `0` to opt out.
