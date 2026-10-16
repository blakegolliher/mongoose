# Bounded memory

**Goal:** mongoose's peak memory should depend on its settings (`--parallel`,
fixed budgets), never on the size of the tree. Every structure that grows
with the number of entries is to be streamed, spilled to disk, or capped by a
budget, with correct behavior when something is evicted.

**Status (2026-10-16):** the walker scan is bounded (nfs-walker `f6f7572`,
mongoose PR #26). Step 1 is closed with no change, step 2 merged (PR #31),
and step 3 is in this change. Steps 4 to 6 are open.

## What grows with tree size

These figures come from the code on `main` `1765416`. The per-row costs are
estimates from the types, not measurements.

| # | Phase | Mechanism | Growth |
|---|---|---|---|
| 1 | Copy | The mover's directory caches (`dirs_known`, and the source and destination directory file-handle caches) cover every directory and are never evicted | about 650 B per directory, for the whole run |
| 2 | Every scan, including the cutover's destination scan | The rewrite held a whole walker part before writing it, about 720 B per row | entries ÷ 32, up to a 512 MiB part (5–8M rows) |
| 3 | Cutover | The namespace join loads each of 256 fixed buckets into a hash map, with each path stored twice | entries ÷ 256 × about 380 B |
| 4 | Sync | Classify uses the same 256 fixed buckets | entries ÷ 256 × about 250 B |
| 5 | Cutover | Read-back results pile up behind a slow file | unbounded while it stalls |
| 6 | Copy | Every directory row of a shard is held until the shard ends, for its re-stamp | about 800 B per directory in a shard |
| 7 | Sync | Loading the pending set parses every downgrade and failure record | per record, which can mean every file |

Fixed but large: each copy micro-batch spawns all of its rows at once, at
100,000 rows or about 100–130 MB.

## Plan

1. **Allocator. Closed, no change.** Measured on var204 with three workloads:
   a no-op sync of the 2M-entry tree, a copy of `/source/web` (115K files,
   58.6 GB), and that copy's cutover.

   | Setting | Sync peak | Copy peak | Copy wall | Copy system CPU |
   |---|---|---|---|---|
   | glibc default | 628–632 MB | 233–241 MB | 1:12–1:14 | 146–150 s |
   | `M_MMAP_THRESHOLD` 128 KiB | 487 MB | 178 MB | 1:30 | 232 s |
   | `M_MMAP_THRESHOLD` 2 MiB / 8 MiB | 622 / 644 MB | 254 / 249 MB | 1:18 / 1:17 | 205 / 199 s |
   | `malloc_trim(0)` every second | 588 MB | 233 MB | 1:12 | 147 s |
   | mimalloc as global allocator | 871 MB | 390 MB | 1:16 | 147 s |

   - **128 KiB threshold:** the only real memory saving, and it costs the
     copy a quarter of its throughput. The mover allocates a 1 MiB buffer
     per file, and each one becomes a fresh mmap.
   - **Higher thresholds:** they save nothing and also turn off glibc's own
     tuning.
   - **Trimming:** about 7% and nothing more.
   - **mimalloc:** uses more memory.
   - **What glibc keeps:** about 150 MB of freed buffers, which is a
     constant, not growth.
2. **Streaming rewrite.** Each translated batch is written as soon as it is
   made, and a canonical row group closes at 32 MiB of encoded data. On one
   real 2,004,879-row walker part:

   | | Peak RSS | Wall time | Output |
   |---|---|---|---|
   | Before | 1,520 MB | 5.9 s | 116.1 MiB in 2 row groups |
   | After | 123 MB | 5.7 s | 122.8 MiB in 5 row groups |

   The rows (whole-row hash), the row ids and the footer metadata are
   identical.
3. **Joins.** Both joins keep their 256 first-level buckets, so they never
   hold more than 256 files open. Before a bucket is joined, a side over
   32 MiB on disk (the side loaded into memory) is split again by a hash
   seeded with the split depth, and its parts are joined in turn, down to
   four levels. Each join's map therefore stays near 60 MB at any index
   size. Below about 55M entries no bucket splits. The namespace map now
   stores each path once, as its key.

   On a 48M-entry index (24 copies of the fs-sim tree) joined against
   itself, with 8 buckets so that each held 6M entries, the bucket size of
   a ~1.5B-entry tree:

   | Join | Peak RSS before | Peak RSS after | Time before | Time after |
   |---|---|---|---|---|
   | Classify, 8 buckets | 1,477 MB | 55 MB | 184 s | 191 s |
   | Namespace, 8 buckets | 2,297 MB | 124 MB | 291 s | 321 s |
   | Classify, 256 buckets (no split) | 57 MB | 63 MB | 156 s | 155 s |

   Both versions gave identical counts. A split costs one extra write and
   read of the bucket files.
4. **Directory caches.** Cap them by bytes. A miss repeats a LOOKUP or an
   mkdir, which is correct and only slower. This needs a directory-heavy
   test tree on real NFS to measure the hit rate (fs-sim-1 has 4,764
   directories).
5. **Smaller items:**
   - a bounded reorder window for the content read-back;
   - re-reading directory rows from the shard file at re-stamp time instead
     of holding them;
   - filtering records while reading them in the pending-set load;
   - writing the failure and downgrade logs incrementally;
   - spawning micro-batch rows lazily.
6. **Regression guard.** A test pushes a synthetic index of several million
   rows through rewrite, classify and namespace verification, and fails if
   peak memory crosses a fixed ceiling. It needs no NFS.

The target after steps 1–6 is about 1–1.5 GB peak for any tree size. That is
an estimate, to be confirmed by the step 6 measurements.
