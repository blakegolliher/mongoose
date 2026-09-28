//! `mongoose copy` — process the prepared local shards with the
//! vamoose mover.
//!
//! Shards are processed **sequentially**; within each shard the
//! existing `migration_worker::shard_processor::ShardProcessor`
//! dispatches rows concurrently (hardlink groups sequential, size-class
//! inflight limits, deepest-first dir attrs). Mongoose supplies the
//! single-host equivalents of the worker's distributed collaborators:
//! a fence that never trips, a disabled coord event emitter, no run
//! control, and local JSONL sinks instead of S3 flushes.
//!
//! SIGINT/SIGTERM is honored at the next batch boundary — no row is
//! ever interrupted mid-copy. The interrupted shard is not marked
//! completed, so a re-run reprocesses it from the top (copies are
//! idempotent: `.partial` + rename).

use crate::cli::Tuning;
use crate::endpoint;
use crate::identity;
use crate::manifest::{self, LocalManifest};
use crate::progress::{write_shard_jsonl, CopyProgress};
use crate::util::raise_fd_limit;
use crate::workdir::WorkDir;
use anyhow::{Context, Result};
use migration_core::fence::Fence;
use migration_mover::batch::{BatchBudget, InflightLimiter, InflightProfile};
use migration_mover::{DowngradeSink, FailureSink};
use migration_worker::caps;
use migration_worker::coord_driver::EventEmitter;
use migration_worker::heartbeat::LivePending;
use migration_worker::mover_factory::{self, MoverParams};
use migration_worker::shard_processor::ShardProcessor;
use migration_worker::throughput::ThroughputCounter;
use std::path::Path;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// How often the ticker logs live counters while a shard runs.
const TICK_SECS: u64 = 15;

#[derive(Debug, Default, Clone)]
pub struct CopySummary {
    pub shards_total: u64,
    pub shards_done: u64,
    pub files_ok: u64,
    pub files_failed: u64,
    pub files_torn: u64,
    pub bytes_moved: u64,
    /// Stopped by SIGINT/SIGTERM at a batch boundary; re-run
    /// `mongoose copy` to resume from the interrupted shard.
    pub interrupted: bool,
}

/// In-flight file limits for a `--parallel` level, scaled from the
/// engine defaults (256/16/4 at 32 pairs): eight small files per
/// pair, one medium file per two pairs, one large file per eight.
pub fn inflight_for(tuning: &Tuning) -> InflightProfile {
    let p = tuning.parallel.max(1) as usize;
    InflightProfile {
        small: p * 8,
        medium: (p / 2).max(1),
        large: (p / 8).max(1),
        ..InflightProfile::default()
    }
}

/// Project the local manifest + `--parallel` onto the shared mover
/// factory parameters. Everything else the engine lets a caller tune
/// is fixed here: the raw-FH fast path (always), atomic `.partial` +
/// rename publish (never direct commit), the sync context pool, and
/// the default per-RPC timeout. Pure; unit-tested without libnfs.
pub fn mover_params(
    m: &LocalManifest,
    tuning: &Tuning,
    require_chown: bool,
    host_id: String,
    same_server: bool,
) -> MoverParams {
    MoverParams {
        source_url: m.source.url.clone(),
        dest_url: m.dest.url.clone(),
        source_root: m.source.root.clone(),
        dest_root: m.dest.root.clone(),
        options: m.options.clone(),
        nfs_connections: tuning.parallel.max(1) as usize,
        use_bucketed_pool: false,
        use_raw_fh: true,
        direct_commit: false,
        rpc_timeout_ms: migration_mover::DEFAULT_RPC_TIMEOUT_MS,
        require_chown,
        // Walker `size` is advisory (SCHEMA_CONTRACT.md "Size
        // semantics"); source truth wins, same as the worker default.
        require_unchanged_size: false,
        inflight: inflight_for(tuning),
        host_id,
        same_server,
    }
}

pub async fn run(work_dir: &Path, tuning: &Tuning) -> Result<CopySummary> {
    run_manifest(work_dir, tuning, "manifest.json").await
}

/// [`run`] against a named manifest inside the work dir. `mongoose
/// sync` points this at a pass dir's delta manifest; everything else
/// (progress, sinks, resume) behaves identically.
pub async fn run_manifest(
    work_dir: &Path,
    tuning: &Tuning,
    manifest_name: &str,
) -> Result<CopySummary> {
    let wd = WorkDir::new(work_dir);
    let m = manifest::load_file(&wd.root().join(manifest_name))?.ok_or_else(|| {
        anyhow::anyhow!(
            "no {manifest_name} under {}; run `mongoose copy` first",
            wd.root().display()
        )
    })?;

    // Endpoint separation, re-proved on every run from the recorded
    // manifest (which may have been hand-edited): layers 1 and 2 —
    // canonical spelling and name resolution — before anything is
    // mounted; layer 3, the servers' own view, once the pool is up
    // and before the first destination write.
    let src_url = endpoint::parse("manifest source.url", &m.source.url)?;
    let dst_url = endpoint::parse("manifest dest.url", &m.dest.url)?;
    endpoint::check_overlap(&src_url, &dst_url)?;
    let names = endpoint::check_overlap_resolved(&src_url, &dst_url)?;

    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        tracing::warn!("mongoose copy is not running as root; libnfs needs reserved ports");
    }
    raise_fd_limit();
    let cap_chown = caps::has_cap_chown();
    if m.options.preserve_owner && !cap_chown {
        tracing::warn!(
            "preserve_owner=true but CAP_CHOWN not held; running in degraded mode \
             (chown EPERM will be recorded as downgrades, not failures)",
        );
    }

    println!(
        "copying {} shards ({} entries) with --parallel {}\n",
        m.shards.len(),
        m.total_rows,
        tuning.parallel,
    );

    // Stop token: SIGINT/SIGTERM finishes the batch in flight and
    // leaves the shard at the boundary. A second signal is logged and
    // ignored (SIGKILL is the escape hatch).
    let stop = CancellationToken::new();
    spawn_signal_listener(stop.clone());

    let host_id = hostname();
    let fence = Fence::new();
    let downgrades = DowngradeSink::new();
    let failures = FailureSink::new();
    let params = mover_params(&m, tuning, cap_chown, host_id, names.may_be_same_server());
    let built = mover_factory::build(&params, downgrades.clone(), fence.clone()).await?;
    let separation = identity::prove_separation(
        &built.pool,
        &src_url,
        &dst_url,
        &names,
        &full_index_shards(&wd, &m),
    )
    .await?;
    println!("  endpoints: {}\n", separation.evidence);
    let throughput = ThroughputCounter::new();
    let inflight = InflightLimiter::new(&params.inflight);
    let live = Arc::new(LivePending::default());

    let mut progress = CopyProgress::load_or_fresh(&wd, &m.run_id, m.shards.len() as u64)?;
    let resumed = progress.completed_shards.len();
    if resumed > 0 {
        println!(
            "  resuming: {resumed} of {} shards already completed\n",
            m.shards.len()
        );
    }
    progress.write(&wd)?;

    // Live visibility while a big shard runs: log counters + MB/s on
    // an interval. The per-shard summary remains the durable record.
    let ticker = spawn_ticker(Arc::clone(&live), throughput.clone());

    let mut summary = CopySummary {
        shards_total: m.shards.len() as u64,
        shards_done: resumed as u64,
        files_ok: progress.files_ok,
        files_failed: progress.files_failed,
        files_torn: progress.files_torn,
        bytes_moved: progress.bytes_moved,
        interrupted: false,
    };

    for shard in &m.shards {
        if progress.is_completed(&shard.path) {
            continue;
        }
        if stop.is_cancelled() {
            summary.interrupted = true;
            break;
        }
        let parquet = wd.shard_path(&shard.path);
        let size = std::fs::metadata(&parquet)
            .map(|md| md.len())
            .with_context(|| format!("shard {} is missing", parquet.display()))?;
        if size != shard.bytes {
            anyhow::bail!(
                "shard {} is {size} bytes; manifest says {} — the index changed since \
                 prepare, refusing to copy from it",
                parquet.display(),
                shard.bytes
            );
        }

        println!("shard {} ({} rows)", shard.file_name(), shard.rows);
        downgrades.set_current_shard(shard.file_name());
        failures.set_current_shard(shard.file_name());
        live.reset();

        let mut processor = ShardProcessor {
            mover: Arc::clone(&built.mover),
            live: Arc::clone(&live),
            fence: fence.clone(),
            budget: BatchBudget::default(),
            inflight: inflight.clone(),
            failures: failures.clone(),
            throughput: throughput.clone(),
            dir_restamp: Vec::new(),
            fsid_fallback_warned: false,
            emitter: EventEmitter::disabled(),
            run_control: None,
            stop: stop.clone(),
        };
        let outcome = processor
            .process(&parquet)
            .await
            .with_context(|| format!("processing shard {}", parquet.display()))?;

        // Drain this shard's failure/downgrade records to local JSONL
        // before the shard is marked complete, so an interruption
        // between the two never loses records.
        if let Some(p) =
            write_shard_jsonl(&wd.failures_dir(), shard.stem(), &failures.drain_jsonl())?
        {
            println!("  failures  -> {}", p.display());
        }
        if let Some(p) = write_shard_jsonl(
            &wd.downgrades_dir(),
            shard.stem(),
            &downgrades.drain_jsonl(),
        )? {
            println!("  downgrades -> {}", p.display());
        }

        summary.files_ok += outcome.files_ok;
        summary.files_failed += outcome.files_failed;
        summary.files_torn += outcome.files_torn;
        summary.bytes_moved += outcome.bytes_moved;

        if outcome.interrupted {
            // Rows already committed are durable; the shard is NOT
            // completed and a re-run reprocesses it from row 0. Its
            // partial counters are deliberately NOT persisted into
            // progress.json — the re-run's full-shard counters would
            // double-count them.
            println!(
                "  interrupted at a batch boundary ({}/{} rows done); \
                 re-run `mongoose copy` to resume",
                outcome.files_ok + outcome.files_failed,
                outcome.rows_total
            );
            summary.interrupted = true;
            progress.write(&wd)?;
            break;
        }

        progress.files_ok += outcome.files_ok;
        progress.files_failed += outcome.files_failed;
        progress.files_torn += outcome.files_torn;
        progress.bytes_moved += outcome.bytes_moved;
        progress.throughput_mb_s_1m = throughput.sample_mb_s(60);
        progress.completed_shards.push(shard.path.clone());
        summary.shards_done += 1;
        progress.write(&wd)?;
        println!(
            "  done: {} ok, {} failed, {} bytes",
            outcome.files_ok, outcome.files_failed, outcome.bytes_moved
        );
    }

    ticker.abort();

    if !summary.interrupted && summary.shards_done == summary.shards_total {
        // The walker emits no row for the migration root, so every
        // file commit under it has bumped its mtime; stamp it back
        // from source. Best-effort — the bytes are already durable.
        if let Err(e) = migration_mover::restore_root_mtime(
            Arc::clone(&built.pool),
            m.source.root.as_bytes(),
            m.dest.root.as_bytes(),
        )
        .await
        {
            tracing::warn!(error = ?e, "root-dir mtime restore failed (non-fatal)");
        }
        progress.done = true;
        progress.write(&wd)?;
    }

    println!(
        "\n{}: {}/{} shards, {} files ok ({} torn), {} failed, {} bytes moved",
        if summary.interrupted {
            "interrupted"
        } else if summary.shards_done == summary.shards_total {
            "complete"
        } else {
            "stopped"
        },
        summary.shards_done,
        summary.shards_total,
        summary.files_ok,
        summary.files_torn,
        summary.files_failed,
        summary.bytes_moved,
    );
    if summary.files_failed > 0 {
        println!(
            "per-file failures recorded under {}",
            wd.failures_dir().display()
        );
    }
    Ok(summary)
}

/// Every shard of this work dir's full index (`manifest.json`), for
/// the identity check's search of the source tree. A delta manifest
/// is a subset; the full index sits beside it. Falls back to the
/// manifest being copied.
fn full_index_shards(wd: &WorkDir, m: &LocalManifest) -> Vec<std::path::PathBuf> {
    let full = manifest::load(wd).ok().flatten();
    let shards = full.as_ref().map(|f| &f.shards).unwrap_or(&m.shards);
    shards.iter().map(|s| wd.shard_path(&s.path)).collect()
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "mongoose".to_string())
}

/// Cancel `stop` on the first SIGINT/SIGTERM; later signals are logged
/// and ignored. Shared by the copy loop and cutover verification.
pub(crate) fn spawn_signal_listener(stop: CancellationToken) {
    tokio::spawn(async move {
        let mut term =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(error = ?e, "cannot install SIGTERM handler");
                    return;
                }
            };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
        tracing::info!("stop requested; finishing the batch in flight, then leaving the shard");
        stop.cancel();
        // Further signals: log and keep going; the batch always
        // finishes (SIGKILL is the escape hatch).
        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
            tracing::warn!(
                "already stopping; the batch in flight will finish (use SIGKILL to abandon it)"
            );
        }
    });
}

fn spawn_ticker(
    live: Arc<LivePending>,
    throughput: ThroughputCounter,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(TICK_SECS));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick.tick().await; // immediate first tick carries no info
        loop {
            tick.tick().await;
            use std::sync::atomic::Ordering::Relaxed;
            tracing::info!(
                shard_rows_done = live.rows_done.load(Relaxed),
                inflight = live.inflight(),
                files_ok = live.files_ok.load(Relaxed),
                files_failed = live.files_failed.load(Relaxed),
                mb_s_1m = format!("{:.1}", throughput.sample_mb_s(60)),
                "copying",
            );
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{LocalShard, MANIFEST_FORMAT_VERSION};
    use migration_core::records::{Endpoint, EndpointKind, MigrationOptions};

    fn tuning() -> Tuning {
        Tuning { parallel: 24 }
    }

    fn local_manifest() -> LocalManifest {
        LocalManifest {
            format_version: MANIFEST_FORMAT_VERSION,
            run_id: "run-t".into(),
            created_utc: "2026-08-31T00:00:00Z".into(),
            source: Endpoint {
                kind: EndpointKind::Nfs,
                url: "nfs://s/e".into(),
                root: "/data".into(),
            },
            dest: Endpoint {
                kind: EndpointKind::Nfs,
                url: "nfs://d/e".into(),
                root: "/copy".into(),
            },
            options: MigrationOptions::default(),
            shards: vec![LocalShard {
                path: "canonical/part-0000.parquet".into(),
                rows: 1,
                bytes: 1,
                sha256: "x".into(),
            }],
            total_rows: 1,
            total_bytes: 1,
        }
    }

    #[test]
    fn mover_params_project_manifest_and_tuning() {
        let p = mover_params(&local_manifest(), &tuning(), true, "h".into(), true);
        assert!(
            p.same_server,
            "the identity verdict arms the per-file check"
        );
        assert_eq!(p.source_url, "nfs://s/e");
        assert_eq!(p.dest_url, "nfs://d/e");
        assert_eq!(p.source_root, "/data");
        assert_eq!(p.dest_root, "/copy");
        assert_eq!(p.nfs_connections, 24);
        assert!(p.use_raw_fh, "raw-FH fast path is always on");
        assert!(!p.direct_commit, "atomic publish is never traded away");
        assert!(!p.use_bucketed_pool, "the sync pool is the only pool");
        assert_eq!(p.rpc_timeout_ms, migration_mover::DEFAULT_RPC_TIMEOUT_MS);
        assert!(p.require_chown);
        assert!(!p.require_unchanged_size, "walker size stays advisory");
        assert_eq!(p.inflight.small, 192);
        assert_eq!(p.inflight.medium, 12);
        assert_eq!(p.inflight.large, 3);
        // Stripe knobs keep the engine defaults.
        assert_eq!(
            p.inflight.large_stripe_size,
            InflightProfile::default().large_stripe_size
        );

        // Projection through the shared factory keeps the same values.
        let cfg = mover_factory::mover_config(&p);
        assert_eq!(cfg.source_root, "/data");
        assert!(cfg.use_raw_fh);
        assert_eq!(cfg.inflight.small, 192);
    }

    #[test]
    fn inflight_scales_with_parallel_and_never_hits_zero() {
        let d = inflight_for(&Tuning::default());
        assert_eq!(
            (d.small, d.medium, d.large),
            (256, 16, 4),
            "defaults match the engine"
        );
        let one = inflight_for(&Tuning { parallel: 1 });
        assert_eq!((one.small, one.medium, one.large), (8, 1, 1));
        let max = inflight_for(&Tuning {
            parallel: crate::cli::MAX_PARALLEL,
        });
        assert_eq!((max.small, max.medium, max.large), (800, 50, 12));
    }

    #[tokio::test]
    async fn copy_without_a_manifest_names_the_command_to_run() {
        let dir = tempfile::tempdir().unwrap();
        let err = run(dir.path(), &tuning()).await.unwrap_err();
        assert!(format!("{err:#}").contains("mongoose copy"), "{err:#}");
    }

    #[tokio::test]
    async fn copy_rejects_an_overlapping_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let wd = WorkDir::new(dir.path());
        let mut m = local_manifest();
        m.source.url = "nfs://h/export".into();
        m.dest.url = "nfs://h/export".into();
        m.source.root = "/".into();
        m.dest.root = "/dst".into();
        crate::util::write_json_atomic(&wd.manifest_json(), &m).unwrap();
        let err = run(dir.path(), &tuning()).await.unwrap_err();
        assert!(format!("{err:#}").contains("overlap"), "{err:#}");
    }

    /// A hand-edited manifest whose two URLs spell one server two
    /// ways is refused by the same layers as the CLI flags, before
    /// any pool mounts (these hosts do not exist; a mount would fail
    /// with a different error).
    #[tokio::test]
    async fn copy_rejects_aliased_manifest_endpoints_before_mounting() {
        for (src, dst, how) in [
            (
                "nfs://H.Example.com/export",
                "nfs://h.example.com./export/backup",
                "same host name",
            ),
            (
                "nfs://h.example.com:2049/export",
                "nfs://h.example.com/export?version=3",
                "same host name",
            ),
            (
                "nfs://localhost/export",
                "nfs://127.0.0.1/export/sub",
                "both names resolve to",
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let wd = WorkDir::new(dir.path());
            let mut m = local_manifest();
            m.source.url = src.into();
            m.dest.url = dst.into();
            m.source.root = "/".into();
            m.dest.root = "/".into();
            crate::util::write_json_atomic(&wd.manifest_json(), &m).unwrap();
            let err = run(dir.path(), &tuning()).await.unwrap_err();
            let msg = format!("{err:#}");
            assert!(msg.contains("overlap"), "{src} vs {dst}: {msg}");
            assert!(msg.contains(how), "{src} vs {dst}: {msg}");
        }
    }

    #[tokio::test]
    async fn copy_rejects_a_malformed_manifest_url() {
        let dir = tempfile::tempdir().unwrap();
        let wd = WorkDir::new(dir.path());
        let mut m = local_manifest();
        m.dest.url = "new-server:/export".into();
        crate::util::write_json_atomic(&wd.manifest_json(), &m).unwrap();
        let err = run(dir.path(), &tuning()).await.unwrap_err();
        assert!(format!("{err:#}").contains("manifest dest.url"), "{err:#}");
    }
}
