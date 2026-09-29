//! The embedded scan and rewrite stages, shared by `prepare` (pass 0)
//! and `sync` (every resync pass, plus the cutover's destination
//! scan). A pass dir has the same layout as the root work dir, so
//! both call these with their own [`WorkDir`].
//!
//! ## Scan completeness
//!
//! A scan is checkpointed as complete only when the walker read every
//! directory. The walker retries transient failures itself and
//! reports what it could not read as a structured error; mongoose
//! then keeps the attempt directory (part files, the walker's
//! progress log, and `errors.jsonl` with every unreadable directory)
//! as evidence, records the attempt in `scan.json` with
//! `complete: false`, and fails the command. The next run scans
//! afresh into a new attempt directory; an incomplete checkpoint is
//! never reused, so neither a sync baseline nor a cutover can be
//! built on a scan with holes. There is no permissive mode.

use crate::util::{read_json_opt, utc_now, write_json_atomic};
use crate::workdir::WorkDir;
use anyhow::{Context, Result};
use migration_core::prepare_tools as tools;
use nfs_walker::error::DirFailure;
use nfs_walker::{WalkStats, WalkerError};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Minimum seconds between scan-progress log lines.
const SCAN_LOG_SECS: u64 = 10;

// Shard size: the walker writes 512 MiB part files (its built-in
// default; the flag that set it is gone as of nfs-walker 0.2.0) and
// the rewrite is 1:1 per part, so that is the canonical shard size.
// It only shapes checkpoint granularity and memory, never the copy.

/// `scan.json`: the last scan attempt. Reusable only when
/// [`checkpoint_reusable`] says so.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanCheckpoint {
    pub complete: bool,
    /// Directory holding the part files (what the rewrite reads).
    pub scan_dir: PathBuf,
    pub walker_version: String,
    pub scan_url: String,
    pub finished_utc: String,
    /// Directories the walker could not read. Always 0 on a complete
    /// checkpoint; an incomplete attempt records what it found.
    #[serde(default)]
    pub errors: u64,
    /// Directories that disappeared during the scan (a race on a live
    /// tree; not an error).
    #[serde(default)]
    pub vanished: u64,
    /// The attempt directory this checkpoint describes.
    #[serde(default)]
    pub attempt_dir: Option<PathBuf>,
    /// The walker's `errors.jsonl`, one record per unreadable or
    /// vanished directory.
    #[serde(default)]
    pub failure_log: Option<PathBuf>,
    /// The walker's progress log.
    #[serde(default)]
    pub walker_log: Option<PathBuf>,
}

/// A complete checkpoint with no unreadable directories. An older
/// checkpoint written before `errors` was recorded parses as 0 and
/// stays reusable; a checkpoint claiming completion *with* errors
/// (hand-edited, or from a build that did not fail them) is not.
pub fn checkpoint_reusable(cp: &ScanCheckpoint) -> bool {
    cp.complete && cp.errors == 0
}

/// The scan acceptance rule: a scan counts only when it ran to the
/// end and read every directory. Belt-and-braces over the walker,
/// which already returns an error for either condition.
pub fn accept_scan(stats: &WalkStats) -> Result<()> {
    anyhow::ensure!(
        stats.completed,
        "the scan was interrupted before completion; re-run to rescan"
    );
    anyhow::ensure!(
        stats.errors == 0,
        "the walker reported {} unreadable director{} but did not fail the scan; refusing to \
         checkpoint an incomplete index",
        stats.errors,
        if stats.errors == 1 { "y" } else { "ies" },
    );
    Ok(())
}

/// Why the embedded walker did not produce an acceptable scan.
#[derive(Debug)]
pub enum ScanFailure {
    /// The walker ran to the end but could not read every directory.
    Incomplete {
        stats: WalkStats,
        failures: Vec<DirFailure>,
        failure_log: Option<PathBuf>,
    },
    /// Anything else: mount failure, writer failure, interruption.
    Other(anyhow::Error),
}

/// How many unreadable directories the failure message lists.
const FAILURE_SAMPLE: usize = 10;

/// The operator-facing error for an incomplete scan: counts, a sample
/// of what could not be read, and where the full diagnostics are.
pub fn incomplete_message(
    scan_url: &str,
    stats: &WalkStats,
    failures: &[DirFailure],
    attempt_dir: &Path,
    failure_log: Option<&Path>,
    walker_log: &Path,
) -> String {
    let mut msg = format!(
        "scan of {scan_url} is incomplete: {} director{} could not be read ({} vanished during \
         the scan); nothing was checkpointed\n",
        stats.errors,
        if stats.errors == 1 { "y" } else { "ies" },
        stats.vanished,
    );
    for f in failures.iter().take(FAILURE_SAMPLE) {
        msg.push_str(&format!(
            "  {:<18} {}  ({}; {} attempt{})\n",
            f.kind.as_str(),
            f.path,
            f.error,
            f.attempts,
            if f.attempts == 1 { "" } else { "s" }
        ));
    }
    if failures.len() > FAILURE_SAMPLE {
        msg.push_str(&format!("  ... {} more\n", failures.len() - FAILURE_SAMPLE));
    }
    msg.push_str(&format!("  attempt     {}\n", attempt_dir.display()));
    if let Some(p) = failure_log {
        msg.push_str(&format!("  failures    {}\n", p.display()));
    }
    msg.push_str(&format!("  walker log  {}\n", walker_log.display()));
    msg.push_str(
        "Fix the cause (export permissions, connectivity, a directory the server will not list) \
         and re-run; the next run scans afresh into a new attempt directory.",
    );
    msg
}

/// Everything that shapes one scan, resolved by the caller.
#[derive(Debug, Clone)]
pub struct ScanParams {
    pub scan_url: String,
    pub workers: usize,
    /// Directory-name globs (`crate::exclude`), the job's `--exclude`
    /// set; handed to the walker as `--exclude-dir`.
    pub exclude: Vec<String>,
}

/// Version label of the compiled-in scanner, recorded in the scan
/// checkpoint and stamped into each canonical shard's KV metadata.
pub fn embedded_walker_version() -> String {
    use clap::CommandFactory;
    let v = nfs_walker::CliArgs::command()
        .get_version()
        .unwrap_or("unknown")
        .to_string();
    format!("nfs-walker {v} (embedded)")
}

/// Reuse a complete scan checkpoint or run the embedded walker into a
/// fresh attempt dir. An incomplete scan is recorded and fails.
pub async fn ensure_scan(wd: &WorkDir, params: &ScanParams) -> Result<ScanCheckpoint> {
    let checkpoint_path = wd.scan_json();
    if let Some(cp) = read_json_opt::<ScanCheckpoint>(&checkpoint_path)? {
        if checkpoint_reusable(&cp) && tools::resolve_scan_dir(&cp.scan_dir).is_ok() {
            println!("  scan checkpoint valid; not rescanning");
            return Ok(cp);
        }
        if !checkpoint_reusable(&cp) {
            println!(
                "  previous scan attempt{} was incomplete ({} unreadable director{}); scanning afresh",
                cp.attempt_dir
                    .as_ref()
                    .map(|p| format!(" {}", p.display()))
                    .unwrap_or_default(),
                cp.errors,
                if cp.errors == 1 { "y" } else { "ies" },
            );
        }
    }

    // A bad pattern is refused before a scan attempt exists.
    crate::exclude::validate("--exclude", &params.exclude)?;

    let attempt_dir = wd.next_attempt_dir()?;
    std::fs::create_dir_all(&attempt_dir)?;
    let invocation = tools::WalkerInvocation {
        scan_url: params.scan_url.clone(),
        output: attempt_dir.join("walk.parquet"),
        workers: params.workers,
        exclude: Vec::new(),
        exclude_dirs: params.exclude.clone(),
        log: attempt_dir.join("walker-progress.jsonl"),
    };
    println!(
        "  scanning {} ({} workers)",
        params.scan_url, invocation.workers,
    );
    let walker_version = embedded_walker_version();
    let stats = match run_embedded_walker(&invocation).await {
        Ok(stats) => stats,
        Err(ScanFailure::Incomplete {
            stats,
            failures,
            failure_log,
        }) => {
            // Keep the attempt as evidence and record it, then fail.
            // The next run never reuses this checkpoint.
            let scan_dir = tools::resolve_scan_dir(&invocation.output)
                .unwrap_or_else(|_| invocation.output.clone());
            write_json_atomic(
                &checkpoint_path,
                &ScanCheckpoint {
                    complete: false,
                    scan_dir,
                    walker_version,
                    scan_url: params.scan_url.clone(),
                    finished_utc: utc_now(),
                    errors: stats.errors,
                    vanished: stats.vanished,
                    attempt_dir: Some(attempt_dir.clone()),
                    failure_log: failure_log.clone(),
                    walker_log: Some(invocation.log.clone()),
                },
            )?;
            anyhow::bail!(incomplete_message(
                &params.scan_url,
                &stats,
                &failures,
                &attempt_dir,
                failure_log.as_deref(),
                &invocation.log,
            ));
        }
        Err(ScanFailure::Other(e)) => return Err(e),
    };
    accept_scan(&stats)?;
    println!(
        "  found {} dirs, {} files, {} bytes in {:.0?} ({} vanished during the scan)",
        stats.dirs, stats.files, stats.bytes, stats.duration, stats.vanished,
    );
    let scan_dir = tools::resolve_scan_dir(&invocation.output)?;

    let cp = ScanCheckpoint {
        complete: true,
        scan_dir,
        walker_version,
        scan_url: params.scan_url.clone(),
        finished_utc: utc_now(),
        errors: 0,
        vanished: stats.vanished,
        attempt_dir: Some(attempt_dir),
        failure_log: None,
        walker_log: Some(invocation.log.clone()),
    };
    write_json_atomic(&checkpoint_path, &cp)?;
    Ok(cp)
}

/// Delete the raw walker scan output under this work dir's `scan/`.
/// Safe once the canonical shards and manifest are committed: the
/// scan is a pure intermediate that doubles the index footprint.
/// Best-effort — a failed purge is a warning, never an error.
pub fn purge_scan_output(wd: &WorkDir) {
    let root = wd.scan_root();
    match std::fs::remove_dir_all(&root) {
        Ok(()) => tracing::debug!(path = %root.display(), "purged scan output"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(
            error = %e,
            path = %root.display(),
            "scan purge failed (non-fatal)",
        ),
    }
}

/// Rewrite a completed scan into canonical shards under
/// `<wd>/canonical/` with `<wd>/rewrite.json` as the resumable
/// report. In-process (`mig_walker_rewrite` library), off-runtime.
pub async fn ensure_canonical(wd: &WorkDir, scan: &ScanCheckpoint) -> Result<()> {
    let rewrite = mig_walker_rewrite::Cli {
        input: scan.scan_dir.clone(),
        output: wd.canonical_dir(),
        // Not the configured source root: the scan was anchored there
        // already (see prepare_tools::REWRITE_SOURCE_ROOT).
        source_root: tools::REWRITE_SOURCE_ROOT.to_string(),
        walker_version: scan.walker_version.clone(),
        resume: true,
        report: Some(wd.rewrite_json()),
        verbose: false,
    };
    std::fs::create_dir_all(&rewrite.output)?;
    // Parquet decode/encode is CPU work — keep it off the runtime.
    tokio::task::spawn_blocking(move || mig_walker_rewrite::run_rewrite(&rewrite))
        .await
        .context("rewrite task panicked")?
        .context("canonical rewrite failed (finished shards are checkpointed; re-run to resume)")
}

/// Parse a [`tools::WalkerInvocation`] into the embedded walker's own
/// CLI struct. Going through the real clap surface (instead of
/// constructing `WalkConfig` by hand) keeps the argument semantics
/// identical to the standalone `nfs-walker` binary — and makes flag
/// drift a unit-test failure instead of a runtime surprise.
pub(crate) fn walker_cli(invocation: &tools::WalkerInvocation) -> Result<nfs_walker::CliArgs> {
    use clap::Parser;
    let mut argv: Vec<std::ffi::OsString> = vec!["nfs-walker".into()];
    argv.extend(invocation.args());
    // Progress goes to tracing + the JSONL log, not a terminal bar.
    argv.push("--quiet".into());
    nfs_walker::CliArgs::try_parse_from(argv)
        .map_err(|e| anyhow::anyhow!("embedded nfs-walker rejected the scan arguments: {e}"))
}

/// Run the compiled-in scanner on the blocking pool, relaying its
/// progress into tracing every [`SCAN_LOG_SECS`]. An incomplete scan
/// comes back as [`ScanFailure::Incomplete`] with the walker's stats
/// and failure sample intact.
async fn run_embedded_walker(
    invocation: &tools::WalkerInvocation,
) -> std::result::Result<WalkStats, ScanFailure> {
    let cli = walker_cli(invocation).map_err(ScanFailure::Other)?;
    let config = nfs_walker::WalkConfig::from_args(cli)
        .context("invalid embedded scan configuration")
        .map_err(ScanFailure::Other)?;
    let outcome = tokio::task::spawn_blocking(move || {
        let walker = nfs_walker::SimpleWalker::new(config);
        let last_logged = AtomicU64::new(0);
        walker.run_with_progress(move |p| {
            let elapsed = p.elapsed.as_secs();
            let prev = last_logged.load(Ordering::Relaxed);
            if elapsed >= prev + SCAN_LOG_SECS
                && last_logged
                    .compare_exchange(prev, elapsed, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
            {
                let secs = p.elapsed.as_secs_f64().max(0.001);
                tracing::info!(
                    dirs = p.dirs,
                    files = p.files,
                    bytes = p.bytes,
                    errors = p.errors,
                    entries_s = format!("{:.0}", (p.dirs + p.files) as f64 / secs),
                    "scanning",
                );
            }
        })
    })
    .await
    .context("walker task panicked")
    .map_err(ScanFailure::Other)?;
    let stats = match outcome {
        Ok(stats) => stats,
        Err(WalkerError::ScanIncomplete {
            stats,
            failures,
            failure_log,
        }) => {
            return Err(ScanFailure::Incomplete {
                stats,
                failures,
                failure_log,
            })
        }
        Err(e) => {
            return Err(ScanFailure::Other(
                anyhow::Error::new(e).context("scan failed"),
            ))
        }
    };
    if !stats.completed {
        return Err(ScanFailure::Other(anyhow::anyhow!(
            "scan was interrupted before completion; re-run to rescan"
        )));
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The successor to vamoose's runtime `check_walker_flags` probe:
    /// every flag `WalkerInvocation::args()` emits must parse into the
    /// embedded walker's own CLI. Flag drift between the pinned
    /// nfs-walker rev and prepare/sync fails here, at test time.
    #[test]
    fn walker_invocation_parses_into_the_embedded_cli() {
        let invocation = tools::WalkerInvocation {
            scan_url: "nfs://h/export/data".into(),
            output: "/w/scan/attempt-0001/walk.parquet".into(),
            workers: 8,
            exclude: Vec::new(),
            exclude_dirs: vec![".snapshot".into(), "tmp".into()],
            log: "/w/scan/attempt-0001/walker-progress.jsonl".into(),
        };
        let cli = walker_cli(&invocation).expect("embedded CLI accepts prepare's arguments");
        assert_eq!(cli.nfs_url.as_deref(), Some("nfs://h/export/data"));
        assert_eq!(cli.workers, 8);
        assert_eq!(
            cli.output,
            std::path::PathBuf::from("/w/scan/attempt-0001/walk.parquet")
        );
        // mongoose's excludes are directory-name globs, never path regexes.
        assert_eq!(cli.exclude_dirs, vec![".snapshot", "tmp"]);
        assert!(cli.exclude_patterns.is_empty());
        assert!(cli.quiet, "terminal progress bar suppressed");
    }

    /// The walker compiles the globs at config time, so a bad one
    /// fails there; mongoose refuses it earlier still, before an
    /// attempt directory exists.
    #[tokio::test]
    async fn invalid_exclude_glob_fails_before_an_attempt_dir_exists() {
        let dir = tempfile::tempdir().unwrap();
        let wd = WorkDir::new(dir.path());
        let err = ensure_scan(
            &wd,
            &ScanParams {
                scan_url: "nfs://h/export".into(),
                workers: 1,
                exclude: vec![".snapshot".into(), "[".into()],
            },
        )
        .await
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("--exclude") && msg.contains("\"[\""), "{msg}");
        assert!(!wd.scan_root().exists(), "no attempt directory was created");
        assert!(!wd.scan_json().exists());

        // And the walker itself agrees the pattern is invalid.
        let invocation = tools::WalkerInvocation {
            scan_url: "nfs://h/export".into(),
            output: dir.path().join("walk.parquet"),
            workers: 1,
            exclude: Vec::new(),
            exclude_dirs: vec!["[".into()],
            log: dir.path().join("log.jsonl"),
        };
        let cli = walker_cli(&invocation).unwrap();
        assert!(nfs_walker::WalkConfig::from_args(cli).is_err());
    }

    fn stats(completed: bool, errors: u64) -> WalkStats {
        WalkStats {
            dirs: 10,
            files: 100,
            bytes: 1000,
            errors,
            vanished: 1,
            duration: std::time::Duration::from_secs(1),
            completed,
        }
    }

    /// The acceptance rule: completed stats with any unreadable
    /// directory are rejected, so no complete checkpoint can follow.
    #[test]
    fn accept_scan_rejects_errors_and_interruption() {
        accept_scan(&stats(true, 0)).unwrap();
        let err = accept_scan(&stats(true, 3)).unwrap_err();
        assert!(
            format!("{err:#}").contains("3 unreadable directories"),
            "{err:#}"
        );
        let err = accept_scan(&stats(true, 1)).unwrap_err();
        assert!(
            format!("{err:#}").contains("1 unreadable directory"),
            "{err:#}"
        );
        let err = accept_scan(&stats(false, 0)).unwrap_err();
        assert!(format!("{err:#}").contains("interrupted"), "{err:#}");
    }

    fn checkpoint(complete: bool, errors: u64) -> ScanCheckpoint {
        ScanCheckpoint {
            complete,
            scan_dir: PathBuf::from("/w/scan/attempt-0001/walk.parquet/scans/x"),
            walker_version: "nfs-walker test".into(),
            scan_url: "nfs://h/export".into(),
            finished_utc: "2026-09-28T00:00:00Z".into(),
            errors,
            vanished: 0,
            attempt_dir: Some(PathBuf::from("/w/scan/attempt-0001")),
            failure_log: None,
            walker_log: None,
        }
    }

    #[test]
    fn only_complete_error_free_checkpoints_are_reused() {
        assert!(checkpoint_reusable(&checkpoint(true, 0)));
        assert!(!checkpoint_reusable(&checkpoint(false, 0)));
        assert!(!checkpoint_reusable(&checkpoint(false, 4)));
        assert!(
            !checkpoint_reusable(&checkpoint(true, 2)),
            "a checkpoint claiming completion with errors is not trusted"
        );
    }

    /// A `scan.json` written before completeness was recorded (no
    /// `errors` field) still parses and, being complete, is reused.
    #[test]
    fn older_checkpoints_without_error_fields_still_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scan.json");
        std::fs::write(
            &path,
            r#"{"complete":true,"scan_dir":"/w/scan/attempt-0001/walk.parquet/scans/x",
                "walker_version":"nfs-walker 0.1.0 (embedded)","scan_url":"nfs://h/e",
                "finished_utc":"2026-08-31T00:00:00Z"}"#,
        )
        .unwrap();
        let cp: ScanCheckpoint = read_json_opt(&path).unwrap().unwrap();
        assert_eq!(cp.errors, 0);
        assert!(cp.attempt_dir.is_none());
        assert!(checkpoint_reusable(&cp));
    }

    /// An incomplete attempt is recorded (not reusable) and the next
    /// attempt directory is a fresh one.
    #[test]
    fn incomplete_attempt_is_recorded_and_never_reused() {
        let dir = tempfile::tempdir().unwrap();
        let wd = WorkDir::new(dir.path());
        let attempt = wd.next_attempt_dir().unwrap();
        std::fs::create_dir_all(&attempt).unwrap();
        let mut cp = checkpoint(false, 2);
        cp.attempt_dir = Some(attempt.clone());
        write_json_atomic(&wd.scan_json(), &cp).unwrap();
        let back: ScanCheckpoint = read_json_opt(&wd.scan_json()).unwrap().unwrap();
        assert!(!checkpoint_reusable(&back));
        assert_eq!(back.errors, 2);
        assert_eq!(back.attempt_dir.as_deref(), Some(attempt.as_path()));
        assert!(attempt.exists(), "evidence kept");
        assert_ne!(
            wd.next_attempt_dir().unwrap(),
            attempt,
            "a rerun gets a fresh attempt"
        );
    }

    #[test]
    fn incomplete_message_lists_counts_sample_and_paths() {
        let failures: Vec<DirFailure> = (0..12)
            .map(|i| DirFailure {
                path: format!("/data/d{i}"),
                kind: nfs_walker::error::FailureKind::PermissionDenied,
                error: "Permission denied".into(),
                attempts: 1,
            })
            .collect();
        let msg = incomplete_message(
            "nfs://old/export",
            &stats(true, 12),
            &failures,
            Path::new("/w/scan/attempt-0002"),
            Some(Path::new(
                "/w/scan/attempt-0002/walk.parquet/scans/x/errors.jsonl",
            )),
            Path::new("/w/scan/attempt-0002/walker-progress.jsonl"),
        );
        assert!(msg.contains("12 directories could not be read"), "{msg}");
        assert!(msg.contains("1 vanished"), "{msg}");
        assert!(msg.contains("permission_denied  /data/d0"), "{msg}");
        assert!(msg.contains("... 2 more"), "{msg}");
        assert!(msg.contains("errors.jsonl"), "{msg}");
        assert!(msg.contains("walker-progress.jsonl"), "{msg}");
        assert!(msg.contains("nothing was checkpointed"), "{msg}");
    }

    #[test]
    fn embedded_walker_version_names_the_scanner() {
        let v = embedded_walker_version();
        assert!(v.starts_with("nfs-walker "), "{v}");
        assert!(v.ends_with("(embedded)"), "{v}");
    }
}
