# Bounded memory

**Goal:** mongoose's peak memory should depend on its settings (`--parallel`,
fixed budgets), never on the size of the tree. Every structure that grows
with the number of entries is to be streamed, spilled to disk, or capped by a
budget, with correct behavior when something is evicted.

**Status (2026-10-17):** the walker scan is bounded (nfs-walker `f6f7572`,
mongoose PR #26). Step 1 is closed with no change, steps 2 to 4 and 6
merged (PRs #31 to #34), and step 5 is in this change. Steps 7 and 8 are
open.

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
4. **Directory caches.** `dirs_known` and the source and destination
   directory filehandle caches are now two-generation maps
   (`migration-mover/src/generational.rs`): inserts go to the current
   generation, a hit in the previous one moves forward, and a full current
   generation replaces the previous one. Each cache keeps at most two
   generations of 524,288 directories. A miss repeats a LOOKUP or an mkdir,
   which is correct and only slower.

   Each run copied an fs-sim tree of 1,000,000 directories and 1,000,000
   1 KiB files on var204 into a fresh destination:

   | Directories per generation | Copy phase | Peak RSS |
   |---|---|---|
   | unbounded (before) | 1,250 s | 1,118 MB |
   | 4,096 | 1,933 s | 876 MB |
   | 131,072 | 1,361 s | 959 MB |
   | 524,288 (chosen) | 1,299 s | 1,101 MB |

   - **Per-directory cost:** the three caches cost about 270 B per
     directory, so unbounded they grow about 13 GB per 50M directories.
     At the chosen size they stop at about 280 MB.
   - **Why the chosen size behaves like unbounded here:** this tree's 1M
     directories fit in its two generations.
   - **Cost of misses:** each directory is touched twice in different
     shards, once as its file's parent and once for its own row, so a
     cache too small to hold the tree pays a LOOKUP per side.

   The directory-heavy tree also showed two more consumers that grow with
   directories (steps 6 and 7).
5. **Smaller items.** Each is now bounded by the concurrency or a fixed
   budget:
   - **Content read-back (row 5):** each result is recorded when it
     completes. The checkpoint lists the entries still running, at most the
     concurrency, and every other entry below the last one started is done.
     A slow file no longer holds a result for every entry checked while it
     is read. `mismatches.jsonl` is now in completion order.
   - **Directory re-stamp (row 6):** the shard-end re-stamp reads the
     directory rows back from the shard file instead of holding a copy of
     each.
   - **Pending set (row 7):** records are parsed one line at a time, and
     only failures and torn copies are kept.
   - **Failure and downgrade logs:** `mongoose copy` spills each shard's
     records to a hidden `.partial` file as they arrive, then renames it
     into place before the shard is marked complete.
   - **Task spawning:** tasks start per size class as permits free, with up
     to one waiting task per permit, instead of one per row up front. Each
     class has its own queue, so large files waiting for permits do not
     hold back small ones.

   One 2,004,879-row shard went through the shard processor and the copy
   loop's result persistence, with no NFS. The mover took 1 ms per row and
   recorded a downgrade for every file:

   | Shard | Peak before | Peak after | Time before | Time after |
   |---|---|---|---|---|
   | All files | 1,171–1,175 MB | 51–53 MB | 25.6–26.8 s | 27.0–27.3 s |
   | Half of its rows directories | 1,319 MB | 136–148 MB | 34.7 s | 34.6–36.4 s |

   - **Records:** the same apart from their timestamps.
   - **Time:** about 5% more on the all-files shard, because the 527 MB of
     records are written while the shard runs. This mover is about 45 times
     faster than a real copy.
   - **Lazy spawning:** inside a batch it saves only about 7 MB per 100,000
     rows, far less than the 100–130 MB estimated above. It matters for the
     re-stamp, which spawned a task for every directory of the shard: with
     every task spawned up front, the directory-heavy shard still peaked at
     1,105 MB.

   On var204, portable builds of `main` and of this change, one after the
   other:

   | Workload | Before | After |
   |---|---|---|
   | No-op `sync` of the 2M-entry tree | 648 MB, 31.2 s | 645 MB, 30.2 s |
   | Copy of `/source/web` (115,370 files, 58.6 GB) | 213 MB, 1:15.6 (copy phase 58 s) | 217 MB, 1:21.5 (copy phase 59 s) |
   | That copy's cutover | 321 MB, 1:47.8 | 289 MB, 1:48.1 |

   These trees have few directories and no downgrades, so little changes
   here. The copy's extra 6 s is in the scan and address discovery; the copy
   phase ran at 950 and 952 MB/s. Both cutovers read back 115,086 files and
   found no mismatch.
6. **The walker's pending-directory queue.** nfs-walker `0e9b795` makes each
   worker's own queue LIFO. The scan used to go breadth first, and most of
   the tree's widest level waited in the queues at once. Now it goes depth
   first, and peers still steal the oldest, shallowest directory a worker
   has queued. The directories waiting at once stay near the tree's
   depth × fan-out: a walker test on a full tree of 6⁴ leaf directories
   holds at most 21 pending, against 1,296 before.

   Standalone walker on var204 with 32 workers, alternating runs:

   | Tree | Before | After |
   |---|---|---|
   | 1M directories, 1M files | 727–749 MB, 33–34 s | 625–675 MB, 28–34 s |
   | 2M files in 4,764 directories | 718–726 MB, 8.1 s | 710–744 MB, 8.1–8.2 s |

   In mongoose, a no-op `sync` of the 2M-file tree with this walker peaked
   at 640 MB, against 636 MB with the previous pin. It classified all
   2,004,879 entries unchanged against a baseline the breadth-first walker
   wrote.

   One directory with millions of subdirectories still queues all of them
   when it is read.
7. **The READDIRPLUS child cache's evicted set** (`DirChildrenState`): it
   is capped at 1,000,000 paths, about 45 MB on this tree and up to about
   150 MB.
8. **Regression guard.** A test pushes a synthetic index of several million
   rows through rewrite, classify and namespace verification, and fails if
   peak memory crosses a fixed ceiling. It needs no NFS.

The target after steps 1–8 is about 1–1.5 GB peak for any tree size. That is
an estimate, to be confirmed by the step 8 measurements.
