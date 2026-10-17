//! Per-shard processing.
//!
//! Once a shard has been claimed and downloaded, this module walks its
//! rows in `row_id` order, builds byte-budgeted micro-batches, and
//! drives the mover. M3 dispatches rows concurrently.
//!
//! ## Concurrency model (M3)
//!
//! - Within a batch, rows are partitioned into **hardlink groups** and
//!   **singletons**:
//!   - Singletons: each gets its own task; up to `inflight_*` (per
//!     size class) tasks run in parallel. A task is spawned when its
//!     class has a free permit, not before, so a batch never holds a
//!     task per row.
//!   - Hardlink groups (same `(fsid, inode)` and `nlink > 1`): each
//!     group becomes one task, processing its rows **sequentially**
//!     so the first row is fully renamed before subsequent rows
//!     `nfs_link` against it.
//! - Each task acquires a size-class permit from `InflightLimiter`
//!   before each mover call, then a libnfs context pair from the
//!   pool, then runs the libnfs work on the blocking pool. Concurrent
//!   tasks therefore use distinct libnfs contexts.
//! - Outcomes are collected on the dispatcher thread; `record(...)`
//!   updates counters and counts failures. No shared map needed.
//!
//! ## Hardlink fidelity (R5 + SCHEMA_CONTRACT.md)
//!
//! The per-group sequential dispatch keeps hardlink fidelity without
//! a shared mutable map: the group leader copies; subsequent rows
//! `nfs_link` against the leader's path. The processor still emits
//! the one-shot `FsidUngrouped` downgrade per shard for hardlinked rows
//! whose `inode` is set but `fsid` is not. Such rows are copied
//! independently because inode-only grouping is unsafe across filesystems.
//!
//! Cross-shard hardlinks remain out of scope for v1.
//!
//! ## Special nodes (fifo, socket, device)
//!
//! The mover recognizes these and does not create them. Such a row is a
//! **processed omission**: it advances the shard, is not retried, moves
//! no bytes, and is counted in `files_special_not_copied`, never in
//! `files_ok` or `files_failed`. The mover writes one
//! `SPECIAL_NOT_COPIED` downgrade record per row. Special rows never
//! join a hardlink group: nothing is created for later rows to link
//! against, so each row is its own omission.
//!
//! ## Operator pause (coord `Pause`)
//!
//! Checked between batches only: a paused worker finishes the batch in
//! flight, then blocks until the mode changes. The claim stays ours
//! throughout — the heartbeat task keeps writing the per-host progress
//! record, which is the liveness signal reclaimers trust (see
//! `check_progress_liveness`), so a long pause never turns into a
//! peer stealing the shard. Drain/Cancel are NOT honored mid-shard:
//! the shard runs to completion and the orchestrator's claim loop
//! stops claiming, so no partial shard is ever left for replay.
//!
//! ## Process stop (SIGTERM / SIGINT)
//!
//! Also checked between batches only. A stop request finishes the
//! batch in flight and returns with `ProcessOutcome::interrupted`
//! set; the orchestrator then releases the claim so a peer can pick
//! the shard up at once. Rows already committed are durable and are
//! recognized on replay, so the batch boundary is the cheapest safe
//! point to stop — no row is ever interrupted mid-copy.
//!
//! ## Fence checks (R3)
//!
//! - Between batches (in `process`).
//! - Between rows within a sequential group, and at task entry for
//!   singletons. **Never inside a copy.** Once a row enters the mover,
//!   it runs to commit (rename) or per-file failure.

use crate::throughput::ThroughputCounter;
use migration_core::fence::Fence;
use migration_core::records::{DowngradeKind, FailurePhase};
use migration_core::schema::FileTypeTag;
use migration_core::shard::{RowView, ShardReader};
use migration_mover::batch::{Batch, BatchBudget, InflightLimiter, SizeClass};
use migration_mover::strategy::Strategy;
use migration_mover::{FailureSink, FileMover, MoveOutcome};
use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::Arc;
use tokio::task::{AbortHandle, JoinSet};
use tokio_util::sync::CancellationToken;

/// Filesystem-scoped hardlink-map key.
type HardlinkKey = (u64, u64);

/// How many shard-end re-stamp rows are read ahead of the ones running.
const RESTAMP_LOOKAHEAD: usize = 1024;

/// One task's work: a lone row, or a hardlink group whose rows run in
/// order on one task.
enum Job {
    Row(RowView),
    Group(Vec<RowView>),
}

impl Job {
    /// The size class whose permits the job waits for. A group's rows
    /// share an inode, so its first row speaks for all of them.
    fn class(&self) -> SizeClass {
        let size = match self {
            Job::Row(row) => row.size,
            Job::Group(rows) => rows.first().map_or(0, |row| row.size),
        };
        SizeClass::classify(size)
    }
}

/// Which rows a dispatch runs, which decides how their outcomes count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Files, symlinks, special nodes and hardlink groups.
    Rows,
    /// A batch's directories, one depth level at a time.
    Dirs,
    /// The shard-end replay of every directory's attributes. Their
    /// successes were counted when their batch ran, so only failures
    /// are recorded.
    Restamp,
}

const CLASSES: [SizeClass; 3] = [SizeClass::Small, SizeClass::Medium, SizeClass::Large];

pub struct ShardProcessor {
    pub mover: Arc<dyn FileMover>,
    /// Live per-row counters published by the heartbeat between batch
    /// commits (see `heartbeat::LivePending`). Reset by the
    /// orchestrator when it merges shard-end totals into
    /// `ProgressState`.
    pub live: Arc<crate::heartbeat::LivePending>,
    pub fence: Fence,
    pub budget: BatchBudget,
    pub inflight: InflightLimiter,
    pub failures: FailureSink,
    pub throughput: ThroughputCounter,
    /// Set true the first time we see a hardlinked, inode-bearing row with no
    /// `fsid`; gates the one-shot WARN + `FsidUngrouped` downgrade.
    pub fsid_ungrouped_warned: bool,
    /// Coord event emitter. Disabled in legacy S3-only mode (every
    /// call is a no-op); when enabled, `record_outcome` pushes a
    /// `WorkerEventDraft` per file outcome so the coord_driver can
    /// coalesce a `ProgressDelta` for `/workers/{id}/events`.
    pub emitter: crate::events::EventEmitter,
    /// Coord-driven run control, consulted between batches for
    /// `Pause`. `None` in S3-only mode (no `[coord]`).
    pub run_control: Option<crate::run_control::RunControlReader>,
    /// Process stop request (SIGTERM / SIGINT), consulted between
    /// batches. Cancelled → finish the batch in hand and return with
    /// `interrupted` set. See the module docs.
    pub stop: CancellationToken,
}

impl ShardProcessor {
    pub async fn process(&mut self, parquet_path: &Path) -> anyhow::Result<ProcessOutcome> {
        let reader = ShardReader::open(parquet_path)?;
        let total = reader.rows();
        tracing::info!(parquet = %parquet_path.display(), total_rows = total, "shard opened");

        // Per-shard state reset.
        self.fsid_ungrouped_warned = false;
        let mut dirs: u64 = 0;

        let mut current = Batch::default();
        let mut outcome = ProcessOutcome {
            rows_total: total,
            ..Default::default()
        };

        // A stop that arrived before the first batch: nothing has been
        // copied from this shard, hand it straight back.
        if self.stop.is_cancelled() {
            return Ok(outcome.with_interrupted(true));
        }

        for row_result in reader.into_rows()? {
            if !self.fence.is_valid() {
                return Ok(outcome.with_fenced(true));
            }
            let row = row_result?;

            if current.would_overflow(&row, &self.budget) {
                let to_run = current.take();
                self.run_batch(to_run, &mut outcome).await?;
                if !self.fence.is_valid() {
                    return Ok(outcome.with_fenced(true));
                }
                self.hold_while_paused().await;
                // The fence may have tripped during a long hold.
                if !self.fence.is_valid() {
                    return Ok(outcome.with_fenced(true));
                }
                // Batch boundary: the only point a process stop is
                // honored. Everything before it is committed.
                if self.stop.is_cancelled() {
                    tracing::info!(
                        rows_done = outcome.rows_processed(),
                        rows_total = total,
                        "stop requested; leaving shard at batch boundary",
                    );
                    return Ok(outcome.with_interrupted(true));
                }
            }
            if row.file_type == FileTypeTag::Dir {
                dirs += 1;
            }
            current.push(row);
        }

        if !current.is_empty() {
            self.run_batch(current, &mut outcome).await?;
        }

        if !self.fence.is_valid() {
            return Ok(outcome.with_fenced(true));
        }

        // ---- Shard-end dir attr re-stamp -----------------------------
        // Every file CREATE bumps its parent dir's mtime, so per-batch
        // dir stamping is undone by later batches. Replays DirAttrs for
        // every dir row now that no more file CREATEs in this shard can
        // bump parent mtimes (cross-shard children remain the
        // documented caveat). Idempotent; no ordering requirement
        // (SETATTR on a child dir does not touch its parent). Failures
        // count normally. The dir rows are read back from the shard
        // file instead of being held since their batch ran.
        if dirs > 0 {
            tracing::info!(dirs, "shard-end directory attr re-stamp");
            let rows = ShardReader::open(parquet_path)?
                .into_rows()?
                .filter(|row| !matches!(row, Ok(r) if r.file_type != FileTypeTag::Dir))
                .map(|row| row.map(Job::Row).map_err(anyhow::Error::from));
            self.dispatch(rows, RESTAMP_LOOKAHEAD, Phase::Restamp, &mut outcome)
                .await?;
            if !self.fence.is_valid() {
                return Ok(outcome.with_fenced(true));
            }
        }
        Ok(outcome)
    }

    /// Block between batches while the coord has the job paused. The
    /// batch boundary is the only safe point: no row is mid-copy, and
    /// the heartbeat keeps the claim alive for as long as the hold
    /// lasts.
    async fn hold_while_paused(&mut self) {
        let Some(rc) = self.run_control.as_mut() else {
            return;
        };
        if !rc.is_paused() {
            return;
        }
        tracing::info!("coord requested pause; holding at batch boundary");
        // A process stop ends the hold early; the caller's stop check
        // right after this returns then hands the shard back.
        tokio::select! {
            after = rc.wait_while_paused() => {
                tracing::info!(?after, "pause released; resuming shard");
            }
            _ = self.stop.cancelled() => {
                tracing::info!("stop requested while paused; leaving hold");
            }
        }
    }

    async fn run_batch(
        &mut self,
        batch: Batch,
        outcome: &mut ProcessOutcome,
    ) -> anyhow::Result<()> {
        if !self.fence.is_valid() {
            return Ok(());
        }

        // ---- Partition: hardlink groups, non-dir singletons, dirs ----
        //
        // Dirs are split out and run *after* everything else so file
        // commits inside them don't restamp their mtime. POSIX disallows
        // hardlinks on dirs, so dir rows can't appear in hardlink groups.
        let mut groups: HashMap<HardlinkKey, Vec<RowView>> = HashMap::new();
        let mut singletons: Vec<RowView> = Vec::new();
        let mut dirs: Vec<RowView> = Vec::new();
        for row in batch.rows {
            if row.file_type == FileTypeTag::Dir {
                dirs.push(row);
                continue;
            }
            if row.file_type.is_special() {
                // Never part of a hardlink group: the mover creates
                // nothing for a special node, so a later row of the
                // group would have no destination entry to link to and
                // would fail. Each special row is its own omission.
                singletons.push(row);
                continue;
            }
            if row.nlink.unwrap_or(1) > 1 {
                match hardlink_key(&row) {
                    Some(key) => groups.entry(key).or_default().push(row),
                    None => {
                        if row.inode.is_some() && row.fsid.is_none() {
                            self.maybe_emit_fsid_ungrouped_warning(&row);
                        }
                        singletons.push(row);
                    }
                }
            } else {
                singletons.push(row);
            }
        }

        // Interleave singletons round-robin across parent directories.
        // Row order is directory-clustered (walker DFS), so the
        // in-flight window otherwise targets only a handful of parent
        // dirs at a time — and the NFS server serializes creates
        // within one parent (measured ~185 creates/s/dir on VAST).
        // Spreading the window across parents turns the per-dir
        // ceiling into a non-factor. Deterministic: first-seen parent
        // order, original row order within each parent.
        let singletons = interleave_by_parent(singletons);

        // ---- Phase 1: dispatch non-dir work concurrently -------------
        let jobs = groups
            .into_values()
            .map(Job::Group)
            .chain(singletons.into_iter().map(Job::Row))
            .map(Ok);
        self.dispatch(jobs, usize::MAX, Phase::Rows, outcome)
            .await?;

        // ---- Phase 2: dirs, deepest-first, level-parallel ------------
        //
        // Deepest-first is the invariant that keeps dir mtimes honest: a
        // parent's setattr must come after every deeper mkdir that could
        // restamp it. Fully sequential processing preserved that but
        // serializes ~5 ms metadata RPCs per dir, which dominates
        // dir-dense batches. Instead: group dirs into depth levels and
        // run each level concurrently with a barrier between levels.
        // Within one level no dir is an ancestor of another, and all
        // deeper levels (the only possible restampers) are fully
        // committed before a shallower level's setattr runs — the same
        // guarantee the sequential loop gave.
        if !dirs.is_empty() {
            sort_deepest_first(&mut dirs);
            let mut levels: Vec<Vec<RowView>> = Vec::new();
            let mut level_depth: Option<usize> = None;
            for row in dirs {
                let d = row.path.iter().filter(|&&b| b == b'/').count();
                if level_depth != Some(d) {
                    levels.push(Vec::new());
                    level_depth = Some(d);
                }
                levels.last_mut().expect("just pushed").push(row);
            }

            for level in levels {
                if !self.fence.is_valid() {
                    break;
                }
                let jobs = level.into_iter().map(Job::Row).map(Ok);
                self.dispatch(jobs, usize::MAX, Phase::Dirs, outcome)
                    .await?;
            }
        }

        Ok(())
    }

    /// Run `jobs`, each on its own task, with at most twice as many
    /// tasks of a size class alive at once as the limiter has permits
    /// for it: one set holding permits, one waiting to take each permit
    /// as it is released. Spawning a task per row up front held one
    /// for every row of a batch while a few hundred of them ran. Each
    /// class has its own queue, so large files waiting for their few
    /// permits never hold back the small files behind them. At most
    /// `lookahead` jobs wait in the queues. A tripped fence or a
    /// failing source starts nothing more; what is running finishes.
    async fn dispatch(
        &mut self,
        jobs: impl Iterator<Item = anyhow::Result<Job>>,
        lookahead: usize,
        phase: Phase,
        outcome: &mut ProcessOutcome,
    ) -> anyhow::Result<()> {
        let mut jobs = Some(jobs);
        let mut failed = None;
        let mut queued: [VecDeque<Job>; 3] = Default::default();
        let mut running = [0usize; 3];
        let mut class_of: HashMap<tokio::task::Id, usize> = HashMap::new();
        let mut joins: JoinSet<Vec<(RowView, MoveOutcome)>> = JoinSet::new();
        loop {
            while let Some(source) = jobs.as_mut() {
                if queued.iter().map(VecDeque::len).sum::<usize>() >= lookahead {
                    break;
                }
                match source.next() {
                    Some(Ok(job)) => queued[job.class() as usize].push_back(job),
                    Some(Err(e)) => {
                        // Let what is running finish, then report.
                        failed = Some(e);
                        jobs = None;
                    }
                    // Dropping the source frees whatever it still holds.
                    None => jobs = None,
                }
            }
            if !self.fence.is_valid() || failed.is_some() {
                jobs = None;
                queued.iter_mut().for_each(VecDeque::clear);
            }
            for (c, class) in CLASSES.into_iter().enumerate() {
                while running[c] < 2 * self.inflight.permits(class) {
                    let Some(job) = queued[c].pop_front() else {
                        break;
                    };
                    let task = self.spawn_job(&mut joins, job, phase);
                    class_of.insert(task.id(), c);
                    running[c] += 1;
                }
            }
            let Some(joined) = joins.join_next_with_id().await else {
                break;
            };
            let id = match &joined {
                Ok((id, _)) => *id,
                Err(e) => e.id(),
            };
            if let Some(c) = class_of.remove(&id) {
                running[c] -= 1;
            }
            match joined {
                Ok((_, results)) => {
                    for (row, mo) in results {
                        if phase != Phase::Restamp || mo.result.is_err() {
                            self.record(&row, mo, outcome);
                        }
                    }
                }
                Err(e) => {
                    // A spawned task panicked or was cancelled. Don't
                    // panic the worker — log and keep collecting the
                    // rest. Treat as a failure so the operator sees it.
                    outcome.files_failed += 1;
                    tracing::error!(error = ?e, ?phase, "dispatched task join failed");
                }
            }
        }
        failed.map_or(Ok(()), Err)
    }

    fn spawn_job(
        &self,
        joins: &mut JoinSet<Vec<(RowView, MoveOutcome)>>,
        job: Job,
        phase: Phase,
    ) -> AbortHandle {
        let mover = Arc::clone(&self.mover);
        let inflight = self.inflight.clone();
        let fence = self.fence.clone();
        let row = match job {
            Job::Group(rows) => return joins.spawn(run_group(mover, inflight, fence, rows)),
            Job::Row(row) => row,
        };
        let live = (phase == Phase::Rows).then(|| Arc::clone(&self.live));
        joins.spawn(async move {
            if !fence.is_valid() {
                return Vec::new();
            }
            let _permit = inflight.acquire(row.size).await;
            // Fence may have tripped while waiting for the inflight
            // permit. Without rechecking here, all queued tasks would
            // run to completion even after fence trip, since the
            // pre-acquire check fired before they reached the
            // semaphore.
            if !fence.is_valid() {
                return Vec::new();
            }
            if let Some(live) = live {
                live.rows_started
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            let mo = mover.move_one(&row).await;
            vec![(row, mo)]
        })
    }

    fn maybe_emit_fsid_ungrouped_warning(&mut self, row: &RowView) {
        if self.fsid_ungrouped_warned {
            return;
        }
        self.fsid_ungrouped_warned = true;
        tracing::warn!(
            row_id = row.row_id,
            "hardlinked row has inode without fsid; copying affected entries \
             independently (see SCHEMA_CONTRACT.md \"Null attribute semantics\")",
        );
        self.mover
            .downgrade_sink()
            .record(row.row_id, &row.path, DowngradeKind::FsidUngrouped);
    }

    fn record(&mut self, row: &RowView, mo: MoveOutcome, outcome: &mut ProcessOutcome) {
        // Live counters first — same classification as record_outcome.
        {
            use std::sync::atomic::Ordering::Relaxed;
            self.live.rows_done.fetch_add(1, Relaxed);
            match (&mo.result, mo.strategy) {
                (Ok(()), Strategy::SpecialNotCopied) => {
                    self.live.files_special_not_copied.fetch_add(1, Relaxed);
                }
                (Ok(()), _) => {
                    self.live.files_ok.fetch_add(1, Relaxed);
                    self.live.bytes_moved.fetch_add(mo.bytes_moved, Relaxed);
                }
                (Err(e), _) if matches!(e.phase, migration_core::records::FailurePhase::Fenced) => {
                    self.live.files_fenced.fetch_add(1, Relaxed);
                }
                (Err(_), _) => {
                    self.live.files_failed.fetch_add(1, Relaxed);
                }
            }
        }
        record_outcome(
            &row.path,
            mo,
            outcome,
            &self.failures,
            &self.throughput,
            &self.emitter,
        );
    }
}

/// Classify a single mover outcome into one of four sinks: the
/// special-node omission counter, the success counter, the fenced
/// counter, or the per-file failures sink. Factored out of
/// `ShardProcessor::record` so the special cases can be unit-tested
/// without standing up a Mover + libnfs pool.
fn record_outcome(
    row_path: &[u8],
    mo: MoveOutcome,
    outcome: &mut ProcessOutcome,
    failures: &FailureSink,
    throughput: &ThroughputCounter,
    emitter: &crate::events::EventEmitter,
) {
    match mo.result {
        // A fifo, socket, or device node the mover recognized and did
        // not create. Checked before the general success arm: the
        // mover's result is `Ok` so the shard completes and the row is
        // not retried, but nothing was copied. It is not a copied
        // file, moves no bytes, feeds neither the throughput sample
        // nor the failure ratio behind backpressure, and emits no
        // coord progress event (the coord's delta has files, bytes,
        // and errors, and this is none of them). The mover already
        // wrote the row's `SPECIAL_NOT_COPIED` downgrade record.
        Ok(()) if mo.strategy == Strategy::SpecialNotCopied => {
            outcome.files_special_not_copied += 1;
            tracing::debug!(
                row_id = mo.row_id,
                path = %String::from_utf8_lossy(row_path),
                "special node not copied",
            );
        }
        Ok(()) => {
            outcome.files_ok += 1;
            // F05: a torn copy still commits (at-least-once; source
            // intact) — it is a success with a TornCopy downgrade
            // record already in the sink, not a failure. Count it so
            // the per-shard summary surfaces the tear.
            if mo.torn {
                outcome.files_torn += 1;
            }
            outcome.bytes_moved = outcome.bytes_moved.saturating_add(mo.bytes_moved);
            throughput.add(mo.bytes_moved);
            // Best-effort: push a per-file event draft. Disabled in
            // legacy mode; never blocks the processor.
            emitter.progress_ok(mo.bytes_moved);
        }
        // R8: a Fenced row is not a per-file failure. The mover saw
        // the fence trip immediately before its commit-point op
        // (rename / link / symlink) and bailed out without writing.
        // The shard's claim will terminate; the next reclaimer
        // copies this row. Recording it as a per-file failure would
        // (a) trip M5 assertion E (failures sink must be empty), and
        // (b) mislead operators into chasing a non-bug.
        Err(e) if e.phase == FailurePhase::Fenced => {
            outcome.files_fenced += 1;
            tracing::warn!(
                row_id = mo.row_id,
                strategy = ?mo.strategy,
                error = %e.error,
                "row fenced before commit; will be picked up after reclaim",
            );
            emitter.progress_fenced();
        }
        Err(e) => {
            outcome.files_failed += 1;
            tracing::warn!(
                row_id = mo.row_id,
                strategy = ?mo.strategy,
                phase = ?e.phase,
                error = %e.error,
                "file failed",
            );
            failures.record(mo.row_id, row_path, e.phase, e.error);
            emitter.progress_failed();
        }
    }
}

#[derive(Debug, Default)]
pub struct ProcessOutcome {
    pub rows_total: u64,
    pub files_ok: u64,
    pub files_failed: u64,
    /// R8: rows that bailed out at the mover's pre-commit fence check.
    /// Not a failure (no failures-sink record, no operator alert) — the
    /// row will be copied by whichever worker reclaims the shard next.
    /// Surfaced for observability so a spike is visible.
    pub files_fenced: u64,
    /// F05: rows that committed but were modified on the source while
    /// being copied (`MoveOutcome::torn`). Each also produced a
    /// `DowngradeKind::TornCopy` downgrade record. Counted inside
    /// `files_ok` — a torn copy is a success with a caveat, not a
    /// failure.
    pub files_torn: u64,
    /// Fifos, sockets, and device nodes the mover recognized and did
    /// not create. Processed rows that are neither copied files nor
    /// failures: not in `files_ok`, `files_failed`, `files_fenced`,
    /// or `bytes_moved`. Each has a `SPECIAL_NOT_COPIED` downgrade
    /// record.
    pub files_special_not_copied: u64,
    pub bytes_moved: u64,
    pub fenced: bool,
    /// The shard was left at a batch boundary because the process was
    /// asked to stop (SIGTERM / SIGINT). Counters above cover only the
    /// batches that ran; the orchestrator releases the claim so a
    /// peer finishes the rest.
    pub interrupted: bool,
}

impl ProcessOutcome {
    /// Rows that reached a final classification in this shard: copied,
    /// failed, or a special node left out. Fenced rows are not
    /// processed; the next reclaimer takes them.
    pub fn rows_processed(&self) -> u64 {
        self.files_ok + self.files_failed + self.files_special_not_copied
    }

    pub fn with_fenced(mut self, v: bool) -> Self {
        self.fenced = v;
        self
    }

    pub fn with_interrupted(mut self, v: bool) -> Self {
        self.interrupted = v;
        self
    }
}

// =============================================================================
// Per-group sequential runner. Lives outside the impl so it can be
// `spawn`ed without borrowing `&mut self` across an await.
// =============================================================================

async fn run_group(
    mover: Arc<dyn FileMover>,
    inflight: InflightLimiter,
    fence: Fence,
    rows: Vec<RowView>,
) -> Vec<(RowView, MoveOutcome)> {
    let mut results = Vec::with_capacity(rows.len());
    let mut group_target: Option<Vec<u8>> = None;

    for row in rows {
        if !fence.is_valid() {
            break;
        }
        let _permit = inflight.acquire(row.size).await;
        // Fence may have tripped while waiting for the inflight permit;
        // recheck before launching the mover op so a tripped fence stops
        // the group at the next boundary instead of draining all queued
        // hardlink members.
        if !fence.is_valid() {
            break;
        }
        let outcome = match group_target.as_deref() {
            Some(target) => mover.move_hardlink(&row, target).await,
            None => mover.move_one(&row).await,
        };
        if outcome.result.is_ok() && group_target.is_none() && row.nlink.unwrap_or(1) > 1 {
            // Per R5: record the *final* path post-rename, not the
            // partial. `move_one` succeeded, so the rename happened.
            group_target = Some(row.path.clone());
        }
        results.push((row, outcome));
    }

    results
}

// =============================================================================
// Pure helpers — extracted so they can be unit-tested without a real
// mover or libnfs.
// =============================================================================

/// Build the hardlink-map key from a row. Both filesystem and inode identity
/// are required; without either one, grouping could link unrelated files.
pub(crate) fn hardlink_key(row: &RowView) -> Option<HardlinkKey> {
    row.fsid.zip(row.inode)
}

/// Sort dir rows so deeper paths come first. Required by Phase 2 of
/// `run_batch`: applying attrs to a child dir doesn't bump the
/// parent's mtime, but `mkdir` of an empty child *does*. Doing the
/// deepest dirs first means any mkdir-of-empty-child happens before
/// the parent's setattr, so the parent's mtime is the one we set.
///
/// "Depth" here is just the count of `/` separators. Stable so
/// equal-depth paths keep their input order.
/// Round-robin rows across their parent directories: one row from each
/// parent in first-seen order, repeating until all queues drain. See
/// the call site in `run_batch` for why (per-directory create
/// serialization on the NFS server).
pub(crate) fn interleave_by_parent(rows: Vec<RowView>) -> Vec<RowView> {
    let total = rows.len();
    let mut queues: Vec<std::collections::VecDeque<RowView>> = Vec::new();
    let mut index: HashMap<Vec<u8>, usize> = HashMap::new();
    for row in rows {
        let parent = match row.path.iter().rposition(|&b| b == b'/') {
            Some(i) => row.path[..i].to_vec(),
            None => Vec::new(),
        };
        let qi = *index.entry(parent).or_insert_with(|| {
            queues.push(std::collections::VecDeque::new());
            queues.len() - 1
        });
        queues[qi].push_back(row);
    }
    let mut out = Vec::with_capacity(total);
    while out.len() < total {
        for q in queues.iter_mut() {
            if let Some(row) = q.pop_front() {
                out.push(row);
            }
        }
    }
    out
}

pub(crate) fn sort_deepest_first(dirs: &mut [RowView]) {
    dirs.sort_by(|a, b| {
        let da = a.path.iter().filter(|&&b| b == b'/').count();
        let db = b.path.iter().filter(|&&b| b == b'/').count();
        db.cmp(&da)
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use migration_core::schema::FileTypeTag;

    fn row(inode: Option<u64>, fsid: Option<u64>, nlink: Option<u32>) -> RowView {
        RowView {
            row_id: 1,
            path: b"/data/file".to_vec(),
            size: 1024,
            mtime_sec: None,
            mtime_nsec: None,
            atime_sec: None,
            atime_nsec: None,
            mode: 0o100644,
            uid: None,
            gid: None,
            nlink,
            inode,
            fsid,
            xattr_blob: None,
            symlink_target: None,
            file_type: FileTypeTag::Regular,
        }
    }

    #[test]
    fn hardlink_key_is_none_without_inode() {
        assert_eq!(hardlink_key(&row(None, Some(1), Some(2))), None);
    }

    #[test]
    fn hardlink_key_includes_fsid_when_present() {
        assert_eq!(
            hardlink_key(&row(Some(42), Some(7), Some(2))),
            Some((7, 42)),
        );
    }

    #[test]
    fn hardlink_key_is_none_without_fsid() {
        assert_eq!(hardlink_key(&row(Some(42), None, Some(2))), None);
    }

    #[test]
    fn rows_on_different_filesystems_do_not_collide() {
        let a = hardlink_key(&row(Some(42), Some(1), Some(2))).unwrap();
        let b = hardlink_key(&row(Some(42), Some(2), Some(2))).unwrap();
        assert_ne!(a, b);
    }

    fn dir_row(path: &[u8]) -> RowView {
        let mut r = row(None, None, None);
        r.path = path.to_vec();
        r.file_type = FileTypeTag::Dir;
        r
    }

    #[test]
    fn sort_deepest_first_orders_children_before_parents() {
        let mut dirs = vec![dir_row(b"/a"), dir_row(b"/a/b/c"), dir_row(b"/a/b")];
        sort_deepest_first(&mut dirs);
        let paths: Vec<&[u8]> = dirs.iter().map(|r| r.path.as_slice()).collect();
        assert_eq!(paths, vec![&b"/a/b/c"[..], &b"/a/b"[..], &b"/a"[..]]);
    }

    #[test]
    fn sort_deepest_first_is_stable_at_equal_depth() {
        let mut dirs = vec![dir_row(b"/a/x"), dir_row(b"/a/y"), dir_row(b"/a/z")];
        sort_deepest_first(&mut dirs);
        let paths: Vec<&[u8]> = dirs.iter().map(|r| r.path.as_slice()).collect();
        assert_eq!(paths, vec![&b"/a/x"[..], &b"/a/y"[..], &b"/a/z"[..]]);
    }

    // ---- R8: record_outcome Fenced special-case ------------------
    //
    // The classification rules are the load-bearing piece of R8 from
    // the operator's POV. M5 assertion E ("the failures/host-<id>/
    // prefix is empty or absent") becomes trivially false if a Fenced
    // row is mis-routed into the failures sink, so these tests pin
    // the routing for each of the three buckets.

    use migration_mover::strategy::Strategy;
    use migration_mover::MoveError;

    fn fenced_err() -> MoveError {
        MoveError::new(FailurePhase::Fenced, "FENCE_TRIPPED")
    }

    #[test]
    fn record_outcome_routes_fenced_to_counter_not_sink() {
        let sink = FailureSink::new();
        let throughput = ThroughputCounter::new();
        let mut outcome = ProcessOutcome::default();
        let mo = MoveOutcome {
            row_id: 42,
            strategy: Strategy::LibnfsIoUring,
            bytes_moved: 0,
            torn: false,
            result: Err(fenced_err()),
        };

        record_outcome(
            b"/data/file",
            mo,
            &mut outcome,
            &sink,
            &throughput,
            &crate::events::EventEmitter::disabled(),
        );

        assert_eq!(outcome.files_fenced, 1, "Fenced must bump files_fenced");
        assert_eq!(outcome.files_failed, 0, "Fenced must NOT bump files_failed");
        assert_eq!(outcome.files_ok, 0);
        assert_eq!(outcome.bytes_moved, 0);
        assert!(
            sink.is_empty(),
            "Fenced must NOT land in the failures sink (M5 assertion E)",
        );
    }

    #[test]
    fn record_outcome_routes_non_fenced_err_to_failures_sink() {
        // Sanity: pre-R8 routing for a genuine per-file failure must
        // still record to the sink and bump files_failed.
        let sink = FailureSink::new();
        let throughput = ThroughputCounter::new();
        let mut outcome = ProcessOutcome::default();
        let mo = MoveOutcome {
            row_id: 7,
            strategy: Strategy::LibnfsIoUring,
            bytes_moved: 0,
            torn: false,
            result: Err(MoveError::new(FailurePhase::Write, "ENOSPC")),
        };

        record_outcome(
            b"/data/file",
            mo,
            &mut outcome,
            &sink,
            &throughput,
            &crate::events::EventEmitter::disabled(),
        );

        assert_eq!(outcome.files_failed, 1);
        assert_eq!(outcome.files_fenced, 0);
        assert_eq!(sink.len(), 1, "non-Fenced Err must land in failures sink");
    }

    /// A special node the mover left out is an omission. The mover's
    /// result is `Ok`, but the row must not reach the general success
    /// arm: it is not a copied file, not a failure, moves no bytes, and
    /// adds nothing to the throughput sample.
    #[test]
    fn record_outcome_counts_a_special_node_as_an_omission_not_a_success() {
        let sink = FailureSink::new();
        let throughput = ThroughputCounter::new();
        let mut outcome = ProcessOutcome::default();
        let mo = MoveOutcome {
            row_id: 9,
            strategy: Strategy::SpecialNotCopied,
            bytes_moved: 0,
            torn: false,
            result: Ok(()),
        };

        record_outcome(
            b"/data/fifo-\xff",
            mo,
            &mut outcome,
            &sink,
            &throughput,
            &crate::events::EventEmitter::disabled(),
        );

        assert_eq!(outcome.files_special_not_copied, 1);
        assert_eq!(outcome.files_ok, 0, "an omission is not a copied file");
        assert_eq!(outcome.files_failed, 0, "and not a failure");
        assert_eq!(outcome.files_fenced, 0);
        assert_eq!(outcome.files_torn, 0);
        assert_eq!(outcome.bytes_moved, 0);
        assert_eq!(outcome.rows_processed(), 1, "but it is a processed row");
        assert!(sink.is_empty(), "nothing for a retry to pick up");
        assert_eq!(throughput.sample_mb_s(60), 0.0);
    }

    /// Shard completion and interruption reporting count every final
    /// classification once: copied, failed, and special-not-copied.
    /// Fenced rows are not processed.
    #[test]
    fn rows_processed_sums_each_final_classification_once() {
        let outcome = ProcessOutcome {
            rows_total: 10,
            files_ok: 5,
            files_failed: 2,
            files_special_not_copied: 3,
            files_fenced: 4,
            files_torn: 1, // inside files_ok
            ..ProcessOutcome::default()
        };
        assert_eq!(outcome.rows_processed(), 10);
    }

    #[test]
    fn record_outcome_success_path_unchanged_by_r8() {
        // Regression guard: the Ok branch must still bump files_ok
        // and bytes_moved, untouched by the new Fenced arm.
        let sink = FailureSink::new();
        let throughput = ThroughputCounter::new();
        let mut outcome = ProcessOutcome::default();
        let mo = MoveOutcome {
            row_id: 1,
            strategy: Strategy::LibnfsIoUring,
            bytes_moved: 4096,
            torn: false,
            result: Ok(()),
        };

        record_outcome(
            b"/data/file",
            mo,
            &mut outcome,
            &sink,
            &throughput,
            &crate::events::EventEmitter::disabled(),
        );

        assert_eq!(outcome.files_ok, 1);
        assert_eq!(outcome.files_failed, 0);
        assert_eq!(outcome.files_fenced, 0);
        assert_eq!(outcome.files_special_not_copied, 0);
        assert_eq!(outcome.bytes_moved, 4096);
        assert!(sink.is_empty());
    }

    // ---- F05: torn-copy wire-up -----------------------------------
    //
    // Both regular-file movers detect a source modified mid-copy and surface
    // it on `MoveOutcome::torn`; the per-shard summary counts that bit in
    // `files_torn`. Drive the async classifier end-to-end here (no NFS
    // needed); mover.rs separately covers the sync/raw outcome plumbing.

    #[test]
    fn files_torn_increments_when_classifier_flags_torn() {
        use migration_mover::file_mover::classify_copy;
        use migration_mover::libnfs::asyncio::NfsStat64;
        use migration_mover::FileCopyResult;

        fn stat(size: u64, mtime: u64, ctime: u64) -> NfsStat64 {
            NfsStat64 {
                dev: 0,
                ino: 1,
                mode: 0o100644,
                nlink: 1,
                uid: 0,
                gid: 0,
                rdev: 0,
                size,
                blksize: 4096,
                blocks: 0,
                atime: 0,
                mtime,
                ctime,
                atime_nsec: 0,
                mtime_nsec: 0,
                ctime_nsec: 0,
                used: 0,
            }
        }

        let r = row(None, None, None);
        let copy = FileCopyResult {
            file_hash: [0; 16],
            bytes_copied: 1024,
            torn: true,
            pre_stat: stat(1024, 100, 100),
            post_stat: stat(1024, 200, 200),
        };
        let disposition = classify_copy(&copy, &r);
        assert!(disposition.commits(), "torn must still commit");

        let sink = FailureSink::new();
        let throughput = ThroughputCounter::new();
        let mut outcome = ProcessOutcome::default();
        let mo = MoveOutcome {
            row_id: r.row_id,
            strategy: Strategy::LibnfsIoUring,
            bytes_moved: copy.bytes_copied,
            torn: disposition.downgrade().is_some(),
            result: Ok(()),
        };

        record_outcome(
            b"/data/file",
            mo,
            &mut outcome,
            &sink,
            &throughput,
            &crate::events::EventEmitter::disabled(),
        );

        assert_eq!(outcome.files_torn, 1, "torn commit must bump files_torn");
        assert_eq!(outcome.files_ok, 1, "torn still counts as copied");
        assert_eq!(outcome.files_failed, 0, "torn is NOT a failure");
        assert!(sink.is_empty(), "torn must not land in the failures sink");
    }

    // ---- dispatch -------------------------------------------------

    /// Succeeds at once for small rows; a large row waits until every
    /// small row is done. `fail` names a path that fails.
    struct Gated {
        downgrades: migration_mover::DowngradeSink,
        smalls: usize,
        small_done: std::sync::atomic::AtomicUsize,
        calls: std::sync::atomic::AtomicUsize,
        fail: Option<&'static [u8]>,
    }

    #[async_trait::async_trait]
    impl FileMover for Gated {
        async fn move_one(&self, row: &RowView) -> MoveOutcome {
            use std::sync::atomic::Ordering::SeqCst;
            self.calls.fetch_add(1, SeqCst);
            if SizeClass::classify(row.size) == SizeClass::Large {
                while self.small_done.load(SeqCst) < self.smalls {
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                }
            } else {
                self.small_done.fetch_add(1, SeqCst);
            }
            let result = match self.fail {
                Some(path) if path == row.path.as_slice() => {
                    Err(MoveError::new(FailurePhase::Write, "EIO"))
                }
                _ => Ok(()),
            };
            MoveOutcome {
                row_id: row.row_id,
                strategy: Strategy::LibnfsIoUring,
                bytes_moved: row.size,
                torn: false,
                result,
            }
        }

        async fn move_hardlink(&self, row: &RowView, _target: &[u8]) -> MoveOutcome {
            self.move_one(row).await
        }

        fn downgrade_sink(&self) -> &migration_mover::DowngradeSink {
            &self.downgrades
        }
    }

    fn processor(mover: Arc<Gated>) -> ShardProcessor {
        ShardProcessor {
            mover,
            live: Arc::new(crate::heartbeat::LivePending::default()),
            fence: Fence::new(),
            budget: BatchBudget::default(),
            inflight: InflightLimiter::new(&migration_mover::batch::InflightProfile {
                small: 2,
                medium: 1,
                large: 1,
                ..Default::default()
            }),
            failures: FailureSink::new(),
            throughput: ThroughputCounter::new(),
            fsid_ungrouped_warned: false,
            emitter: crate::events::EventEmitter::disabled(),
            run_control: None,
            stop: CancellationToken::new(),
        }
    }

    fn gated(smalls: usize, fail: Option<&'static [u8]>) -> Arc<Gated> {
        Arc::new(Gated {
            downgrades: migration_mover::DowngradeSink::new(),
            smalls,
            small_done: Default::default(),
            calls: Default::default(),
            fail,
        })
    }

    /// Rows `[from, to)`, each of `size` bytes.
    fn sized_rows(from: u64, to: u64, size: u64) -> Vec<Job> {
        (from..to)
            .map(|i| {
                let mut r = row(None, None, None);
                r.row_id = i;
                r.path = format!("/data/f{i}").into_bytes();
                r.size = size;
                Job::Row(r)
            })
            .collect()
    }

    /// Eight large files come first, and each waits until the small
    /// files behind them are done. With one queue for all classes,
    /// the large ones fill every slot and nothing moves.
    #[tokio::test]
    async fn large_files_waiting_never_hold_back_small_ones() {
        let mover = gated(8, None);
        let mut p = processor(Arc::clone(&mover));
        let mut jobs = sized_rows(0, 8, 2 << 30);
        jobs.extend(sized_rows(8, 16, 100));
        let mut outcome = ProcessOutcome::default();
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            p.dispatch(
                jobs.into_iter().map(Ok),
                usize::MAX,
                Phase::Rows,
                &mut outcome,
            ),
        )
        .await
        .expect("the small files ran past the waiting large ones")
        .unwrap();
        assert_eq!(outcome.files_ok, 16);
        assert_eq!(
            p.live
                .rows_started
                .load(std::sync::atomic::Ordering::SeqCst),
            16
        );
    }

    #[tokio::test]
    async fn the_restamp_records_only_failures() {
        let mover = gated(0, Some(b"/data/f3"));
        let mut p = processor(Arc::clone(&mover));
        let mut outcome = ProcessOutcome::default();
        let jobs = sized_rows(0, 10, 4096).into_iter().map(Ok);
        p.dispatch(jobs, 4, Phase::Restamp, &mut outcome)
            .await
            .unwrap();
        assert_eq!(mover.calls.load(std::sync::atomic::Ordering::SeqCst), 10);
        assert_eq!(outcome.files_ok, 0, "counted when their batch ran");
        assert_eq!(outcome.files_failed, 1);
        assert_eq!(p.failures.len(), 1);
    }

    #[tokio::test]
    async fn a_tripped_fence_starts_nothing_more() {
        let mover = gated(0, None);
        let mut p = processor(Arc::clone(&mover));
        p.fence.trip("test");
        let mut outcome = ProcessOutcome::default();
        let jobs = sized_rows(0, 10, 100).into_iter().map(Ok);
        p.dispatch(jobs, usize::MAX, Phase::Rows, &mut outcome)
            .await
            .unwrap();
        assert_eq!(mover.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(outcome.rows_processed(), 0);
    }

    /// A source that fails starts nothing more: the two rows running
    /// when it fails finish, the one queued behind them never starts,
    /// and the error is reported.
    #[tokio::test]
    async fn a_failing_source_drains_then_reports() {
        let mover = gated(0, None);
        let mut p = processor(Arc::clone(&mover));
        let mut outcome = ProcessOutcome::default();
        let jobs = sized_rows(0, 3, 100)
            .into_iter()
            .map(Ok)
            .chain(std::iter::once(Err(anyhow::anyhow!("torn shard"))))
            .chain(sized_rows(3, 6, 100).into_iter().map(Ok));
        let err = p
            .dispatch(jobs, 2, Phase::Rows, &mut outcome)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("torn shard"), "{err:#}");
        assert_eq!(mover.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(outcome.files_ok, 2);
    }
}
