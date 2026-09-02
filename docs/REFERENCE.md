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
```

The two most recent completed passes are kept; older pass dirs are
pruned when the baseline advances.

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

## Limitations (deliberate)

- Change detection is scan-diff on `(file_type, size, mtime, ctime)`,
  with whole-file recopy on any mismatch. A content rewrite with
  identical size inside the server's ctime granularity is missed. The
  cutover pass's zero-drift check is the convergence gate, not a
  byte-level verify.
- Deletions are recorded (`classify/deleted.jsonl`) but never
  propagated to the destination. Renames therefore copy as
  delete+create.
- Hardlink fidelity is shard-scoped: links that span shards copy as
  separate files.
- A directory whose children land in a different shard can end with a
  bumped mtime; the migration root itself is re-stamped at the end of
  a complete run.
- Single host, NFSv3 via libnfs only, Linux, root.

## Exit codes

| code | meaning |
|------|---------|
| 0    | success, including a deliberate SIGINT/SIGTERM stop (re-run to resume) |
| 1    | error (bad flags, missing index, unreachable export, corrupt shard) |
| 2    | copy completed but recorded per-file failures (see `failures/`) |

## Environment

- `RUST_LOG` overrides `-v` entirely (tracing-subscriber env-filter
  syntax).
- `LIBNFS_USE_ALL_RESERVED` is set to `1` by mongoose so libnfs uses
  the full reserved port range (without it `/etc/services` name
  registrations throttle a host to about 55 context pairs). Export it
  as `0` to opt out.
