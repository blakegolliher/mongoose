//! Phase 2 of cutover verification: read every queued file back from
//! both servers and compare SHA-256 digests; READLINK every queued
//! symlink on both sides and compare the target bytes.
//!
//! The work list (`content-todo.bin`, written by the namespace phase)
//! is processed in order with bounded concurrency. Completion is
//! tracked as a **frontier**: every entry below it is done, its
//! mismatch (if any) appended to `mismatches.jsonl`, and both are
//! durable before `content-progress.json` records the new frontier.
//! A resumed run truncates the mismatch file back to the recorded
//! length and continues from the frontier, so an interrupted cutover
//! never re-reads finished files and never duplicates a record.
//!
//! The libnfs implementation ([`LibnfsChecker`]) holds one
//! source/destination context pair per entry and hashes the two sides
//! on two threads; everything else is generic over
//! [`ContentChecker`] so the driver is tested without an NFS server.

use super::{Mismatch, MismatchKind, MISMATCHES_FILE};
use crate::util::{read_json_opt, write_json_atomic};
use crate::workdir::WorkDir;
use anyhow::{Context, Result};
use async_trait::async_trait;
use base64::Engine;
use migration_core::records::FailurePhase;
use migration_mover::join_root;
use migration_mover::libnfs::{ops, LibnfsContextPool, MultiPool, NfsContext};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

/// Work list under `verify/`.
pub const TODO_FILE: &str = "content-todo.bin";
/// Resume checkpoint under `verify/`.
pub const PROGRESS_FILE: &str = "content-progress.json";
/// How often the frontier is made durable (and logged).
const CHECKPOINT_EVERY: Duration = Duration::from_secs(15);
/// Per-READ size, matching the mover's stream buffer.
const READ_BUF: usize = 1 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TodoKind {
    /// Hash both sides; `size` is the indexed size (for accounting).
    File { size: u64 },
    /// READLINK both sides.
    Symlink,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TodoEntry {
    pub path: Vec<u8>,
    pub kind: TodoKind,
}

/// One entry's verdict. Read failures are mismatches of kind
/// [`MismatchKind::ReadError`]: nothing was proved about that path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckResult {
    Match,
    Mismatch(Mismatch),
}

/// How one entry is read back on both sides.
#[async_trait]
pub trait ContentChecker: Send + Sync {
    async fn check(&self, entry: &TodoEntry) -> CheckResult;
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentCounts {
    pub entries_done: u64,
    pub files_read: u64,
    /// Indexed bytes of the files read (source side).
    pub bytes_read: u64,
    pub symlinks_read: u64,
    pub content: u64,
    pub symlink_target: u64,
    pub read_error: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ContentProgress {
    complete: bool,
    /// Entries `[0, frontier)` of the work list are done and recorded.
    frontier: u64,
    /// Length of `mismatches.jsonl` covering exactly those entries.
    mismatch_file_len: u64,
    counts: ContentCounts,
}

#[derive(Debug)]
pub struct ContentOutcome {
    pub counts: ContentCounts,
    /// Stopped before the work list was drained; re-run to resume.
    pub interrupted: bool,
}

// ---------------------------------------------------------------------
// Work-list encoding: u8 kind | u64 size | u32 path_len | path
// ---------------------------------------------------------------------

const KIND_FILE: u8 = 1;
const KIND_SYMLINK: u8 = 2;

pub struct TodoWriter {
    w: BufWriter<File>,
}

impl TodoWriter {
    pub fn create(path: &Path) -> Result<Self> {
        Ok(Self {
            w: BufWriter::new(
                File::create(path).with_context(|| format!("creating {}", path.display()))?,
            ),
        })
    }

    pub fn push(&mut self, e: &TodoEntry) -> Result<()> {
        let (kind, size) = match e.kind {
            TodoKind::File { size } => (KIND_FILE, size),
            TodoKind::Symlink => (KIND_SYMLINK, 0),
        };
        self.w.write_all(&[kind])?;
        self.w.write_all(&size.to_le_bytes())?;
        self.w.write_all(&(e.path.len() as u32).to_le_bytes())?;
        self.w.write_all(&e.path)?;
        Ok(())
    }

    pub fn finish(mut self) -> Result<()> {
        self.w.flush()?;
        self.w
            .into_inner()
            .context("flushing the content work list")?
            .sync_all()?;
        Ok(())
    }
}

pub struct TodoReader {
    r: BufReader<File>,
    path: PathBuf,
}

impl TodoReader {
    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            r: BufReader::new(
                File::open(path).with_context(|| format!("opening {}", path.display()))?,
            ),
            path: path.to_path_buf(),
        })
    }

    fn read_body(&mut self, kind: u8) -> Result<TodoEntry> {
        let mut head = [0u8; 12];
        self.r
            .read_exact(&mut head)
            .with_context(|| format!("torn entry in {}", self.path.display()))?;
        let size = u64::from_le_bytes(head[0..8].try_into().unwrap());
        let len = u32::from_le_bytes(head[8..12].try_into().unwrap()) as usize;
        let mut path = vec![0u8; len];
        self.r
            .read_exact(&mut path)
            .with_context(|| format!("torn entry in {}", self.path.display()))?;
        let kind = match kind {
            KIND_FILE => TodoKind::File { size },
            KIND_SYMLINK => TodoKind::Symlink,
            other => anyhow::bail!("unknown work-list kind {other} in {}", self.path.display()),
        };
        Ok(TodoEntry { path, kind })
    }
}

impl Iterator for TodoReader {
    type Item = Result<TodoEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut kind = [0u8; 1];
        match self.r.read_exact(&mut kind) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return None,
            Err(e) => {
                return Some(Err(e).with_context(|| format!("reading {}", self.path.display())))
            }
        }
        Some(self.read_body(kind[0]))
    }
}

// ---------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------

/// Run (or resume) the content phase for one pass.
///
/// `initial_mismatch_len` is the length of `mismatches.jsonl` after
/// the namespace phase — the offset a first run appends from.
pub async fn run(
    pass_wd: &WorkDir,
    checker: Arc<dyn ContentChecker>,
    concurrency: usize,
    initial_mismatch_len: u64,
    stop: CancellationToken,
) -> Result<ContentOutcome> {
    let vdir = pass_wd.verify_dir();
    let progress_path = vdir.join(PROGRESS_FILE);
    let mismatches_path = pass_wd.root().join(MISMATCHES_FILE);
    let mut progress = match read_json_opt::<ContentProgress>(&progress_path)? {
        Some(p) => p,
        None => ContentProgress {
            complete: false,
            frontier: 0,
            mismatch_file_len: initial_mismatch_len,
            counts: ContentCounts::default(),
        },
    };
    if progress.complete {
        println!("  content checkpoint valid; every entry was already read back");
        return Ok(ContentOutcome {
            counts: progress.counts,
            interrupted: false,
        });
    }
    if progress.frontier > 0 {
        println!(
            "  resuming content verification at entry {} ({} files, {} bytes already read)",
            progress.frontier, progress.counts.files_read, progress.counts.bytes_read,
        );
    }

    // Records appended after the last durable checkpoint belong to
    // entries that will be re-read; drop them so nothing duplicates.
    {
        let f = OpenOptions::new()
            .write(true)
            .open(&mismatches_path)
            .with_context(|| format!("opening {}", mismatches_path.display()))?;
        f.set_len(progress.mismatch_file_len)?;
        f.sync_all()?;
    }
    let mut out = BufWriter::new(
        OpenOptions::new()
            .append(true)
            .open(&mismatches_path)
            .with_context(|| format!("opening {} for append", mismatches_path.display()))?,
    );

    let mut reader = TodoReader::open(&vdir.join(TODO_FILE))?;
    for _ in 0..progress.frontier {
        match reader.next() {
            Some(r) => {
                r?;
            }
            None => anyhow::bail!(
                "the content work list is shorter than the recorded frontier ({}); \
                 remove {} and re-run to rebuild it",
                progress.frontier,
                vdir.display()
            ),
        }
    }

    let concurrency = concurrency.max(1);
    let mut inflight: JoinSet<(u64, TodoKind, CheckResult)> = JoinSet::new();
    let mut finished: BTreeMap<u64, (TodoKind, CheckResult)> = BTreeMap::new();
    let mut next_idx = progress.frontier;
    let mut exhausted = false;
    let mut last_checkpoint = Instant::now();

    loop {
        while !exhausted && !stop.is_cancelled() && inflight.len() < concurrency {
            match reader.next() {
                Some(entry) => {
                    let entry = entry?;
                    let idx = next_idx;
                    next_idx += 1;
                    let checker = Arc::clone(&checker);
                    inflight.spawn(async move {
                        let r = checker.check(&entry).await;
                        (idx, entry.kind, r)
                    });
                }
                None => exhausted = true,
            }
        }
        let Some(joined) = inflight.join_next().await else {
            break;
        };
        let (idx, kind, result) = joined.context("content check task panicked")?;
        finished.insert(idx, (kind, result));
        while let Some((kind, result)) = finished.remove(&progress.frontier) {
            record(&mut progress.counts, kind, &result, &mut out)?;
            progress.frontier += 1;
        }
        if last_checkpoint.elapsed() >= CHECKPOINT_EVERY {
            checkpoint(&mut out, &mut progress, &progress_path)?;
            last_checkpoint = Instant::now();
            let c = &progress.counts;
            tracing::info!(
                entries_done = c.entries_done,
                files = c.files_read,
                bytes = c.bytes_read,
                symlinks = c.symlinks_read,
                mismatches = c.content + c.symlink_target + c.read_error,
                "verifying",
            );
        }
    }
    debug_assert!(
        finished.is_empty(),
        "frontier must have consumed every result"
    );
    let interrupted = !exhausted;
    progress.complete = !interrupted;
    checkpoint(&mut out, &mut progress, &progress_path)?;
    Ok(ContentOutcome {
        counts: progress.counts,
        interrupted,
    })
}

fn record(
    counts: &mut ContentCounts,
    kind: TodoKind,
    result: &CheckResult,
    out: &mut impl Write,
) -> Result<()> {
    match kind {
        TodoKind::File { size } => {
            counts.files_read += 1;
            counts.bytes_read += size;
        }
        TodoKind::Symlink => counts.symlinks_read += 1,
    }
    counts.entries_done += 1;
    if let CheckResult::Mismatch(m) = result {
        match m.kind {
            MismatchKind::Content => counts.content += 1,
            MismatchKind::SymlinkTarget => counts.symlink_target += 1,
            _ => counts.read_error += 1,
        }
        serde_json::to_writer(&mut *out, m)?;
        out.write_all(b"\n")?;
    }
    Ok(())
}

/// Make everything below the frontier durable, in order: records,
/// then the progress file that vouches for them.
fn checkpoint(
    out: &mut BufWriter<File>,
    progress: &mut ContentProgress,
    progress_path: &Path,
) -> Result<()> {
    out.flush()?;
    out.get_ref().sync_all()?;
    progress.mismatch_file_len = out.get_ref().metadata()?.len();
    write_json_atomic(progress_path, progress)
}

// ---------------------------------------------------------------------
// Comparison (pure)
// ---------------------------------------------------------------------

/// `(bytes read, SHA-256)` of one side, or a read error message.
pub type SideHash = Result<(u64, [u8; 32]), String>;

/// Stream a file through SHA-256 with `read_at(offset, buf) -> bytes`
/// until it returns 0. Short reads are fine; only 0 means EOF.
pub fn hash_stream(mut read_at: impl FnMut(u64, &mut [u8]) -> Result<usize, String>) -> SideHash {
    let mut buf = vec![0u8; READ_BUF];
    let mut off = 0u64;
    let mut h = Sha256::new();
    loop {
        let n = read_at(off, &mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        off += n as u64;
    }
    Ok((off, h.finalize().into()))
}

pub fn compare_hashes(path: &[u8], indexed_size: u64, src: SideHash, dst: SideHash) -> CheckResult {
    match (src, dst) {
        (Err(s), Err(d)) => CheckResult::Mismatch(
            Mismatch::new(path, MismatchKind::ReadError)
                .detail(format!("source: {s}; destination: {d}")),
        ),
        (Err(s), _) => CheckResult::Mismatch(
            Mismatch::new(path, MismatchKind::ReadError).detail(format!("source: {s}")),
        ),
        (_, Err(d)) => CheckResult::Mismatch(
            Mismatch::new(path, MismatchKind::ReadError).detail(format!("destination: {d}")),
        ),
        (Ok((sb, sh)), Ok((db, dh))) => {
            if sb == db && sh == dh {
                return CheckResult::Match;
            }
            let mut m = Mismatch::new(path, MismatchKind::Content)
                .expected(format!("sha256:{} ({sb} bytes)", hex::encode(sh)))
                .actual(format!("sha256:{} ({db} bytes)", hex::encode(dh)));
            if sb != indexed_size {
                m = m.detail(format!(
                    "the source is {sb} bytes now; the index recorded {indexed_size}"
                ));
            }
            CheckResult::Mismatch(m)
        }
    }
}

pub fn compare_targets(
    path: &[u8],
    src: Result<Vec<u8>, String>,
    dst: Result<Vec<u8>, String>,
) -> CheckResult {
    match (src, dst) {
        (Err(s), Err(d)) => CheckResult::Mismatch(
            Mismatch::new(path, MismatchKind::ReadError)
                .detail(format!("source: {s}; destination: {d}")),
        ),
        (Err(s), _) => CheckResult::Mismatch(
            Mismatch::new(path, MismatchKind::ReadError).detail(format!("source: {s}")),
        ),
        (_, Err(d)) => CheckResult::Mismatch(
            Mismatch::new(path, MismatchKind::ReadError).detail(format!("destination: {d}")),
        ),
        (Ok(s), Ok(d)) if s == d => CheckResult::Match,
        (Ok(s), Ok(d)) => {
            let b64 = base64::engine::general_purpose::STANDARD;
            CheckResult::Mismatch(
                Mismatch::new(path, MismatchKind::SymlinkTarget)
                    .expected(String::from_utf8_lossy(&s).into_owned())
                    .actual(String::from_utf8_lossy(&d).into_owned())
                    .detail(format!(
                        "expected_b64={} actual_b64={}",
                        b64.encode(&s),
                        b64.encode(&d)
                    )),
            )
        }
    }
}

// ---------------------------------------------------------------------
// libnfs implementation
// ---------------------------------------------------------------------

/// Reads both sides over libnfs: one mounted (source, destination)
/// context pair per in-flight entry, from a [`MultiPool`] sized to
/// the requested concurrency.
pub struct LibnfsChecker {
    pool: Arc<MultiPool>,
    src_root: Vec<u8>,
    dst_root: Vec<u8>,
}

impl LibnfsChecker {
    /// Mount `pairs` context pairs against both URLs.
    pub fn mount(
        src_url: &str,
        dst_url: &str,
        src_root: &str,
        dst_root: &str,
        pairs: usize,
    ) -> Result<Arc<Self>> {
        let pool = MultiPool::build(
            src_url,
            dst_url,
            pairs.max(1),
            migration_mover::DEFAULT_RPC_TIMEOUT_MS,
        )?;
        tracing::info!(
            pairs = pool.capacity(),
            src = src_url,
            dst = dst_url,
            "verification pool mounted"
        );
        Ok(Arc::new(Self {
            pool,
            src_root: src_root.as_bytes().to_vec(),
            dst_root: dst_root.as_bytes().to_vec(),
        }))
    }
}

impl LibnfsChecker {
    /// The mounted pool, for the endpoint identity check.
    pub fn pool(&self) -> Arc<dyn LibnfsContextPool> {
        self.pool.clone()
    }
}

fn fmt_err(e: migration_mover::MoveError) -> String {
    format!("{} during {:?}", e.error, e.phase)
}

/// Hash one path on one context: open, stream READs, close.
fn hash_side(ctx: &mut NfsContext, path: &[u8]) -> SideHash {
    let fh = ops::open_read(ctx, path).map_err(fmt_err)?;
    let result = hash_stream(|off, buf| ops::pread(ctx, &fh, off, buf).map_err(fmt_err));
    match result {
        Ok(v) => {
            ops::close_fh(ctx, fh, FailurePhase::Read).map_err(fmt_err)?;
            Ok(v)
        }
        Err(e) => {
            ops::close_quietly(ctx, fh);
            Err(e)
        }
    }
}

#[async_trait]
impl ContentChecker for LibnfsChecker {
    async fn check(&self, entry: &TodoEntry) -> CheckResult {
        let pair = match self.pool.acquire().await {
            Ok(p) => p,
            Err(e) => {
                return CheckResult::Mismatch(
                    Mismatch::new(&entry.path, MismatchKind::ReadError)
                        .detail(format!("acquiring an NFS context pair: {e:#}")),
                )
            }
        };
        let src = join_root(&self.src_root, &entry.path);
        let dst = join_root(&self.dst_root, &entry.path);
        let path = entry.path.clone();
        let entry = entry.clone();
        let joined = tokio::task::spawn_blocking(move || {
            let mut pair = pair;
            let (s, d) = pair.split();
            match entry.kind {
                TodoKind::File { size } => {
                    // Two servers, two threads: the pair's contexts are
                    // independent, so the destination hashes while the
                    // source does.
                    let (sr, dr) = std::thread::scope(|sc| {
                        let dj = sc.spawn(|| hash_side(d, &dst));
                        let sr = hash_side(s, &src);
                        let dr = dj
                            .join()
                            .unwrap_or_else(|_| Err("destination hash thread panicked".into()));
                        (sr, dr)
                    });
                    compare_hashes(&entry.path, size, sr, dr)
                }
                TodoKind::Symlink => {
                    let sr = ops::readlink(s, &src).map_err(fmt_err);
                    let dr = ops::readlink(d, &dst).map_err(fmt_err);
                    compare_targets(&entry.path, sr, dr)
                }
            }
        })
        .await;
        match joined {
            Ok(r) => r,
            Err(e) => CheckResult::Mismatch(
                Mismatch::new(&path, MismatchKind::ReadError)
                    .detail(format!("content check task failed: {e}")),
            ),
        }
    }
}

// ---------------------------------------------------------------------
// In-memory implementation for tests and dry runs
// ---------------------------------------------------------------------

/// What an in-memory side holds at a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FakeSide {
    File(Vec<u8>),
    Symlink(Vec<u8>),
    /// Reading fails with this message.
    Error(String),
}

/// A [`ContentChecker`] over two in-memory trees. Public so the
/// crate's integration tests can drive the whole verification without
/// an NFS server; never used by the CLI.
#[derive(Debug, Default, Clone)]
pub struct FakeChecker {
    pub src: BTreeMap<Vec<u8>, FakeSide>,
    pub dst: BTreeMap<Vec<u8>, FakeSide>,
}

impl FakeChecker {
    pub fn file(mut self, path: &[u8], src: &[u8], dst: &[u8]) -> Self {
        self.src.insert(path.to_vec(), FakeSide::File(src.to_vec()));
        self.dst.insert(path.to_vec(), FakeSide::File(dst.to_vec()));
        self
    }

    pub fn symlink(mut self, path: &[u8], src: &[u8], dst: &[u8]) -> Self {
        self.src
            .insert(path.to_vec(), FakeSide::Symlink(src.to_vec()));
        self.dst
            .insert(path.to_vec(), FakeSide::Symlink(dst.to_vec()));
        self
    }

    fn hash(side: Option<&FakeSide>) -> SideHash {
        match side {
            Some(FakeSide::File(bytes)) => {
                let mut off = 0usize;
                hash_stream(|_, buf| {
                    let n = (bytes.len() - off).min(buf.len()).min(3); // short reads on purpose
                    buf[..n].copy_from_slice(&bytes[off..off + n]);
                    off += n;
                    Ok(n)
                })
            }
            Some(FakeSide::Error(e)) => Err(e.clone()),
            Some(FakeSide::Symlink(_)) => Err("EINVAL during Open".into()),
            None => Err("ENOENT during Open".into()),
        }
    }

    fn target(side: Option<&FakeSide>) -> Result<Vec<u8>, String> {
        match side {
            Some(FakeSide::Symlink(t)) => Ok(t.clone()),
            Some(FakeSide::Error(e)) => Err(e.clone()),
            Some(FakeSide::File(_)) => Err("EINVAL during Symlink".into()),
            None => Err("ENOENT during Symlink".into()),
        }
    }
}

#[async_trait]
impl ContentChecker for FakeChecker {
    async fn check(&self, entry: &TodoEntry) -> CheckResult {
        let (s, d) = (self.src.get(&entry.path), self.dst.get(&entry.path));
        match entry.kind {
            TodoKind::File { size } => {
                compare_hashes(&entry.path, size, Self::hash(s), Self::hash(d))
            }
            TodoKind::Symlink => compare_targets(&entry.path, Self::target(s), Self::target(d)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sha(bytes: &[u8]) -> [u8; 32] {
        Sha256::digest(bytes).into()
    }

    #[test]
    fn hash_stream_handles_short_reads_and_eof() {
        let data: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
        let mut calls = 0;
        let got = hash_stream(|off, buf| {
            calls += 1;
            let off = off as usize;
            // Alternate full and short reads; 0 at EOF.
            let want = if calls % 2 == 0 { 1000 } else { buf.len() };
            let n = (data.len() - off).min(want);
            buf[..n].copy_from_slice(&data[off..off + n]);
            Ok(n)
        })
        .unwrap();
        assert_eq!(got, (data.len() as u64, sha(&data)));
        let err = hash_stream(|_, _| Err::<usize, _>("EIO during Read".to_string())).unwrap_err();
        assert_eq!(err, "EIO during Read");
        assert_eq!(
            hash_stream(|_, _| Ok::<usize, String>(0)).unwrap(),
            (0, sha(b""))
        );
    }

    #[test]
    fn compare_hashes_matrix() {
        let a: SideHash = Ok((5, sha(b"hello")));
        assert_eq!(
            compare_hashes(b"/p", 5, a.clone(), a.clone()),
            CheckResult::Match
        );

        // Same size, different bytes: the case scan-diff cannot see.
        let b: SideHash = Ok((5, sha(b"jello")));
        let CheckResult::Mismatch(m) = compare_hashes(b"/p", 5, a.clone(), b) else {
            panic!("expected a content mismatch");
        };
        assert_eq!(m.kind, MismatchKind::Content);
        assert!(m.expected.as_deref().unwrap().starts_with("sha256:"));
        assert!(m.detail.is_none());

        // Truncated destination.
        let t: SideHash = Ok((3, sha(b"hel")));
        let CheckResult::Mismatch(m) = compare_hashes(b"/p", 5, a.clone(), t) else {
            panic!()
        };
        assert_eq!(m.kind, MismatchKind::Content);
        assert!(m.actual.as_deref().unwrap().contains("(3 bytes)"), "{m:?}");

        // Source grew since the scan: reported in detail.
        let g: SideHash = Ok((9, sha(b"hello....")));
        let CheckResult::Mismatch(m) = compare_hashes(b"/p", 5, g, a.clone()) else {
            panic!()
        };
        assert!(
            m.detail.as_deref().unwrap().contains("index recorded 5"),
            "{m:?}"
        );

        // Read errors name the side.
        for (s, d, want) in [
            (
                Err("EACCES during Open".to_string()),
                a.clone(),
                "source: EACCES",
            ),
            (
                a.clone(),
                Err("ESTALE during Read".to_string()),
                "destination: ESTALE",
            ),
            (
                Err("x".to_string()),
                Err("y".to_string()),
                "source: x; destination: y",
            ),
        ] {
            let CheckResult::Mismatch(m) = compare_hashes(b"/p", 5, s, d) else {
                panic!()
            };
            assert_eq!(m.kind, MismatchKind::ReadError);
            assert!(m.detail.as_deref().unwrap().contains(want), "{m:?}");
        }
    }

    #[test]
    fn compare_targets_matrix() {
        assert_eq!(
            compare_targets(b"/l", Ok(b"../t".to_vec()), Ok(b"../t".to_vec())),
            CheckResult::Match
        );
        let CheckResult::Mismatch(m) =
            compare_targets(b"/l", Ok(b"../t".to_vec()), Ok(b"/abs/\xff".to_vec()))
        else {
            panic!()
        };
        assert_eq!(m.kind, MismatchKind::SymlinkTarget);
        assert_eq!(m.expected.as_deref(), Some("../t"));
        assert!(
            m.detail.as_deref().unwrap().contains("actual_b64="),
            "{m:?}"
        );
        let CheckResult::Mismatch(m) = compare_targets(
            b"/l",
            Ok(b"t".to_vec()),
            Err("ENOENT during Symlink".into()),
        ) else {
            panic!()
        };
        assert_eq!(m.kind, MismatchKind::ReadError);
    }

    #[test]
    fn todo_encoding_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("todo.bin");
        let entries = vec![
            TodoEntry {
                path: b"/a\xff".to_vec(),
                kind: TodoKind::File { size: 1 << 40 },
            },
            TodoEntry {
                path: b"/l".to_vec(),
                kind: TodoKind::Symlink,
            },
        ];
        let mut w = TodoWriter::create(&p).unwrap();
        for e in &entries {
            w.push(e).unwrap();
        }
        w.finish().unwrap();
        let back: Vec<TodoEntry> = TodoReader::open(&p).unwrap().map(|e| e.unwrap()).collect();
        assert_eq!(back, entries);
    }

    // ---- driver ----------------------------------------------------------

    /// A pass dir with `n` file entries and one symlink queued, an empty
    /// (namespace-phase) mismatches file, and a fake checker where
    /// `bad` paths differ on the destination.
    fn fixture(n: usize, bad: &[usize]) -> (tempfile::TempDir, WorkDir, Arc<FakeChecker>) {
        let tmp = tempfile::tempdir().unwrap();
        let wd = WorkDir::new(tmp.path().join("pass"));
        std::fs::create_dir_all(wd.verify_dir()).unwrap();
        std::fs::write(wd.root().join(MISMATCHES_FILE), b"").unwrap();
        let mut w = TodoWriter::create(&wd.verify_dir().join(TODO_FILE)).unwrap();
        let mut fake = FakeChecker::default();
        for i in 0..n {
            let path = format!("/f/{i:04}").into_bytes();
            let body = format!("body-{i}").into_bytes();
            let dst = if bad.contains(&i) {
                b"corrupt".to_vec()
            } else {
                body.clone()
            };
            fake = fake.file(&path, &body, &dst);
            w.push(&TodoEntry {
                path,
                kind: TodoKind::File {
                    size: body.len() as u64,
                },
            })
            .unwrap();
        }
        fake = fake.symlink(b"/link", b"target", b"target");
        w.push(&TodoEntry {
            path: b"/link".to_vec(),
            kind: TodoKind::Symlink,
        })
        .unwrap();
        w.finish().unwrap();
        (tmp, wd, Arc::new(fake))
    }

    fn mismatches(wd: &WorkDir) -> Vec<Mismatch> {
        std::fs::read_to_string(wd.root().join(MISMATCHES_FILE))
            .unwrap()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn driver_reads_everything_and_records_in_order() {
        let (_tmp, wd, fake) = fixture(50, &[3, 17, 42]);
        let out = run(&wd, fake, 8, 0, CancellationToken::new())
            .await
            .unwrap();
        assert!(!out.interrupted);
        assert_eq!(out.counts.entries_done, 51);
        assert_eq!(out.counts.files_read, 50);
        assert_eq!(out.counts.symlinks_read, 1);
        assert_eq!(out.counts.content, 3);
        assert_eq!(
            out.counts.bytes_read,
            (0..50)
                .map(|i| format!("body-{i}").len() as u64)
                .sum::<u64>()
        );
        let m = mismatches(&wd);
        let paths: Vec<Vec<u8>> = m.iter().map(|m| m.path().unwrap()).collect();
        assert_eq!(
            paths,
            vec![
                b"/f/0003".to_vec(),
                b"/f/0017".to_vec(),
                b"/f/0042".to_vec()
            ],
            "frontier order"
        );
        // Complete: a re-run does no work and reports the same counts.
        let again = run(
            &wd,
            Arc::new(FakeChecker::default()),
            8,
            0,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(again.counts, out.counts);
        assert_eq!(mismatches(&wd).len(), 3, "no duplicates");
    }

    #[tokio::test]
    async fn driver_resumes_from_the_frontier_and_drops_torn_appends() {
        let (_tmp, wd, fake) = fixture(20, &[2, 15]);
        // Simulate a crash: entry 2's record was appended, then junk
        // landed after it, but the progress file only vouches for the
        // first 5 entries (which include entry 2's record).
        let rec = serde_json::to_string(&Mismatch::new(b"/f/0002", MismatchKind::Content)).unwrap();
        let good = format!("{rec}\n");
        std::fs::write(wd.root().join(MISMATCHES_FILE), format!("{good}{{torn")).unwrap();
        write_json_atomic(
            &wd.verify_dir().join(PROGRESS_FILE),
            &ContentProgress {
                complete: false,
                frontier: 5,
                mismatch_file_len: good.len() as u64,
                counts: ContentCounts {
                    entries_done: 5,
                    files_read: 5,
                    bytes_read: 30,
                    content: 1,
                    ..ContentCounts::default()
                },
            },
        )
        .unwrap();
        let out = run(&wd, fake, 4, 0, CancellationToken::new())
            .await
            .unwrap();
        assert!(!out.interrupted);
        assert_eq!(out.counts.entries_done, 21);
        assert_eq!(out.counts.files_read, 20);
        assert_eq!(
            out.counts.content, 2,
            "one carried from the checkpoint, one new"
        );
        let m = mismatches(&wd);
        assert_eq!(m.len(), 2, "torn tail removed, nothing duplicated: {m:?}");
        assert_eq!(m[0].path().unwrap(), b"/f/0002");
        assert_eq!(m[1].path().unwrap(), b"/f/0015");
    }

    #[tokio::test]
    async fn driver_stops_at_a_boundary_when_cancelled() {
        let (_tmp, wd, fake) = fixture(10, &[]);
        let stop = CancellationToken::new();
        stop.cancel();
        let out = run(&wd, fake.clone(), 4, 0, stop).await.unwrap();
        assert!(out.interrupted, "nothing proved");
        assert_eq!(out.counts.entries_done, 0);
        let progress: ContentProgress = read_json_opt(&wd.verify_dir().join(PROGRESS_FILE))
            .unwrap()
            .unwrap();
        assert!(!progress.complete);
        // Resuming without the token finishes the job.
        let out = run(&wd, fake, 4, 0, CancellationToken::new())
            .await
            .unwrap();
        assert!(!out.interrupted);
        assert_eq!(out.counts.entries_done, 11);
    }

    #[tokio::test]
    async fn frontier_beyond_the_work_list_is_refused() {
        let (_tmp, wd, fake) = fixture(3, &[]);
        write_json_atomic(
            &wd.verify_dir().join(PROGRESS_FILE),
            &ContentProgress {
                complete: false,
                frontier: 99,
                mismatch_file_len: 0,
                counts: ContentCounts::default(),
            },
        )
        .unwrap();
        let err = run(&wd, fake, 4, 0, CancellationToken::new())
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("shorter than the recorded frontier"),
            "{err:#}"
        );
    }

    #[tokio::test]
    async fn fake_checker_reports_read_errors_and_targets() {
        let fake = FakeChecker::default()
            .file(b"/ok", b"same", b"same")
            .symlink(b"/l", b"a", b"b");
        let mut fake = fake;
        fake.dst
            .insert(b"/gone".to_vec(), FakeSide::Error("EIO during Read".into()));
        fake.src
            .insert(b"/gone".to_vec(), FakeSide::File(b"x".to_vec()));
        let entry = |path: &[u8], kind| TodoEntry {
            path: path.to_vec(),
            kind,
        };
        assert_eq!(
            fake.check(&entry(b"/ok", TodoKind::File { size: 4 })).await,
            CheckResult::Match
        );
        let CheckResult::Mismatch(m) = fake.check(&entry(b"/l", TodoKind::Symlink)).await else {
            panic!()
        };
        assert_eq!(m.kind, MismatchKind::SymlinkTarget);
        let CheckResult::Mismatch(m) = fake
            .check(&entry(b"/gone", TodoKind::File { size: 1 }))
            .await
        else {
            panic!()
        };
        assert_eq!(m.kind, MismatchKind::ReadError);
        let CheckResult::Mismatch(m) = fake
            .check(&entry(b"/absent", TodoKind::File { size: 1 }))
            .await
        else {
            panic!()
        };
        assert!(m.detail.as_deref().unwrap().contains("ENOENT"));
    }
}
