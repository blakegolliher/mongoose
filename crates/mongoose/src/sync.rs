//! `mongoose sync` — one converging resync pass, per
//! `docs/work-items/MONGOOSE_RESYNC.md`:
//!
//! rescan → rewrite (full, becomes the next baseline) → classify
//! against the previous baseline → emit NEW+DIRTY delta shards →
//! copy them with the unchanged copy loop → advance the baseline.
//!
//! A pass dir (`passes/pass-NNNN/`) has the same layout as the root
//! work dir, plus `classify/`, `delta/`, and `delta-manifest.json`.
//! Every stage checkpoints; re-running `mongoose sync` resumes the
//! in-flight pass. The baseline advance (`baseline.json`, atomic) is
//! the pass commit point — a pass interrupted before it re-runs
//! against the old baseline, which at worst recopies redundantly.
//!
//! Rows that failed or tore in the baseline pass are the **pending
//! set**: forced DIRTY even when their tuple matches, so they retry
//! every pass until they succeed. Deletions are recorded to
//! `classify/deleted.jsonl` and never propagated to the destination.
//!
//! ## `--cutover`
//!
//! Two gates, both of which must pass before the baseline advances:
//!
//! 1. **Source quiescence** — the classifier finds nothing to copy
//!    and no deletions, i.e. the source has not changed since the
//!    last sync and nothing is pending. This proves the last sync
//!    caught everything; it says nothing about the destination.
//! 2. **Destination verification** ([`crate::verify`]) — the
//!    destination is scanned with the same walker and excludes, joined
//!    against the source index, and every file and symlink is read
//!    back from both servers. A clean result is the statement "the
//!    trees match" that the README makes for a clean exit.
//!
//! A failed verification leaves the previous baseline untouched, keeps
//! the pass dir as evidence (moved to `passes/pass-NNNN-failed-<utc>/`,
//! with `verify.json` and `verify/mismatches.jsonl`), and fails the
//! command. An interrupted verification resumes on the next run.

use crate::cli::{SyncArgs, Tuning};
use crate::copy::{self, CopySummary};
use crate::endpoint::{self, NameEvidence};
use crate::identity;
use crate::manifest::{self, LocalManifest};
use crate::progress::CopyProgress;
use crate::scan::{self, ScanParams};
use crate::util::{raise_fd_limit, read_json_opt, write_json_atomic};
use crate::verify::{self, content::LibnfsChecker, VerifyParams, VerifyReport, VerifyStatus};
use crate::workdir::{job_excludes, WorkDir};
use anyhow::{Context, Result};
use base64::Engine;
use migration_core::prepare_tools as tools;
use migration_core::records::{DowngradeKind, DowngradeRecord, FailureRecord};
use migration_resync::ClassifyCounts;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

/// File name of the NEW+DIRTY manifest inside a pass dir.
pub const DELTA_MANIFEST: &str = "delta-manifest.json";

/// Hash-partition fan-out for the classifier: bounds join memory at
/// ~|baseline|/256 rows per bucket.
const CLASSIFY_BUCKETS: usize = 256;

/// Completed pass dirs to retain; older ones are pruned once the
/// baseline advances past them. Two = the new baseline plus the one
/// it was diffed against.
const KEEP_PASSES: u32 = 2;

/// `baseline.json` — which pass's canonical index the next sync
/// diffs against. `dir` is work-dir-relative (`"."` = pass 0).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct BaselineRecord {
    pass: u32,
    dir: String,
}

#[derive(Debug)]
struct Baseline {
    pass: u32,
    wd: WorkDir,
    manifest: LocalManifest,
}

/// `classify.json` — the classifier's counts, checkpointed so a
/// resumed pass (and the cutover gate) needn't re-run the join.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ClassifyCheckpoint {
    complete: bool,
    counts: ClassifyCounts,
}

#[derive(Debug, Default)]
pub struct SyncOutcome {
    pub pass: u32,
    pub counts: Option<ClassifyCounts>,
    pub copy: Option<CopySummary>,
    /// Nothing to copy: the source is unchanged since the last pass
    /// (modulo recorded deletions). Not a statement about the
    /// destination — see `verification`.
    pub in_sync: bool,
    /// The cutover verification report, when `--cutover` ran it.
    pub verification: Option<VerifyReport>,
    /// Stopped by SIGINT/SIGTERM (copy or verification); re-run to
    /// resume this pass.
    pub interrupted: bool,
}

pub async fn run(args: &SyncArgs) -> Result<SyncOutcome> {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        tracing::warn!("mongoose sync is not running as root; NFS mounts usually need sudo");
    }
    raise_fd_limit();

    let wd = WorkDir::new(&args.work_dir);
    let root_manifest = manifest::load(&wd)?.ok_or_else(|| {
        anyhow::anyhow!(
            "no manifest.json under {}; run `mongoose copy` before sync",
            wd.root().display()
        )
    })?;
    // Endpoint separation from the recorded job state, every run:
    // layers 1 and 2 here; layer 3 in the delta copy and before the
    // cutover read-back (both mount).
    let src_url = endpoint::parse("manifest source.url", &root_manifest.source.url)?;
    let dst_url = endpoint::parse("manifest dest.url", &root_manifest.dest.url)?;
    endpoint::check_overlap(&src_url, &dst_url)?;
    let names = endpoint::check_overlap_resolved(&src_url, &dst_url)?;
    // The excludes recorded with the job: the rescan must skip exactly
    // what the copy skipped, or every excluded tree classifies NEW.
    let exclude = job_excludes(&wd)?;

    let baseline = load_baseline(&wd)?;
    ensure_baseline_copied(&wd, &baseline)?;
    let pass = baseline.pass + 1;
    let pass_wd = WorkDir::new(wd.pass_dir(pass));
    std::fs::create_dir_all(pass_wd.root())
        .with_context(|| format!("creating {}", pass_wd.root().display()))?;

    println!(
        "mongoose sync\n  job      {}\n  source   {}\n  dest     {}\n  pass     {pass} (baseline: pass {})\n  work     {}\n",
        root_manifest.run_id,
        tools::scan_url(&root_manifest.source.url, &root_manifest.source.root),
        tools::scan_url(&root_manifest.dest.url, &root_manifest.dest.root),
        baseline.pass,
        pass_wd.root().display(),
    );

    let steps = if args.cutover { 6 } else { 5 };

    // ---- 1. rescan --------------------------------------------------
    println!("[1/{steps}] rescan");
    let scan = scan::ensure_scan(
        &pass_wd,
        &source_scan_params(&root_manifest, &exclude, &args.tuning),
    )
    .await?;

    // ---- 2. rewrite the full rescan (the next baseline) -------------
    println!("[2/{steps}] canonical rewrite");
    let full = match manifest::load(&pass_wd)? {
        Some(m) => {
            println!("  pass index already built; not rewriting");
            m
        }
        None => {
            scan::ensure_canonical(&pass_wd, &scan).await?;
            manifest::build(
                &pass_wd,
                &root_manifest.run_id,
                root_manifest.source.clone(),
                root_manifest.dest.clone(),
                root_manifest.options.clone(),
            )?
        }
    };

    // ---- 3. classify ------------------------------------------------
    println!("[3/{steps}] classify against baseline");
    let counts = ensure_classify(&baseline, &pass_wd, &full)?;
    println!(
        "  new {} | dirty {} | pending {} | unchanged {} | deleted {} (recorded only)",
        counts.new, counts.dirty_tuple, counts.dirty_pending, counts.unchanged, counts.deleted,
    );

    let mut outcome = SyncOutcome {
        pass,
        counts: Some(counts.clone()),
        copy: None,
        in_sync: counts.keep_rows == 0,
        verification: None,
        interrupted: false,
    };

    if args.cutover {
        // Gate 1: with source writers stopped, a converged source shows
        // zero rows to copy and zero deletions. Anything else is drift
        // — fail loudly before touching the destination.
        let drift = counts.keep_rows + counts.deleted;
        if drift != 0 {
            anyhow::bail!(
                "cutover found drift: {} new, {} dirty, {} pending, {} deleted. Source \
                 writers are supposed to be stopped. Run `mongoose sync` without --cutover \
                 to copy the drift, then retry --cutover.",
                counts.new,
                counts.dirty_tuple,
                counts.dirty_pending,
                counts.deleted,
            );
        }
        println!("  cutover gate 1: source unchanged since the last sync (nothing to copy, no deletions)");

        // Gate 2: the destination itself, independently of gate 1.
        let report = cutover_verify(
            &pass_wd,
            pass,
            &root_manifest,
            &full,
            &exclude,
            &args.tuning,
            steps,
            (&src_url, &dst_url, &names),
        )
        .await?;
        match report.status {
            VerifyStatus::Interrupted => {
                println!(
                    "\ncutover verification interrupted after {} of {} entries; \
                     re-run `mongoose sync --cutover` to resume it",
                    report.content.entries_done,
                    report.namespace.files_to_read + report.namespace.symlinks_to_read,
                );
                outcome.verification = Some(report);
                outcome.interrupted = true;
                return Ok(outcome);
            }
            VerifyStatus::Fail => {
                let summary = format!(
                    "{} mismatches ({})",
                    report.mismatches_total,
                    report.breakdown()
                );
                let failed_dir = fail_pass_dir(&wd, pass)?;
                println!(
                    "\ncutover verification FAILED: {summary}\n  \
                     report      {}\n  \
                     mismatches  {}\n  \
                     The baseline was not advanced. This pass was moved aside as evidence; \
                     the next `mongoose sync` starts a fresh pass {pass}.",
                    failed_dir.join("verify.json").display(),
                    failed_dir.join(verify::MISMATCHES_FILE).display(),
                );
                anyhow::bail!(
                    "cutover verification failed: {summary}; see {}",
                    failed_dir.join("verify.json").display()
                );
            }
            VerifyStatus::Pass => {
                println!(
                    "  cutover gate 2: destination verified — {} entries compared, {} files \
                     ({} bytes) and {} symlinks read back from both servers, 0 mismatches",
                    report.namespace.matched,
                    report.content.files_read,
                    report.content.bytes_read,
                    report.content.symlinks_read,
                );
                outcome.verification = Some(report);
            }
        }
    } else if counts.keep_rows == 0 {
        // ---- 4. nothing to copy ---------------------------------------
        println!("[4/{steps}] delta: source unchanged; nothing to copy");
    } else {
        // ---- 4. emit + copy the delta ---------------------------------
        println!("[4/{steps}] delta: {} rows to copy", counts.keep_rows);
        let delta = match manifest::load_file(&pass_wd.delta_manifest_json())? {
            Some(d) => Some(d),
            None => crate::delta::emit(&pass_wd, &full)?,
        };
        let Some(delta) = delta else {
            anyhow::bail!(
                "classifier kept {} rows but the delta came out empty; classify/ and the \
                 pass index disagree — remove {} and re-run",
                counts.keep_rows,
                pass_wd.classify_dir().display(),
            );
        };
        println!(
            "  {} delta shards, {} rows, {} bytes of shard index",
            delta.shards.len(),
            delta.total_rows,
            delta.total_bytes
        );
        let summary = copy::run_manifest(pass_wd.root(), &args.tuning, DELTA_MANIFEST).await?;
        let interrupted = summary.interrupted;
        outcome.copy = Some(summary);
        if interrupted {
            println!("sync interrupted; re-run `mongoose sync` to resume this pass");
            outcome.interrupted = true;
            return Ok(outcome);
        }
    }

    // ---- 5/6. advance the baseline (pass commit point) ----------------
    println!("[{steps}/{steps}] advance baseline to pass {pass}");
    write_json_atomic(
        &wd.baseline_json(),
        &BaselineRecord {
            pass,
            dir: format!("passes/pass-{pass:04}"),
        },
    )?;
    prune_passes(&wd, pass, KEEP_PASSES);

    // The baseline has advanced: this pass's raw scan and its delta
    // shards are spent. Canonical shards stay — they are the baseline
    // the next sync classifies against.
    scan::purge_scan_output(&pass_wd);
    match std::fs::remove_dir_all(pass_wd.delta_dir()) {
        Ok(()) => tracing::debug!(path = %pass_wd.delta_dir().display(), "purged delta shards"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(
            error = %e,
            path = %pass_wd.delta_dir().display(),
            "delta purge failed (non-fatal)",
        ),
    }

    let deleted_note = if counts.deleted > 0 {
        format!(
            "; {} deletions recorded in {} (never propagated)",
            counts.deleted,
            pass_wd.classify_dir().join("deleted.jsonl").display()
        )
    } else {
        String::new()
    };
    match (&outcome.copy, &outcome.verification) {
        (_, Some(v)) => println!(
            "\ncutover pass {pass} complete: the trees match ({} entries, {} bytes verified); \
             report in {}",
            v.namespace.matched,
            v.content.bytes_read,
            pass_wd.verify_json().display(),
        ),
        (Some(c), None) => println!(
            "\nsync pass {pass} complete: {} copied, {} failed, {} bytes{deleted_note}",
            c.files_ok, c.files_failed, c.bytes_moved
        ),
        (None, None) => println!("\nsync pass {pass} complete: source unchanged{deleted_note}"),
    }
    Ok(outcome)
}

/// The rescan of the source: same URL, workers, and excludes as
/// prepare's pass-0 scan.
pub fn source_scan_params(root: &LocalManifest, exclude: &[String], tuning: &Tuning) -> ScanParams {
    ScanParams {
        scan_url: tools::scan_url(&root.source.url, &root.source.root),
        workers: tuning.walker_workers(),
        exclude: exclude.to_vec(),
    }
}

/// The cutover scan of the destination: the same walker, workers, and
/// excludes pointed at the destination URL, so an excluded subtree is
/// absent from both indexes and never reported as missing or extra.
pub fn dest_scan_params(root: &LocalManifest, exclude: &[String], tuning: &Tuning) -> ScanParams {
    ScanParams {
        scan_url: tools::scan_url(&root.dest.url, &root.dest.root),
        workers: tuning.walker_workers(),
        exclude: exclude.to_vec(),
    }
}

/// Cutover gate 2: scan the destination into `<pass>/dest/`, then run
/// (or resume) the verification of it against this pass's source
/// index. Mounts one libnfs pair per `--parallel` for the read-back,
/// and proves endpoint separation on it first: verifying a tree
/// against itself would pass.
#[allow(clippy::too_many_arguments)]
async fn cutover_verify(
    pass_wd: &WorkDir,
    pass: u32,
    root: &LocalManifest,
    full: &LocalManifest,
    exclude: &[String],
    tuning: &Tuning,
    steps: usize,
    endpoints: (&endpoint::NfsUrl, &endpoint::NfsUrl, &NameEvidence),
) -> Result<VerifyReport> {
    let dest_wd = WorkDir::new(pass_wd.dest_dir());
    std::fs::create_dir_all(dest_wd.root())
        .with_context(|| format!("creating {}", dest_wd.root().display()))?;

    println!("[4/{steps}] scan the destination");
    let dscan = scan::ensure_scan(&dest_wd, &dest_scan_params(root, exclude, tuning)).await?;
    let dest_index = match manifest::load(&dest_wd)? {
        Some(m) => {
            println!("  destination index already built; not rewriting");
            m
        }
        None => {
            scan::ensure_canonical(&dest_wd, &dscan).await?;
            manifest::build(
                &dest_wd,
                &root.run_id,
                root.source.clone(),
                root.dest.clone(),
                root.options.clone(),
            )?
        }
    };
    scan::purge_scan_output(&dest_wd);

    println!("[5/{steps}] verify the destination against the source");
    let checker = LibnfsChecker::mount(
        &root.source.url,
        &root.dest.url,
        &root.source.root,
        &root.dest.root,
        tuning.parallel.max(1) as usize,
    )?;
    let source_shards: Vec<PathBuf> = full
        .shards
        .iter()
        .map(|s| pass_wd.shard_path(&s.path))
        .collect();
    let (src_url, dst_url, names) = endpoints;
    let separation =
        identity::prove_separation(&checker.pool(), src_url, dst_url, names, &source_shards)
            .await?;
    println!("  endpoints: {}", separation.evidence);
    let stop = CancellationToken::new();
    copy::spawn_signal_listener(stop.clone());
    let params = VerifyParams {
        run_id: root.run_id.clone(),
        pass,
        source: root.source.clone(),
        dest: root.dest.clone(),
        options: root.options.clone(),
        source_shards,
        dest_shards: dest_index
            .shards
            .iter()
            .map(|s| dest_wd.shard_path(&s.path))
            .collect(),
        concurrency: tuning.parallel.max(1) as usize,
        buckets: CLASSIFY_BUCKETS,
    };
    verify::run(pass_wd, &params, checker, stop).await
}

/// Move a pass whose cutover verification failed to
/// `passes/pass-NNNN-failed-<utc>/`. Its scans, classification, and
/// report stay as evidence; the next sync starts pass NNNN afresh
/// instead of reusing checkpoints that already proved a mismatch.
/// Never pruned automatically.
fn fail_pass_dir(wd: &WorkDir, pass: u32) -> Result<PathBuf> {
    let from = wd.pass_dir(pass);
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    let to = wd
        .root()
        .join("passes")
        .join(format!("pass-{pass:04}-failed-{stamp}"));
    std::fs::rename(&from, &to)
        .with_context(|| format!("moving {} to {}", from.display(), to.display()))?;
    if let Some(parent) = to.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }
    Ok(to)
}

/// Load `baseline.json`, defaulting to pass 0 at the work-dir root.
fn load_baseline(wd: &WorkDir) -> Result<Baseline> {
    let record = read_json_opt::<BaselineRecord>(&wd.baseline_json())?.unwrap_or(BaselineRecord {
        pass: 0,
        dir: ".".to_string(),
    });
    let dir = if record.dir == "." {
        wd.root().to_path_buf()
    } else {
        wd.root().join(&record.dir)
    };
    let baseline_wd = WorkDir::new(&dir);
    let manifest = manifest::load(&baseline_wd)?.ok_or_else(|| {
        anyhow::anyhow!(
            "baseline pass {} has no manifest at {}; the work dir is damaged",
            record.pass,
            baseline_wd.manifest_json().display()
        )
    })?;
    Ok(Baseline {
        pass: record.pass,
        wd: baseline_wd,
        manifest,
    })
}

/// A sync only makes sense once the baseline's copy finished: shards
/// never copied would classify UNCHANGED and be skipped forever. For
/// pass 0 that's the root `progress.json`; for pass N the baseline
/// record itself is only written after the pass's copy completed.
fn ensure_baseline_copied(wd: &WorkDir, baseline: &Baseline) -> Result<()> {
    if baseline.pass > 0 {
        return Ok(());
    }
    let done = read_json_opt::<CopyProgress>(&wd.progress_json())?.is_some_and(|p| {
        p.run_id == baseline.manifest.run_id
            && p.completed_shards.len() as u64 == baseline.manifest.shards.len() as u64
    });
    anyhow::ensure!(
        done,
        "the initial copy has not completed for {}; finish `mongoose copy` before syncing",
        wd.root().display()
    );
    Ok(())
}

/// Classify the pass's index against the baseline, checkpointed in
/// `classify.json`.
fn ensure_classify(
    baseline: &Baseline,
    pass_wd: &WorkDir,
    full: &LocalManifest,
) -> Result<ClassifyCounts> {
    let checkpoint_path = pass_wd.root().join("classify.json");
    if let Some(cp) = read_json_opt::<ClassifyCheckpoint>(&checkpoint_path)? {
        if cp.complete {
            println!("  classify checkpoint valid; not re-classifying");
            return Ok(cp.counts);
        }
    }
    let pending = collect_pending(&baseline.wd)?;
    if !pending.is_empty() {
        println!(
            "  {} pending rows from pass {} (failed or torn) forced into the delta",
            pending.len(),
            baseline.pass
        );
    }
    let baseline_shards: Vec<PathBuf> = baseline
        .manifest
        .shards
        .iter()
        .map(|s| baseline.wd.shard_path(&s.path))
        .collect();
    let current_shards: Vec<PathBuf> = full
        .shards
        .iter()
        .map(|s| pass_wd.shard_path(&s.path))
        .collect();
    let counts = migration_resync::classify(
        &baseline_shards,
        &current_shards,
        &pending,
        &pass_wd.classify_dir(),
        CLASSIFY_BUCKETS,
    )?;
    write_json_atomic(
        &checkpoint_path,
        &ClassifyCheckpoint {
            complete: true,
            counts: counts.clone(),
        },
    )?;
    Ok(counts)
}

/// The pending set: paths whose copy failed in the baseline pass
/// (failures JSONL) or committed torn (TornCopy downgrades). Forced
/// DIRTY until a pass succeeds on them.
fn collect_pending(baseline_wd: &WorkDir) -> Result<HashSet<Vec<u8>>> {
    let mut pending = HashSet::new();
    for record in read_jsonl_dir::<FailureRecord>(&baseline_wd.failures_dir())? {
        pending.insert(decode_path(&record.path_b64)?);
    }
    for record in read_jsonl_dir::<DowngradeRecord>(&baseline_wd.downgrades_dir())? {
        if matches!(record.downgrade, DowngradeKind::TornCopy { .. }) {
            pending.insert(decode_path(&record.path_b64)?);
        }
    }
    Ok(pending)
}

fn decode_path(b64: &str) -> Result<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(b64)
        .context("decoding path_b64 from a result record")
}

/// Parse every record of every `*.jsonl` file in `dir` (absent dir =
/// no records).
fn read_jsonl_dir<T: serde::de::DeserializeOwned>(dir: &Path) -> Result<Vec<T>> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    for entry in entries {
        let path = entry?.path();
        if path.extension().is_none_or(|x| x != "jsonl") {
            continue;
        }
        let body = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        for line in body.lines().filter(|l| !l.trim().is_empty()) {
            out.push(
                serde_json::from_str(line)
                    .with_context(|| format!("parsing a record in {}", path.display()))?,
            );
        }
    }
    Ok(out)
}

/// Remove completed pass dirs older than the retention window. Never
/// touches the current baseline or the pass-0 root; best-effort.
fn prune_passes(wd: &WorkDir, current_pass: u32, keep: u32) {
    for k in 1..current_pass.saturating_sub(keep.saturating_sub(1)) {
        let dir = wd.pass_dir(k);
        if dir.exists() {
            match std::fs::remove_dir_all(&dir) {
                Ok(()) => tracing::info!(pass = k, dir = %dir.display(), "pruned old pass"),
                Err(e) => tracing::warn!(pass = k, error = ?e, "could not prune old pass"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::CopyProgress;
    use migration_core::records::FailurePhase;
    use migration_core::time::UtcTime;

    #[test]
    fn baseline_defaults_to_pass_zero_root() {
        let dir = tempfile::tempdir().unwrap();
        let wd = WorkDir::new(dir.path());
        // No manifest at all: damaged/unprepared work dir.
        let err = load_baseline(&wd).unwrap_err();
        assert!(format!("{err:#}").contains("baseline pass 0"), "{err:#}");
    }

    #[test]
    fn sync_requires_the_initial_copy_to_be_complete() {
        let dir = tempfile::tempdir().unwrap();
        let wd = WorkDir::new(dir.path());
        let m = LocalManifest {
            format_version: crate::manifest::MANIFEST_FORMAT_VERSION,
            run_id: "run-t".into(),
            created_utc: "2026-08-31T00:00:00Z".into(),
            source: migration_core::records::Endpoint {
                kind: migration_core::records::EndpointKind::Nfs,
                url: "nfs://s/e".into(),
                root: "/".into(),
            },
            dest: migration_core::records::Endpoint {
                kind: migration_core::records::EndpointKind::Nfs,
                url: "nfs://d/e".into(),
                root: "/".into(),
            },
            options: migration_core::records::MigrationOptions::default(),
            shards: vec![crate::manifest::LocalShard {
                path: "canonical/part-0000.parquet".into(),
                rows: 1,
                bytes: 1,
                sha256: "x".into(),
            }],
            total_rows: 1,
            total_bytes: 1,
        };
        crate::util::write_json_atomic(&wd.manifest_json(), &m).unwrap();
        let baseline = load_baseline(&wd).unwrap();
        assert_eq!(baseline.pass, 0);

        // No progress.json → not copied yet.
        let err = ensure_baseline_copied(&wd, &baseline).unwrap_err();
        assert!(format!("{err:#}").contains("mongoose copy"), "{err:#}");

        // Progress with the shard completed → good to sync.
        let mut p = CopyProgress::fresh("run-t", 1);
        p.completed_shards
            .push("canonical/part-0000.parquet".into());
        p.write(&wd).unwrap();
        ensure_baseline_copied(&wd, &baseline).unwrap();
    }

    #[test]
    fn pending_collects_failures_and_torn_downgrades_only() {
        let dir = tempfile::tempdir().unwrap();
        let wd = WorkDir::new(dir.path());
        std::fs::create_dir_all(wd.failures_dir()).unwrap();
        std::fs::create_dir_all(wd.downgrades_dir()).unwrap();

        let b64 = |p: &[u8]| base64::engine::general_purpose::STANDARD.encode(p);
        let failure = FailureRecord {
            row_id: 1,
            shard: "part-0000.parquet".into(),
            path_b64: b64(b"/a/failed"),
            error: "ENOSPC".into(),
            phase: FailurePhase::Write,
            ts: UtcTime::now(),
        };
        std::fs::write(
            wd.failures_dir().join("part-0000.jsonl"),
            format!("{}\n", serde_json::to_string(&failure).unwrap()),
        )
        .unwrap();

        let torn = DowngradeRecord {
            row_id: 2,
            shard: "part-0000.parquet".into(),
            path_b64: b64(b"/a/torn"),
            downgrade: DowngradeKind::TornCopy {
                pre: (1, 1, 1),
                post: (2, 2, 2),
            },
            ts: UtcTime::now(),
        };
        let benign = DowngradeRecord {
            row_id: 3,
            shard: "part-0000.parquet".into(),
            path_b64: b64(b"/a/null-owner"),
            downgrade: DowngradeKind::NullOwner,
            ts: UtcTime::now(),
        };
        std::fs::write(
            wd.downgrades_dir().join("part-0000.jsonl"),
            format!(
                "{}\n{}\n",
                serde_json::to_string(&torn).unwrap(),
                serde_json::to_string(&benign).unwrap()
            ),
        )
        .unwrap();

        let pending = collect_pending(&wd).unwrap();
        assert!(pending.contains(b"/a/failed".as_slice()));
        assert!(pending.contains(b"/a/torn".as_slice()));
        assert!(
            !pending.contains(b"/a/null-owner".as_slice()),
            "benign downgrades are not pending"
        );
        assert_eq!(pending.len(), 2);
    }

    fn manifest_for(src: &str, dst: &str) -> LocalManifest {
        LocalManifest {
            format_version: crate::manifest::MANIFEST_FORMAT_VERSION,
            run_id: "run-t".into(),
            created_utc: "2026-09-28T00:00:00Z".into(),
            source: migration_core::records::Endpoint {
                kind: migration_core::records::EndpointKind::Nfs,
                url: src.into(),
                root: "/".into(),
            },
            dest: migration_core::records::Endpoint {
                kind: migration_core::records::EndpointKind::Nfs,
                url: dst.into(),
                root: "/".into(),
            },
            options: migration_core::records::MigrationOptions::default(),
            shards: vec![],
            total_rows: 0,
            total_bytes: 0,
        }
    }

    /// The exclude set is job identity: the cutover's destination scan
    /// must skip exactly what every source scan skipped, or an excluded
    /// tree shows up as missing (or, on the destination, as extra).
    #[test]
    fn source_and_dest_scans_share_workers_and_excludes() {
        let m = manifest_for("nfs://old/export/data", "nfs://new/export/copy");
        let exclude = vec![".snapshot".to_string(), "tmp".to_string()];
        let tuning = Tuning { parallel: 8 };
        let src = source_scan_params(&m, &exclude, &tuning);
        let dst = dest_scan_params(&m, &exclude, &tuning);
        assert_eq!(src.scan_url, "nfs://old/export/data");
        assert_eq!(dst.scan_url, "nfs://new/export/copy");
        assert_eq!(src.exclude, exclude);
        assert_eq!(dst.exclude, src.exclude);
        assert_eq!(src.workers, 8);
        assert_eq!(dst.workers, src.workers);
    }

    #[test]
    fn failed_pass_is_moved_aside_with_its_evidence_and_never_pruned() {
        let dir = tempfile::tempdir().unwrap();
        let wd = WorkDir::new(dir.path());
        let pass_wd = WorkDir::new(wd.pass_dir(3));
        std::fs::create_dir_all(pass_wd.verify_dir()).unwrap();
        std::fs::write(pass_wd.verify_json(), b"{}").unwrap();

        let moved = fail_pass_dir(&wd, 3).unwrap();
        assert!(!wd.pass_dir(3).exists(), "pass 3 starts fresh next time");
        assert!(moved
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("pass-0003-failed-"));
        assert!(moved.join("verify.json").exists(), "evidence kept");

        // Pruning after later passes leaves the evidence alone.
        for k in 3..=6u32 {
            std::fs::create_dir_all(wd.pass_dir(k)).unwrap();
        }
        prune_passes(&wd, 6, 2);
        assert!(moved.exists());
        assert!(!wd.pass_dir(3).exists());
        assert!(wd.pass_dir(6).exists());
    }

    #[test]
    fn prune_keeps_the_retention_window() {
        let dir = tempfile::tempdir().unwrap();
        let wd = WorkDir::new(dir.path());
        for k in 1..=4u32 {
            std::fs::create_dir_all(wd.pass_dir(k)).unwrap();
        }
        // Baseline just advanced to pass 4; keep 2 (passes 3 and 4).
        prune_passes(&wd, 4, 2);
        assert!(!wd.pass_dir(1).exists());
        assert!(!wd.pass_dir(2).exists());
        assert!(wd.pass_dir(3).exists());
        assert!(wd.pass_dir(4).exists());
    }
}
