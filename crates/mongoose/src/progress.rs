//! Local copy progress (`progress.json`) and the per-shard JSONL
//! result files (`failures/`, `downgrades/`).

use crate::util::{read_json_opt, utc_now, write_json_atomic};
use crate::workdir::WorkDir;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Operator-visible copy state, rewritten atomically after every
/// shard. Doubles as the shard-granularity resume checkpoint: a
/// re-run skips every shard listed in `completed_shards`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CopyProgress {
    pub run_id: String,
    pub started_utc: String,
    pub updated_utc: String,
    pub shards_total: u64,
    /// Manifest `path`s of shards fully processed (all rows dispatched
    /// and recorded; failures drained to JSONL).
    pub completed_shards: Vec<String>,
    pub files_ok: u64,
    pub files_failed: u64,
    /// Committed copies whose source changed mid-copy (also counted in
    /// `files_ok`; each has a TornCopy downgrade record).
    pub files_torn: u64,
    /// Fifos, sockets, and device nodes that were recognized and NOT
    /// copied. Processed rows: they are in no other counter, and each
    /// has a `SPECIAL_NOT_COPIED` record under `downgrades/`.
    /// Defaults to zero for a progress file written before the
    /// counter existed.
    #[serde(default)]
    pub files_special_not_copied: u64,
    pub bytes_moved: u64,
    pub throughput_mb_s_1m: f64,
    /// True once every shard completed and the root mtime restore ran.
    pub done: bool,
}

impl CopyProgress {
    pub fn fresh(run_id: &str, shards_total: u64) -> Self {
        Self {
            run_id: run_id.to_string(),
            started_utc: utc_now(),
            updated_utc: utc_now(),
            shards_total,
            completed_shards: Vec::new(),
            files_ok: 0,
            files_failed: 0,
            files_torn: 0,
            files_special_not_copied: 0,
            bytes_moved: 0,
            throughput_mb_s_1m: 0.0,
            done: false,
        }
    }

    /// Fold one fully processed shard into the checkpoint: its counters,
    /// and its path in `completed_shards` so a re-run skips it. A shard
    /// whose only outcome for some rows is "special node not copied" is
    /// complete; those rows are not retried.
    pub fn record_shard(
        &mut self,
        shard_path: &str,
        outcome: &migration_worker::shard_processor::ProcessOutcome,
        throughput_mb_s_1m: f64,
    ) {
        self.files_ok += outcome.files_ok;
        self.files_failed += outcome.files_failed;
        self.files_torn += outcome.files_torn;
        self.files_special_not_copied += outcome.files_special_not_copied;
        self.bytes_moved += outcome.bytes_moved;
        self.throughput_mb_s_1m = throughput_mb_s_1m;
        self.completed_shards.push(shard_path.to_string());
    }

    /// Resume an existing progress file when it belongs to this run;
    /// start fresh otherwise (a different run id in the same work dir
    /// is refused upstream by the run-spec check).
    pub fn load_or_fresh(workdir: &WorkDir, run_id: &str, shards_total: u64) -> Result<Self> {
        match read_json_opt::<CopyProgress>(&workdir.progress_json())? {
            Some(p) if p.run_id == run_id => Ok(Self {
                shards_total,
                updated_utc: utc_now(),
                ..p
            }),
            _ => Ok(Self::fresh(run_id, shards_total)),
        }
    }

    pub fn is_completed(&self, shard_path: &str) -> bool {
        self.completed_shards.iter().any(|s| s == shard_path)
    }

    pub fn write(&mut self, workdir: &WorkDir) -> Result<()> {
        self.updated_utc = utc_now();
        write_json_atomic(&workdir.progress_json(), self)
    }
}

/// Write one shard's drained sink body to `<dir>/<stem>.jsonl`.
/// An empty body removes any stale file from a previous attempt of
/// the same shard, so results always reflect the last completed pass.
/// Returns the path written, or `None` when there was nothing.
pub fn write_shard_jsonl(dir: &Path, stem: &str, body: &[u8]) -> Result<Option<PathBuf>> {
    write_shard_jsonl_with(dir, stem, body, |_| Ok(()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResultFileStep {
    Remove,
    ParentSync,
}

fn write_shard_jsonl_with(
    dir: &Path,
    stem: &str,
    body: &[u8],
    mut before: impl FnMut(ResultFileStep) -> std::io::Result<()>,
) -> Result<Option<PathBuf>> {
    let path = dir.join(format!("{stem}.jsonl"));
    if body.is_empty() {
        before(ResultFileStep::Remove)?;
        match std::fs::remove_file(&path) {
            Ok(()) => {
                before(ResultFileStep::ParentSync)?;
                std::fs::File::open(dir)
                    .with_context(|| format!("opening result directory {}", dir.display()))?
                    .sync_all()
                    .with_context(|| format!("syncing result directory {}", dir.display()))?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("removing stale {}", path.display())),
        }
        return Ok(None);
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    crate::util::write_bytes_atomic(&path, body)?;
    Ok(Some(path))
}

/// Persist both result streams before invoking the closure that commits
/// shard completion to progress. A persistence error never runs `commit`.
pub fn persist_shard_results_then<T>(
    failures_dir: &Path,
    downgrades_dir: &Path,
    stem: &str,
    failures: &[u8],
    downgrades: &[u8],
    mut write: impl FnMut(&Path, &str, &[u8]) -> Result<Option<PathBuf>>,
    commit: impl FnOnce() -> Result<T>,
) -> Result<(Option<PathBuf>, Option<PathBuf>, T)> {
    let failure_path = write(failures_dir, stem, failures)?;
    let downgrade_path = write(downgrades_dir, stem, downgrades)?;
    let committed = commit()?;
    Ok((failure_path, downgrade_path, committed))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `progress.json` written before special nodes were counted has
    /// no such field; it must still load, with the counter at zero.
    #[test]
    fn progress_written_before_the_special_counter_reads_as_zero() {
        let old = r#"{
            "run_id": "run-old",
            "started_utc": "2026-09-01T00:00:00Z",
            "updated_utc": "2026-09-01T00:10:00Z",
            "shards_total": 2,
            "completed_shards": ["canonical/part-0000.parquet"],
            "files_ok": 7,
            "files_failed": 1,
            "files_torn": 0,
            "bytes_moved": 4096,
            "throughput_mb_s_1m": 1.5,
            "done": false
        }"#;
        let p: CopyProgress = serde_json::from_str(old).unwrap();
        assert_eq!(p.files_special_not_copied, 0);
        assert_eq!(p.files_ok, 7);
        assert!(p.is_completed("canonical/part-0000.parquet"));
    }

    /// A shard with special nodes commits like any other, and its
    /// omissions land in their own counter, not in `files_ok`.
    #[test]
    fn record_shard_commits_the_shard_and_keeps_omissions_separate() {
        use migration_worker::shard_processor::ProcessOutcome;
        let dir = tempfile::tempdir().unwrap();
        let wd = WorkDir::new(dir.path());
        let mut p = CopyProgress::fresh("run-s", 2);
        let outcome = ProcessOutcome {
            rows_total: 6,
            files_ok: 3,
            files_failed: 1,
            files_special_not_copied: 2,
            files_torn: 1,
            bytes_moved: 900,
            ..ProcessOutcome::default()
        };

        p.record_shard("canonical/part-0000.parquet", &outcome, 2.5);
        p.record_shard("canonical/part-0001.parquet", &outcome, 3.5);
        p.write(&wd).unwrap();

        let back = CopyProgress::load_or_fresh(&wd, "run-s", 2).unwrap();
        assert_eq!(back.files_ok, 6);
        assert_eq!(back.files_failed, 2);
        assert_eq!(back.files_special_not_copied, 4);
        assert_eq!(back.files_torn, 2);
        assert_eq!(back.bytes_moved, 1800);
        assert_eq!(back.throughput_mb_s_1m, 3.5);
        assert!(back.is_completed("canonical/part-0000.parquet"));
        assert!(back.is_completed("canonical/part-0001.parquet"));
        let json = std::fs::read_to_string(wd.progress_json()).unwrap();
        assert!(
            json.contains(r#""files_special_not_copied": 4"#)
                || json.contains(r#""files_special_not_copied":4"#),
            "{json}"
        );
    }

    #[test]
    fn progress_resumes_only_its_own_run() {
        let dir = tempfile::tempdir().unwrap();
        let wd = WorkDir::new(dir.path());
        let mut p = CopyProgress::fresh("run-a", 3);
        p.completed_shards
            .push("canonical/part-0000.parquet".into());
        p.files_ok = 10;
        p.write(&wd).unwrap();

        let resumed = CopyProgress::load_or_fresh(&wd, "run-a", 3).unwrap();
        assert!(resumed.is_completed("canonical/part-0000.parquet"));
        assert!(!resumed.is_completed("canonical/part-0001.parquet"));
        assert_eq!(resumed.files_ok, 10);

        let other = CopyProgress::load_or_fresh(&wd, "run-b", 3).unwrap();
        assert!(
            other.completed_shards.is_empty(),
            "different run starts fresh"
        );
    }

    #[test]
    fn shard_jsonl_written_only_when_nonempty_and_stale_files_removed() {
        let dir = tempfile::tempdir().unwrap();
        let sink_dir = dir.path().join("failures");

        // Nothing to write, nothing created.
        assert!(write_shard_jsonl(&sink_dir, "part-0000", b"")
            .unwrap()
            .is_none());
        assert!(!sink_dir.exists());

        let body = b"{\"row_id\":1}\n";
        let path = write_shard_jsonl(&sink_dir, "part-0000", body)
            .unwrap()
            .expect("written");
        assert_eq!(std::fs::read(&path).unwrap(), body);

        // A clean re-run of the shard removes the stale record file.
        assert!(write_shard_jsonl(&sink_dir, "part-0000", b"")
            .unwrap()
            .is_none());
        assert!(!path.exists());
    }

    #[test]
    fn stale_result_removal_and_parent_sync() {
        let dir = tempfile::tempdir().unwrap();
        let sink_dir = dir.path().join("failures");
        let path = sink_dir.join("part-0000.jsonl");
        std::fs::create_dir_all(&sink_dir).unwrap();
        std::fs::write(&path, b"old\n").unwrap();
        write_shard_jsonl(&sink_dir, "part-0000", b"").unwrap();
        assert!(!path.exists());
        assert_eq!(std::fs::read_dir(&sink_dir).unwrap().count(), 0);
    }

    #[test]
    fn stale_result_removal_error_keeps_old_file() {
        let dir = tempfile::tempdir().unwrap();
        let sink_dir = dir.path().join("failures");
        let path = sink_dir.join("part-0000.jsonl");
        std::fs::create_dir_all(&sink_dir).unwrap();
        std::fs::write(&path, b"old\n").unwrap();
        let result = write_shard_jsonl_with(&sink_dir, "part-0000", b"", |_| {
            Err(std::io::Error::other("injected removal failure"))
        });
        assert!(result.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"old\n");
    }

    #[test]
    fn stale_result_sync_failure_does_not_commit_progress() {
        let dir = tempfile::tempdir().unwrap();
        let wd = WorkDir::new(dir.path());
        std::fs::create_dir_all(wd.failures_dir()).unwrap();
        let result_path = wd.failures_dir().join("part-0000.jsonl");
        std::fs::write(&result_path, b"old\n").unwrap();
        let mut progress = CopyProgress::fresh("run-a", 1);
        progress.write(&wd).unwrap();
        let result = persist_shard_results_then(
            &wd.failures_dir(),
            &wd.downgrades_dir(),
            "part-0000",
            b"",
            b"",
            |target_dir, stem, body| {
                if target_dir == wd.failures_dir() {
                    write_shard_jsonl_with(target_dir, stem, body, |step| {
                        if step == ResultFileStep::ParentSync {
                            Err(std::io::Error::other("injected directory sync failure"))
                        } else {
                            Ok(())
                        }
                    })
                } else {
                    write_shard_jsonl(target_dir, stem, body)
                }
            },
            || {
                progress
                    .completed_shards
                    .push("canonical/part-0000.parquet".into());
                progress.write(&wd)
            },
        );
        assert!(result.is_err());
        assert!(!progress.is_completed("canonical/part-0000.parquet"));
        let saved = CopyProgress::load_or_fresh(&wd, "run-a", 1).unwrap();
        assert!(!saved.is_completed("canonical/part-0000.parquet"));
    }

    #[test]
    fn durable_results_are_in_place_before_progress_commit() {
        let dir = tempfile::tempdir().unwrap();
        let wd = WorkDir::new(dir.path());
        std::fs::create_dir_all(wd.downgrades_dir()).unwrap();
        let stale_downgrade = wd.downgrades_dir().join("part-0000.jsonl");
        std::fs::write(&stale_downgrade, b"stale\n").unwrap();
        let mut progress = CopyProgress::fresh("run-a", 1);
        let result = persist_shard_results_then(
            &wd.failures_dir(),
            &wd.downgrades_dir(),
            "part-0000",
            b"failure\n",
            b"",
            write_shard_jsonl,
            || {
                assert_eq!(
                    std::fs::read(wd.failures_dir().join("part-0000.jsonl")).unwrap(),
                    b"failure\n"
                );
                assert!(!stale_downgrade.exists());
                progress
                    .completed_shards
                    .push("canonical/part-0000.parquet".into());
                progress.write(&wd)
            },
        )
        .unwrap();
        assert_eq!(result.1, None);
        let saved = CopyProgress::load_or_fresh(&wd, "run-a", 1).unwrap();
        assert!(saved.is_completed("canonical/part-0000.parquet"));
    }

    #[test]
    fn result_persistence_failure_does_not_commit_shard_progress() {
        let dir = tempfile::tempdir().unwrap();
        let wd = WorkDir::new(dir.path());
        let mut progress = CopyProgress::fresh("run-a", 1);
        progress.write(&wd).unwrap();
        let result = persist_shard_results_then(
            &wd.failures_dir(),
            &wd.downgrades_dir(),
            "part-0000",
            b"failure\n",
            b"downgrade\n",
            |target_dir, stem, body| {
                if target_dir == wd.downgrades_dir() {
                    anyhow::bail!("injected rename/sync failure")
                }
                write_shard_jsonl(target_dir, stem, body)
            },
            || {
                progress
                    .completed_shards
                    .push("canonical/part-0000.parquet".into());
                progress.write(&wd)
            },
        );
        assert!(result.is_err());
        assert!(!progress.is_completed("canonical/part-0000.parquet"));
        let saved = CopyProgress::load_or_fresh(&wd, "run-a", 1).unwrap();
        assert!(!saved.is_completed("canonical/part-0000.parquet"));
    }
}
