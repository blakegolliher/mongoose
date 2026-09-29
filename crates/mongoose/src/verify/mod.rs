//! Cutover verification: prove that the destination matches the
//! source before `mongoose sync --cutover` says so.
//!
//! The classifier's zero-drift gate only proves the *source* has not
//! changed since the last sync. It never looks at the destination, so
//! a destination file that was deleted, truncated, rewritten, or given
//! the wrong metadata after its copy would pass. This module is the
//! independent check: it compares the destination against the source
//! and reads every file back from both servers.
//!
//! ## What "the trees match" means (the verification contract)
//!
//! For every path in the source index (the same scan, same excludes):
//!
//! | entry      | verified                                                    |
//! |------------|-------------------------------------------------------------|
//! | file       | present, type, size, mode bits, owner, mtime (µs), SHA-256   |
//! | directory  | present, type, mode bits, owner                             |
//! | symlink    | present, type, target bytes (READLINK on both sides)        |
//! | special    | reported as `special_not_copied` — mongoose does not copy   |
//! |            | fifos, sockets, or device nodes, so they can never match     |
//!
//! plus: every destination path must exist in the source (an extra
//! fails, including entries the no-delete policy left behind and
//! stale `.partial` files from an interrupted copy).
//!
//! Deliberately outside the contract, because the copy engine does
//! not guarantee them: directory mtimes (any later commit into a
//! directory bumps it), atimes, hardlink topology (links that span
//! shards copy as separate files), symlink mode/owner/mtime (NFSv3
//! has no lchmod; lutimes is best-effort), and the migration root's
//! own attributes (the walker emits no row for it). These are listed
//! in `docs/REFERENCE.md` so the statement a clean exit makes is
//! exact.
//!
//! Mode, owner, and mtime are compared only when the job's
//! `MigrationOptions` asked for them to be preserved (all three are on
//! by default); the report records which were in force.
//!
//! ## Mechanics
//!
//! 1. [`namespace`]: hash-partitioned join of the source and
//!    destination canonical indexes by path. Emits metadata mismatch
//!    records and a work list of files and symlinks whose content must
//!    be read. Cheap (metadata only, local disk).
//! 2. [`content`]: for every work-list entry, hash the file on both
//!    servers (or readlink both sides) and compare. Expensive — it
//!    reads the whole tree twice — and resumable at a durable
//!    frontier, so an interrupted cutover picks up where it stopped.
//! 3. The report: `verify.json` with counts, the contract in force, a
//!    bounded sample of mismatches, and the path of the full
//!    `verify/mismatches.jsonl`.
//!
//! The content check is the only mode. There is no sampled or
//! metadata-only variant: a weaker check would need a different name
//! and a different result, and `--cutover` is the strong one.

pub mod content;
pub mod namespace;

use crate::util::{utc_now, write_json_atomic};
use crate::workdir::WorkDir;
use anyhow::{Context, Result};
use base64::Engine;
use migration_core::records::{Endpoint, MigrationOptions};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::BufRead;
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Bump on breaking changes to `verify.json` / `mismatches.jsonl`.
pub const VERIFY_FORMAT_VERSION: u32 = 1;

/// How many mismatch records the report embeds; the JSONL file has
/// every one.
pub const SAMPLE_LIMIT: usize = 100;

/// Pass-dir-relative path of the full mismatch list.
pub const MISMATCHES_FILE: &str = "verify/mismatches.jsonl";

/// Name of the one content-verification mode this build implements.
/// A future sampled or metadata-only check must use a different name
/// here and a different status, never this one.
pub const MODE_FULL: &str = "full";

/// One way the destination differs from the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MismatchKind {
    /// Source path absent from the destination.
    Missing,
    /// Destination path absent from the source.
    Extra,
    /// Fifo, socket, or device node on the source. mongoose does not
    /// copy these; a cutover can only pass once they are recreated on
    /// the destination or removed from the source.
    SpecialNotCopied,
    FileType,
    Size,
    Mode,
    Owner,
    Mtime,
    SymlinkTarget,
    /// SHA-256 of the bytes differs (or the byte counts do).
    Content,
    /// One side could not be read; nothing can be proved about it.
    ReadError,
}

impl MismatchKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Extra => "extra",
            Self::SpecialNotCopied => "special_not_copied",
            Self::FileType => "file_type",
            Self::Size => "size",
            Self::Mode => "mode",
            Self::Owner => "owner",
            Self::Mtime => "mtime",
            Self::SymlinkTarget => "symlink_target",
            Self::Content => "content",
            Self::ReadError => "read_error",
        }
    }
}

/// One mismatch record — a line of `verify/mismatches.jsonl`.
/// `path_b64` is the lossless path (SCHEMA_CONTRACT.md "Path
/// encoding"); `path_lossy` is a display convenience only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mismatch {
    pub path_b64: String,
    pub path_lossy: String,
    pub kind: MismatchKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl Mismatch {
    pub fn new(path: &[u8], kind: MismatchKind) -> Self {
        Self {
            path_b64: base64::engine::general_purpose::STANDARD.encode(path),
            path_lossy: String::from_utf8_lossy(path).into_owned(),
            kind,
            expected: None,
            actual: None,
            detail: None,
        }
    }

    pub fn expected(mut self, v: impl Into<String>) -> Self {
        self.expected = Some(v.into());
        self
    }

    pub fn actual(mut self, v: impl Into<String>) -> Self {
        self.actual = Some(v.into());
        self
    }

    pub fn detail(mut self, v: impl Into<String>) -> Self {
        self.detail = Some(v.into());
        self
    }

    pub fn path(&self) -> Result<Vec<u8>> {
        base64::engine::general_purpose::STANDARD
            .decode(&self.path_b64)
            .context("decoding path_b64 from a mismatch record")
    }
}

/// Which metadata the contract compares, derived from the job's copy
/// options: an attribute the copy never tried to preserve is not
/// verified. Content, type, size, and symlink targets are always
/// compared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Contract {
    pub mode: bool,
    pub owner: bool,
    pub mtime: bool,
}

impl Contract {
    pub fn from_options(o: &MigrationOptions) -> Self {
        Self {
            mode: o.preserve_mode,
            owner: o.preserve_owner,
            mtime: o.preserve_times,
        }
    }

    /// Everything compared: the default job options.
    pub fn strict() -> Self {
        Self {
            mode: true,
            owner: true,
            mtime: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerifyStatus {
    /// Every check ran and found nothing: the trees match per the
    /// contract.
    Pass,
    /// At least one mismatch record exists.
    Fail,
    /// Stopped by SIGINT/SIGTERM before every entry was read; nothing
    /// is proved. Re-run `mongoose sync --cutover` to resume.
    Interrupted,
}

/// `verify.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyReport {
    pub format_version: u32,
    pub run_id: String,
    pub pass: u32,
    pub status: VerifyStatus,
    /// Always [`MODE_FULL`] in this build.
    pub mode: String,
    pub source_url: String,
    pub dest_url: String,
    pub started_utc: String,
    pub finished_utc: String,
    pub contract: Contract,
    pub namespace: namespace::NamespaceCounts,
    pub content: content::ContentCounts,
    pub mismatches_total: u64,
    pub mismatches_by_kind: BTreeMap<MismatchKind, u64>,
    /// The first [`SAMPLE_LIMIT`] records of `mismatches_file`.
    pub mismatch_samples: Vec<Mismatch>,
    /// Pass-dir-relative path of the full list.
    pub mismatches_file: String,
}

impl VerifyReport {
    /// `missing 3, extra 1, content 2` — for the operator line.
    pub fn breakdown(&self) -> String {
        self.mismatches_by_kind
            .iter()
            .map(|(k, n)| format!("{} {n}", k.as_str()))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Everything one verification needs, resolved by the caller.
pub struct VerifyParams {
    pub run_id: String,
    pub pass: u32,
    pub source: Endpoint,
    pub dest: Endpoint,
    pub options: MigrationOptions,
    /// Canonical shards of the source index (this pass's rescan).
    pub source_shards: Vec<PathBuf>,
    /// Canonical shards of the destination index.
    pub dest_shards: Vec<PathBuf>,
    /// Files/symlinks read concurrently in the content phase.
    pub concurrency: usize,
    /// Hash-partition fan-out for the namespace join.
    pub buckets: usize,
}

/// Run (or resume) the verification for one pass and write
/// `verify.json`. Never advances anything: the caller decides what a
/// [`VerifyStatus`] means for the baseline.
pub async fn run(
    pass_wd: &WorkDir,
    params: &VerifyParams,
    checker: Arc<dyn content::ContentChecker>,
    stop: CancellationToken,
) -> Result<VerifyReport> {
    let started_utc = utc_now();
    let contract = Contract::from_options(&params.options);

    // ---- 1. namespace + metadata ------------------------------------
    let ns_out = namespace::ensure(
        pass_wd,
        &params.source_shards,
        &params.dest_shards,
        contract,
        params.buckets,
    )?;
    let ns = ns_out.counts;
    println!(
        "  namespace: {} source entries, {} destination entries, {} metadata mismatches",
        ns.source_entries, ns.dest_entries, ns.mismatches,
    );
    println!(
        "  content:   {} files ({} bytes) and {} symlinks to read back from both servers",
        ns.files_to_read, ns.bytes_to_read, ns.symlinks_to_read,
    );

    // ---- 2. content -------------------------------------------------
    let outcome = content::run(
        pass_wd,
        checker,
        params.concurrency,
        ns_out.mismatch_file_len,
        stop,
    )
    .await?;

    // ---- 3. report --------------------------------------------------
    let (mismatches_total, by_kind, samples) = tally(pass_wd)?;
    let status = if outcome.interrupted {
        VerifyStatus::Interrupted
    } else if mismatches_total > 0 {
        VerifyStatus::Fail
    } else {
        VerifyStatus::Pass
    };
    let report = VerifyReport {
        format_version: VERIFY_FORMAT_VERSION,
        run_id: params.run_id.clone(),
        pass: params.pass,
        status,
        mode: MODE_FULL.to_string(),
        source_url: params.source.url.clone(),
        dest_url: params.dest.url.clone(),
        started_utc,
        finished_utc: utc_now(),
        contract,
        namespace: ns,
        content: outcome.counts,
        mismatches_total,
        mismatches_by_kind: by_kind,
        mismatch_samples: samples,
        mismatches_file: MISMATCHES_FILE.to_string(),
    };
    write_json_atomic(&pass_wd.verify_json(), &report)?;
    Ok(report)
}

/// Read `verify/mismatches.jsonl` once: total, per-kind counts, and
/// the first [`SAMPLE_LIMIT`] records.
fn tally(pass_wd: &WorkDir) -> Result<(u64, BTreeMap<MismatchKind, u64>, Vec<Mismatch>)> {
    let path = pass_wd.root().join(MISMATCHES_FILE);
    let mut total = 0u64;
    let mut by_kind = BTreeMap::new();
    let mut samples = Vec::new();
    let file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok((total, by_kind, samples));
        }
        Err(e) => return Err(e).with_context(|| format!("opening {}", path.display())),
    };
    for line in std::io::BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let m: Mismatch = serde_json::from_str(&line)
            .with_context(|| format!("parsing a record in {}", path.display()))?;
        total += 1;
        *by_kind.entry(m.kind).or_insert(0) += 1;
        if samples.len() < SAMPLE_LIMIT {
            samples.push(m);
        }
    }
    Ok((total, by_kind, samples))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mismatch_round_trips_and_keeps_raw_path_bytes() {
        let m = Mismatch::new(b"/weird-\xff-name", MismatchKind::Content)
            .expected("aa")
            .actual("bb");
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("\"kind\":\"content\""), "{json}");
        let back: Mismatch = serde_json::from_str(&json).unwrap();
        assert_eq!(back, m);
        assert_eq!(back.path().unwrap(), b"/weird-\xff-name");
        assert_eq!(back.path_lossy, "/weird-\u{fffd}-name");
        // Optional fields are omitted, not null.
        let bare = serde_json::to_string(&Mismatch::new(b"/x", MismatchKind::Missing)).unwrap();
        assert!(!bare.contains("expected"), "{bare}");
    }

    #[test]
    fn contract_follows_the_copy_options() {
        let mut o = MigrationOptions::default();
        assert_eq!(Contract::from_options(&o), Contract::strict());
        o.preserve_owner = false;
        let c = Contract::from_options(&o);
        assert!(!c.owner);
        assert!(c.mode && c.mtime);
    }

    #[test]
    fn kind_names_are_stable_wire_strings() {
        for (k, s) in [
            (MismatchKind::Missing, "missing"),
            (MismatchKind::SpecialNotCopied, "special_not_copied"),
            (MismatchKind::SymlinkTarget, "symlink_target"),
            (MismatchKind::ReadError, "read_error"),
        ] {
            assert_eq!(k.as_str(), s);
            assert_eq!(serde_json::to_string(&k).unwrap(), format!("\"{s}\""));
        }
    }

    #[test]
    fn tally_counts_and_samples() {
        let dir = tempfile::tempdir().unwrap();
        let wd = WorkDir::new(dir.path());
        assert_eq!(tally(&wd).unwrap().0, 0, "absent file = no mismatches");
        std::fs::create_dir_all(wd.verify_dir()).unwrap();
        let mut body = String::new();
        for i in 0..(SAMPLE_LIMIT + 5) {
            let kind = if i % 2 == 0 {
                MismatchKind::Missing
            } else {
                MismatchKind::Extra
            };
            body.push_str(&serde_json::to_string(&Mismatch::new(b"/p", kind)).unwrap());
            body.push('\n');
        }
        std::fs::write(wd.root().join(MISMATCHES_FILE), body).unwrap();
        let (total, by_kind, samples) = tally(&wd).unwrap();
        assert_eq!(total, (SAMPLE_LIMIT + 5) as u64);
        assert_eq!(by_kind[&MismatchKind::Missing], 53);
        assert_eq!(by_kind[&MismatchKind::Extra], 52);
        assert_eq!(samples.len(), SAMPLE_LIMIT, "bounded");
    }
}
