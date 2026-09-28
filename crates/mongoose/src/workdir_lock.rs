//! Advisory process lock for a mongoose work directory.

use anyhow::{Context, Result};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

/// An exclusive, nonblocking lock held for as long as this value lives.
///
/// The file is intentionally persistent: its text is diagnostic metadata,
/// while `flock(2)` on the open file description is the liveness mechanism.
#[derive(Debug)]
pub struct WorkDirLock {
    _file: File,
    path: PathBuf,
}

impl WorkDirLock {
    /// Acquire the exclusive lock for `root`. The root may not exist yet; it is
    /// created before opening the lock so simultaneous first invocations
    /// converge on the same lock file before writing any work state.
    pub fn acquire(root: impl AsRef<Path>, command: &str) -> Result<Self> {
        let root = root.as_ref();
        fs::create_dir_all(root)
            .with_context(|| format!("creating work directory {}", root.display()))?;
        // Resolve relative paths and symlink aliases so they use one lock file.
        let root = fs::canonicalize(root)
            .with_context(|| format!("resolving work directory {}", root.display()))?;
        let path = root.join(".mongoose.lock");
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("opening work-directory lock {}", path.display()))?;

        // flock locks are associated with an open file description, so a
        // second independently opened handle conflicts even in this process.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::WouldBlock {
                let mut owner = String::new();
                let _ = file.read_to_string(&mut owner);
                let owner = if owner.trim().is_empty() {
                    "owner metadata is not available yet".to_owned()
                } else {
                    owner.trim().to_owned()
                };
                anyhow::bail!(
                    "work directory {} is already in use ({owner}); wait for that command to finish, then retry",
                    root.display()
                );
            }
            return Err(error)
                .with_context(|| format!("locking work directory {}", root.display()));
        }

        let hostname = local_hostname();
        let metadata = format!(
            "pid={}\nhostname={}\ncommand={}\nstarted={}\n",
            std::process::id(),
            hostname,
            command,
            chrono::Utc::now().to_rfc3339()
        );
        file.set_len(0)
            .with_context(|| format!("clearing lock metadata {}", path.display()))?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(metadata.as_bytes())
            .with_context(|| format!("writing lock metadata {}", path.display()))?;
        file.sync_all()
            .with_context(|| format!("syncing lock metadata {}", path.display()))?;
        Ok(Self { _file: file, path })
    }

    /// Path containing diagnostic owner metadata (not a liveness indicator).
    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn local_hostname() -> String {
    fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|hostname| hostname.trim().to_owned())
        .ok()
        .filter(|hostname| !hostname.is_empty())
        .unwrap_or_else(|| "mongoose".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use tempfile::tempdir;

    #[test]
    fn independently_opened_handles_conflict_and_report_owner() {
        let temp = tempdir().unwrap();
        let first = WorkDirLock::acquire(temp.path(), "mongoose copy").unwrap();
        let error = WorkDirLock::acquire(temp.path(), "mongoose sync").unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("already in use"), "{message}");
        assert!(
            message.contains(&format!("pid={}", std::process::id())),
            "{message}"
        );
        assert!(message.contains("command=mongoose copy"), "{message}");
        assert!(
            message.contains(&format!("hostname={}\n", local_hostname())),
            "{message}"
        );
        assert!(message.contains("started="), "{message}");
        drop(first);
        WorkDirLock::acquire(temp.path(), "mongoose sync").unwrap();
    }

    #[test]
    fn abandoned_metadata_does_not_block_acquisition() {
        let temp = tempdir().unwrap();
        fs::write(
            temp.path().join(".mongoose.lock"),
            "pid=999999\ncommand=dead\n",
        )
        .unwrap();
        let lock = WorkDirLock::acquire(temp.path(), "mongoose copy").unwrap();
        assert!(fs::read_to_string(lock.path())
            .unwrap()
            .contains("command=mongoose copy"));
    }

    #[test]
    fn first_creation_race_has_one_winner() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("new-work-dir");
        let start = Arc::new(Barrier::new(3));
        let done = Arc::new(Barrier::new(3));
        let handles: Vec<_> = (0..2)
            .map(|n| {
                let root = root.clone();
                let start = start.clone();
                let done = done.clone();
                std::thread::spawn(move || {
                    start.wait();
                    // Keep a successful guard alive until both attempts have
                    // finished, so a late second acquisition cannot win after
                    // the first thread drops its lock.
                    let result = WorkDirLock::acquire(root, &format!("command-{n}"));
                    done.wait();
                    result
                })
            })
            .collect();
        start.wait();
        done.wait();
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
        assert_eq!(results.iter().filter(|r| r.is_err()).count(), 1);
    }
}
