//! mongoose — copy one NFS path to another, then keep it in sync
//! until cutover.
//!
//! A deliberately small front-end over the vamoose engine, shipped
//! as one binary: the nfs-walker scan and `mig-walker-rewrite`
//! canonical conversion are compiled in as libraries (the scanner
//! pinned to the commit `packaging/nfs-walker.lock.json` records),
//! and the copy path is the same libnfs mover and `ShardProcessor`
//! dispatch the vamoose worker uses — but everything on local disk.
//! No S3, no claims, no coordinator, no worker fleet, no TUI, no
//! external tools, and one load knob instead of the engine's dozen.
//!
//! ```text
//! mongoose copy   scan + index (prepare) -> copy every shard (copy)
//! mongoose sync   rescan -> classify against the last pass -> copy the delta
//! mongoose sync --cutover
//!                 rescan -> classify (must be clean) -> scan the dest
//!                 -> verify it against the source, bytes included (verify)
//! ```
//!
//! ## Work-dir layout
//!
//! ```text
//! <work-dir>/
//!   run.json                     job identity: source, dest, excludes
//!   scan.json                    scan checkpoint (raw output is purged
//!                                once the index is built)
//!   canonical/part-NNNN.parquet  canonical shards (mig-walker-rewrite)
//!   rewrite.json                 mig-walker-rewrite's own checkpoint
//!   manifest.json                local run plan
//!   progress.json                copy progress + completed-shard list
//!   failures/part-NNNN.jsonl     per-file failures, per shard
//!   downgrades/part-NNNN.jsonl   per-file metadata downgrades, per shard
//!   baseline.json                which pass the next sync diffs against
//!   passes/pass-NNNN/            one sync pass (same layout, plus
//!                                classify/ and delta-manifest.json;
//!                                a cutover pass adds dest/, verify/,
//!                                and verify.json)
//! ```
//!
//! See `docs/REFERENCE.md` for resume semantics, the correctness
//! posture, and the deliberate limitations.

pub mod cli;
pub mod copy;
pub mod delta;
pub mod endpoint;
pub mod exclude;
pub mod identity;
pub mod manifest;
pub mod prepare;
pub mod progress;
pub mod scan;
pub mod sync;
pub mod util;
pub mod verify;
pub mod workdir;
pub mod workdir_lock;
