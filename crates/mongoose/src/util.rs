//! Small durable-file helpers: atomic JSON writes, tolerant reads,
//! SHA256, timestamps. Local sibling of the vamoose prepare
//! checkpoint helpers (which are private to that crate).

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::Write;
use std::io::{ErrorKind, Result as IoResult};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AtomicWriteStep {
    Create,
    Write,
    Flush,
    FileSync,
    Rename,
    ParentSync,
}

fn temporary_sibling(path: &Path, id: u64) -> Result<PathBuf> {
    let name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("{} has no file name", path.display()))?
        .to_string_lossy();
    Ok(path.with_file_name(format!(".{name}.{}.{}.partial", std::process::id(), id)))
}

fn create_unique_temp_file(
    path: &Path,
    first_id: u64,
    mut before: impl FnMut(AtomicWriteStep) -> IoResult<()>,
) -> Result<(PathBuf, File)> {
    const MAX_TEMP_CANDIDATES: u64 = 128;
    before(AtomicWriteStep::Create)?;
    for offset in 0..MAX_TEMP_CANDIDATES {
        let id = first_id.wrapping_add(offset);
        let temporary = temporary_sibling(path, id)?;
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(file) => return Ok((temporary, file)),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("creating {}", temporary.display()))
            }
        }
    }
    anyhow::bail!(
        "could not create a unique temporary sibling for {} after {MAX_TEMP_CANDIDATES} candidates",
        path.display()
    )
}

fn write_bytes_atomic_with(
    path: &Path,
    bytes: &[u8],
    mut before: impl FnMut(AtomicWriteStep) -> std::io::Result<()>,
) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} has no parent directory", path.display()))?;
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let first_id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let mut temporary = None;
    let mut created = false;
    let write_result = (|| -> Result<()> {
        let (temporary_path, mut file) = create_unique_temp_file(path, first_id, &mut before)?;
        temporary = Some(temporary_path);
        created = true;
        let temporary = temporary.as_ref().expect("temp path assigned above");
        before(AtomicWriteStep::Write)?;
        file.write_all(bytes)
            .with_context(|| format!("writing {}", temporary.display()))?;
        before(AtomicWriteStep::Flush)?;
        file.flush()
            .with_context(|| format!("flushing {}", temporary.display()))?;
        before(AtomicWriteStep::FileSync)?;
        file.sync_all()
            .with_context(|| format!("syncing {}", temporary.display()))?;
        drop(file);
        before(AtomicWriteStep::Rename)?;
        std::fs::rename(temporary, path)
            .with_context(|| format!("renaming {} -> {}", temporary.display(), path.display()))?;
        before(AtomicWriteStep::ParentSync)?;
        std::fs::File::open(parent)
            .with_context(|| format!("opening parent {}", parent.display()))?
            .sync_all()
            .with_context(|| format!("syncing parent {}", parent.display()))?;
        Ok(())
    })();
    if write_result.is_err() && created {
        if let Some(temporary) = temporary {
            let _ = std::fs::remove_file(temporary);
        }
    }
    write_result
}

/// Write bytes through a unique same-directory temporary file, then
/// flush, fsync, rename, and fsync the containing directory.
pub fn write_bytes_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    write_bytes_atomic_with(path, bytes, |_| Ok(()))
}

/// Write `value` as pretty JSON through a unique `.partial` sibling and
/// an atomic rename, fsyncing file and directory so interrupted writes
/// never leave a torn checkpoint behind.
pub fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    write_bytes_atomic(path, &bytes)
}

/// `Ok(None)` when the file does not exist; parse failures are errors
/// (a corrupt checkpoint must be looked at, not silently redone).
pub fn read_json_opt<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    match std::fs::read(path) {
        Ok(bytes) => {
            Ok(Some(serde_json::from_slice(&bytes).with_context(|| {
                format!("parsing checkpoint {}", path.display())
            })?))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file =
        std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher)?;
    Ok(hex::encode(hasher.finalize()))
}

/// RFC 3339 UTC with second precision and a `Z` suffix.
pub fn utc_now() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Raise the soft `RLIMIT_NOFILE` toward 1M (capped at the hard
/// limit). The embedded scan holds per-worker NFS sockets plus
/// per-shard parquet writers, and the copy holds a socket pair per
/// libnfs context; the default soft limit of 1024 causes "Too many
/// open files" at scale. Same policy as the standalone nfs-walker.
pub fn raise_fd_limit() {
    const TARGET: libc::rlim_t = 1_048_576;
    // SAFETY: getrlimit/setrlimit are thread-safe and we pass valid
    // pointers to initialized rlimit structs.
    unsafe {
        let mut current = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut current) != 0 {
            tracing::warn!("could not read RLIMIT_NOFILE; large runs may hit fd limits");
            return;
        }
        let target = TARGET.min(current.rlim_max);
        if current.rlim_cur >= target {
            return;
        }
        let new = libc::rlimit {
            rlim_cur: target,
            rlim_max: current.rlim_max,
        };
        if libc::setrlimit(libc::RLIMIT_NOFILE, &new) != 0 {
            tracing::warn!(
                soft = current.rlim_cur,
                hard = current.rlim_max,
                requested = target,
                "could not raise RLIMIT_NOFILE; large runs may hit 'Too many open files'",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_then_read_round_trips_and_leaves_no_partial() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("cp.json");
        write_json_atomic(&path, &serde_json::json!({"a": 1})).unwrap();
        let back: serde_json::Value = read_json_opt(&path).unwrap().unwrap();
        assert_eq!(back["a"], 1);
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );
        let absent: Option<serde_json::Value> =
            read_json_opt(&dir.path().join("missing.json")).unwrap();
        assert!(absent.is_none());
    }

    #[test]
    fn atomic_bytes_replace_existing_file_and_use_unique_temps() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("results.jsonl");
        write_bytes_atomic(&path, b"old\n").unwrap();
        write_bytes_atomic(&path, b"new\n").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new\n");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn atomic_bytes_follow_durable_publication_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        let mut steps = Vec::new();
        write_bytes_atomic_with(&path, b"record\n", |step| {
            steps.push(step);
            Ok(())
        })
        .unwrap();
        assert_eq!(
            steps,
            [
                AtomicWriteStep::Create,
                AtomicWriteStep::Write,
                AtomicWriteStep::Flush,
                AtomicWriteStep::FileSync,
                AtomicWriteStep::Rename,
                AtomicWriteStep::ParentSync,
            ]
        );
    }

    #[test]
    fn stale_temp_candidate_is_skipped_without_modifying_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("results.jsonl");
        let first_id = 42_424;
        let stale = temporary_sibling(&path, first_id).unwrap();
        std::fs::write(&stale, b"stale crash artifact").unwrap();

        let (created_path, mut file) =
            create_unique_temp_file(&path, first_id, |_| Ok(())).unwrap();
        assert_ne!(created_path, stale);
        assert_eq!(std::fs::read(&stale).unwrap(), b"stale crash artifact");
        file.write_all(b"fresh result").unwrap();
        drop(file);
        assert_eq!(std::fs::read(&created_path).unwrap(), b"fresh result");
        std::fs::remove_file(created_path).unwrap();
    }

    #[test]
    fn injected_atomic_write_failures_preserve_old_target_and_remove_temp() {
        for failed_step in [
            AtomicWriteStep::Create,
            AtomicWriteStep::Write,
            AtomicWriteStep::Flush,
            AtomicWriteStep::FileSync,
            AtomicWriteStep::Rename,
            AtomicWriteStep::ParentSync,
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("record.jsonl");
            std::fs::write(&path, b"old\n").unwrap();
            let err = write_bytes_atomic_with(&path, b"new\n", |step| {
                if step == failed_step {
                    Err(std::io::Error::other("injected failure"))
                } else {
                    Ok(())
                }
            });
            assert!(err.is_err(), "{failed_step:?} should fail");
            if failed_step == AtomicWriteStep::ParentSync {
                // Rename happened before the directory sync failed.
                assert_eq!(std::fs::read(&path).unwrap(), b"new\n");
            } else {
                assert_eq!(std::fs::read(&path).unwrap(), b"old\n");
            }
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        }
    }

    #[test]
    fn corrupt_checkpoint_is_an_error_not_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cp.json");
        std::fs::write(&path, b"{not json").unwrap();
        let err = read_json_opt::<serde_json::Value>(&path).unwrap_err();
        assert!(format!("{err:#}").contains("parsing checkpoint"));
    }

    #[test]
    fn sha256_file_matches_known_digest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        std::fs::write(&path, b"hello").unwrap();
        assert_eq!(
            sha256_file(&path).unwrap(),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }
}
