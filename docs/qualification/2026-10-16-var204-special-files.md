# Special-file real-NFS qualification: var204, 2026-10-16

This is the hardware run of the procedure in
`docs/SPECIAL_FILE_TYPE_SAFETY_HANDOFF.md` ("Real-NFS qualification"). It
**passed** for FIFOs and sockets.

- **Device nodes:** not tested.
- **Non-UTF-8 names:** the server refused to create them, so they are not
  tested either.
- **Scope:** the binary was a local build of `main`. The release
  qualification (11C-4 in `docs/PR_11C_RELEASE_HARDENING_HANDOFF.md`) must
  repeat the procedure with the attested release binary.

## What ran

| | |
|---|---|
| mongoose | `main` at `63301e601b2d3643850de395d6de57f28c4230a8` (PR #28 merged) |
| Binary | `make build-portable`. SHA-256 `6d1d8d8f57a4c48bb4c76c1064e035e139594b901a131a5c4ee8ba1da5d03234`. Max glibc symbol `GLIBC_2.34`. No dynamic libnfs. |
| Toolchain | Rust 1.98.0, cargo-zigbuild 0.20.1, Zig 0.16.0 (`/snap/zig/16117`) |
| Embedded walker | nfs-walker `f6f75725d8014a3c3bd943f5dd3650aef54a9cd7`. Lock digest of the portable walker: `792552c9c3f21f7d6c7fcf4fdbaea4152f8f082adbf690a7c43f876b54953921`. |
| Client | Ubuntu 24.04, kernel 6.12.41, 8 vCPUs, run as root |
| Server | VAST cluster var204, `main.selab-var204.selab.vastdata.com`, NFSv3. DNS round robin over 172.200.204.1 to .6. |
| Source | `nfs://main.selab-var204.selab.vastdata.com/source/special-qual`. Endpoint identity `1:8194859080173691964`. |
| Destination | `nfs://main.selab-var204.selab.vastdata.com/dest/special-qual`. Endpoint identity `1:15820436802150030058`. |
| Work dir | `/var/lib/mongoose/special-qual-2`, job `run-20261016T141132Z` |

The separation proof for both runs: "same host name; both connections reach
172.200.204.1; one server; ancestry of both roots walked to the top without
meeting".

## 1. Source tree

The tree was created through a kernel mount of `/source` (`vers=3`) with
[`tools/make-special-tree.py`](tools/make-special-tree.py):

```
sudo python3 -I tools/make-special-tree.py /mnt/source/special-qual
```

| Entry | Type | Mode | Owner |
|---|---|---|---|
| `control.txt` | regular (44 bytes) | 0644 | 1001:1001 |
| `data.bin`, `data-link.bin` | regular, hardlinked (one inode, nlink 2) | 0640 | 1002:1002 |
| `link-to-control` | symlink to `control.txt` | | |
| `dangling-link` | symlink to `missing-target` | | |
| `nested/` | directory | 0755 | |
| `pipe.fifo` | FIFO | 0640 | 1003:1004 |
| `nested/inner.fifo` | FIFO | 0644 | 1006:1006 |
| `sock.sock` | bound Unix socket | 0660 | 1007:1007 |

**Server limitation:** creating `caf\xe9.txt` and `pipe-\xff.fifo` (non-UTF-8
names) failed with `EINVAL`. This export does not accept non-UTF-8 names over
NFSv3. The synthetic tests (`crates/mongoose/tests/special_files.rs`) remain
the only coverage for non-UTF-8 special paths. Device nodes were not part of
the approved scope for this export.

## 2. Walker output

These are the rows from the walker scan inside the release candidate (kept
with the first cutover pass). The FIFO and socket rows carry the exact type
strings, the source permissions and owners, and the inodes the kernel client
reports. Each `path_bytes` value is the raw path.

| path | file_type | perms | owner | inode | fsid | path_bytes |
|---|---|---|---|---|---|---|
| `/control.txt` | file | 0644 | 1001:1001 | 6282454732121964171 | 1 | `2F636F6E74726F6C2E747874` |
| `/data-link.bin` | file | 0640 | 1002:1002 | 18004097881031266770 | 1 | `2F646174612D6C696E6B2E62696E` |
| `/data.bin` | file | 0640 | 1002:1002 | 18004097881031266770 | 1 | `2F646174612E62696E` |
| `/nested/inner.fifo` | fifo | 0644 | 1006:1006 | 235850342290634729 | 1 | `2F6E65737465642F696E6E65722E6669666F` |
| `/pipe.fifo` | fifo | 0640 | 1003:1004 | 14534155131617954647 | 1 | `2F706970652E6669666F` |
| `/sock.sock` | socket | 0660 | 1007:1007 | 11541651252830817468 | 1 | `2F736F636B2E736F636B` |

## 3. Canonical index

Each tag agrees with the `S_IFMT` bits of its mode:

| path | tag | mode | S_IFMT |
|---|---|---|---|
| `/control.txt` | 1 Regular | 100644 | 010 |
| `/link-to-control` | 3 Symlink | 120777 | 012 |
| `/nested` | 2 Dir | 040755 | 004 |
| `/nested/inner.fifo` | 4 Fifo | 010644 | 001 |
| `/pipe.fifo` | 4 Fifo | 010640 | 001 |
| `/sock.sock` | 5 Socket | 140660 | 014 |

## 4. Copy

```
sudo mongoose copy --src nfs://main.selab-var204.selab.vastdata.com/source/special-qual \
  --dst nfs://main.selab-var204.selab.vastdata.com/dest/special-qual \
  --work-dir /var/lib/mongoose/special-qual-2
```

```
index ready: 32 shards, 9 entries
complete: 32/32 shards, 6 files ok (0 torn), 0 failed, 3 special NOT copied, 131116 bytes moved
WARNING: 3 fifo, socket, or device entries were NOT copied (mongoose does not create special nodes).
```

The 32-shard index of a 9-entry tree is mostly empty shards. Before PR #28
this copy failed at "verify the index" with `manifest shard
canonical/part-r00-00000.parquet has invalid row count 0`.

**Counters** (`progress.json`):

| Counter | Value |
|---|---|
| `files_ok` | 6 |
| `files_failed` | 0 |
| `files_special_not_copied` | 3 |
| `bytes_moved` | 131116 |

**Destination:**
- **Specials:** absent.
- **Copied:** `control.txt` and `data.bin` are byte-identical to the source.
  Both symlinks kept their targets, and `nested/` exists.
- **Data path:** no special node was sent down the data path. Each one has
  an omission record and nothing at its path on the destination, where a
  data copy would have created a regular file. `bytes_moved` is exactly the
  three regular files (44 + 2 × 65,536). The synthetic test in
  `crates/mongoose/tests/special_files.rs` covers the same rule at the mover
  level.

**Omission records** (`downgrades/`, one per node). They decode to the exact
path bytes:

| `path_b64` | Decoded | Record |
|---|---|---|
| `L25lc3RlZC9pbm5lci5maWZv` | `/nested/inner.fifo` | `SPECIAL_NOT_COPIED{node: fifo}` |
| `L3BpcGUuZmlmbw==` | `/pipe.fifo` | `SPECIAL_NOT_COPIED{node: fifo}` |
| `L3NvY2suc29jaw==` | `/sock.sock` | `SPECIAL_NOT_COPIED{node: socket}` |

## 5. Cutover with the specials absent

```
sudo mongoose sync --work-dir /var/lib/mongoose/special-qual-2 --cutover
```

```
namespace: 9 source entries, 6 destination entries, 3 metadata mismatches
content:   3 files (131116 bytes) and 2 symlinks to read back from both servers
cutover verification FAILED: 3 mismatches (special_not_copied 3)
```

The run exited 1. The three mismatches are exactly the three special nodes.
Every copied entry matched, including the byte-for-byte read-back of the files
and symlinks.

## 6. Cutover after recreating specials on the destination

Two nodes were recreated with the same type, mode and owner as the source:
`pipe.fifo` as a FIFO and `sock.sock` as a bound socket. As a negative check,
a regular file with the same mode and owner went where `nested/inner.fifo`
should be. The script was [`tools/recreate-on-dest.py`](tools/recreate-on-dest.py):

```
sudo python3 -I tools/recreate-on-dest.py /mnt/dest/special-qual
```

The same cutover command then reported:

```
namespace: 9 source entries, 9 destination entries, 1 metadata mismatches
cutover verification FAILED: 1 mismatches (file_type 1)
{"path_lossy":"/nested/inner.fifo","kind":"file_type","expected":"fifo","actual":"file"}
```

The run exited 1.
- **Recreated nodes:** `pipe.fifo` and `sock.sock` are no longer reported.
- **Wrong type:** `nested/inner.fifo` is reported as `file_type`, not as
  `special_not_copied`.

## Logs

The logs are kept on the client, in the home directory of the account that
ran the test:

| File | SHA-256 |
|---|---|
| `special-qual-2-copy.log` | `551aff7fc868e188989c54629d21f80e4e58109215a5b5023df350a2c6a9aa1d` |
| `special-qual-2-cutover-1.log` | `6aaa6f90f214a5741158f5173a2b2f4dbde68fb083db67bdf24b9378811ecf4b` |
| `special-qual-2-cutover-2.log` | `1c2c3a497cf3db42ab22eb9267a8fa0aa81a56e8d715e225ba7ce2b25afef0c2` |

## Observations outside this procedure

- **Hardlinks:** `data.bin` and `data-link.bin` arrived as two separate files
  (different inodes, nlink 1). That is the documented limit in
  `docs/REFERENCE.md`: "links that span shards copy as separate files". The
  walker shards by path hash across 32 shards, so two names of one inode
  share a shard only about 1 time in 32. In practice most hardlinks are
  split.
- **Address discovery:** the first cutover connected over one address
  (`172.200.204.3 x32`), while the copy before it used all six. A DNS stall
  longer than discovery's 3 s quiet window, on the first answer, makes the
  name look like a single-address name. This affects throughput, not
  correctness.
