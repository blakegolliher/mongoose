# mig-walker-rewrite

A throwaway pre-flight shim that converts an `nfs-walker` parquet
output directory to the canonical migration schema defined in
`migration/SCHEMA_CONTRACT.md`. It exists so M2/M3 manual verification
can proceed against real VAST hardware today, before walker is updated
to emit the canonical schema natively.

This crate will be **removed** when walker emits canonical schema
natively. Do not build long-lived workflows on top of it.

## Usage

```text
# Walker output root (auto-descends into scans/<scan_id>/):
mig-walker-rewrite \
    --input  /path/to/walker-output/ \
    --output /path/to/canonical-shards/ \
    --source-root /bgolliher/vamoose-source

# Or point directly at the scan subdirectory:
mig-walker-rewrite \
    --input  /path/to/walker-output/scans/<scan_id>/ \
    --output /path/to/canonical-shards/ \
    --source-root /bgolliher/vamoose-source
```

`--input` accepts either form. If an output root contains more than
one `scans/<scan_id>/` subdirectory, the shim refuses to guess and
requires `--input` pointed at a specific scan.

`--source-root` is the export root that the walker scanned. Walker
emits absolute paths (e.g. `/bgolliher/vamoose-source/m2-verify/file.bin`);
the canonical schema requires paths relative to the export root with a
leading slash (`/m2-verify/file.bin`). The shim strips this prefix.
If a walker path does not begin with `--source-root`, the shim refuses
the shard rather than emit a corrupted path.

Current walker shards also contain an authoritative Binary `path_bytes`
column. The shim prefers it and preserves arbitrary POSIX filename bytes.
Older shards without that column remain readable through the legacy UTF-8
`path` column.

Output filenames mirror input filenames. Shard indices for `row_id`
materialization are assigned in lexicographic order of input
filenames; running the shim twice on the same input produces
byte-identical canonical `row_id` values.

After running the shim, operators run `aws s3 cp --recursive` on the
output directory themselves. This tool deliberately does not handle
S3, manifest generation, or filtering — it is purely a schema
translator.

## Entry types

The walker's `permissions` column (`UInt16`) holds permission bits
only. The canonical `mode` (`UInt32`) also carries the type in its
`S_IFMT` bits, so the shim derives both the canonical `file_type` tag
and those bits from the walker's `file_type` string. All seven entry
types are represented:

| Walker `file_type` | Canonical tag | Value | `mode` type bits |
|---|---|---:|---|
| `file` | `Regular` | 1 | `S_IFREG` |
| `directory` | `Dir` | 2 | `S_IFDIR` |
| `symlink` | `Symlink` | 3 | `S_IFLNK` |
| `fifo` | `Fifo` | 4 | `S_IFIFO` |
| `socket` | `Socket` | 5 | `S_IFSOCK` |
| `block_device` | `BlockDev` | 6 | `S_IFBLK` |
| `char_device` | `CharDev` | 7 | `S_IFCHR` |

`mode = (permissions & 0o7777) | type bits`. The table has one
definition, `FileTypeTag::from_walker_file_type` in `migration-core`.

**Any other value fails the rewrite**: `unknown`, the empty string, a
null, a different case, a MIME-style value such as `text/plain`. There
is no fallback type. The error names the shard, the row, the path, and
the value; the shard is not activated and not checkpointed, and
`--resume` rewrites it once the input is corrected.

Fifos, sockets, and device nodes are **represented, not created**. The
mover recognizes them, records each as `SPECIAL_NOT_COPIED`, and leaves
them out; cutover verification reports any that are missing from the
destination.

## Limitations (do not fix; wait for native walker support)

The shim carries the walker's nullable `fsid` into canonical output. Older
walker shards without that column remain readable with `fsid = null`; the
mover copies their affected hardlink entries independently rather than risk
grouping equal inode numbers from different filesystems. `symlink_target` and
`xattr_blob` remain null: the former triggers a `READLINK` round-trip during
copy, while the latter means xattrs are not preserved.

## Sunset

This crate will be removed when `nfs-walker` emits canonical schema
natively. Track the deletion in the walker canonical-schema PR.
