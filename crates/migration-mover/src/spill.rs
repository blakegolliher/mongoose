//! A sink's records written to a JSONL file as they arrive, for
//! [`crate::FailureSink::spill_to`] and
//! [`crate::DowngradeSink::spill_to`]. A shard can produce a record per
//! row (an owner that cannot be set, say), and holding them until the
//! shard ends cost memory for every one.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

pub(crate) struct Spill {
    path: PathBuf,
    out: BufWriter<File>,
    records: u64,
    /// The first write error. Later records are dropped, and
    /// [`Spill::finish`] reports it, so a shard never completes with
    /// records missing.
    error: Option<std::io::Error>,
}

/// A finished spill file, flushed and synced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spilled {
    pub path: PathBuf,
    /// Records in the file. Zero means it is empty.
    pub records: u64,
}

impl Spill {
    /// Create or truncate `path`.
    pub(crate) fn create(path: &Path) -> std::io::Result<Self> {
        Ok(Self {
            path: path.to_path_buf(),
            out: BufWriter::new(File::create(path)?),
            records: 0,
            error: None,
        })
    }

    /// Append one record, serialized by `write`, and its newline.
    pub(crate) fn push(
        &mut self,
        write: impl FnOnce(&mut BufWriter<File>) -> serde_json::Result<()>,
    ) {
        if self.error.is_some() {
            return;
        }
        match write(&mut self.out)
            .map_err(std::io::Error::from)
            .and_then(|()| self.out.write_all(b"\n"))
        {
            Ok(()) => self.records += 1,
            Err(e) => self.error = Some(e),
        }
    }

    pub(crate) fn records(&self) -> u64 {
        self.records
    }

    /// Flush and fsync the file.
    pub(crate) fn finish(self) -> std::io::Result<Spilled> {
        if let Some(e) = self.error {
            return Err(e);
        }
        let file = self.out.into_inner().map_err(|e| e.into_error())?;
        file.sync_all()?;
        Ok(Spilled {
            path: self.path,
            records: self.records,
        })
    }
}
