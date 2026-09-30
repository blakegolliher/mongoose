//! End-to-end regression for special nodes (fifo, socket, block and
//! character device), no NFS and no real device nodes.
//!
//! A synthetic walker scan with one regular file and one of each
//! special type goes through the real pipeline:
//!
//! 1. `mig_walker_rewrite::run_rewrite` — walker strings to canonical
//!    tags and modes;
//! 2. `ShardProcessor::process` with the real `Mover` deciding every
//!    special row (only the regular-file copy itself is stubbed, since
//!    it needs a server);
//! 3. the copy loop's own persistence: results to `downgrades/`, then
//!    the shard committed to `progress.json`;
//! 4. `verify::run`, the cutover check, against a destination index.
//!
//! What it pins down: a special node is never sent down the file-data
//! path, never counted as a copied file, always leaves a durable
//! record with its raw path, completes its shard without being
//! retried, and blocks cutover until the destination has a node of
//! the same type.

use arrow::array::{
    ArrayRef, BinaryBuilder, Int64Builder, StringBuilder, UInt16Builder, UInt32Builder,
    UInt64Builder,
};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use async_trait::async_trait;
use base64::Engine;
use migration_core::fence::Fence;
use migration_core::records::{
    DowngradeKind, DowngradeRecord, Endpoint, EndpointKind, MigrationOptions, SpecialNode,
};
use migration_core::schema::{FileTypeTag, S_IFBLK, S_IFCHR, S_IFIFO, S_IFREG, S_IFSOCK};
use migration_core::shard::{RowView, ShardReader};
use migration_mover::batch::{BatchBudget, InflightLimiter, InflightProfile};
use migration_mover::strategy::Strategy;
use migration_mover::{
    ContextPair, DowngradeSink, FailureSink, FileMover, LibnfsContextPool, MoveOutcome, Mover,
    MoverConfig,
};
use migration_worker::events::EventEmitter;
use migration_worker::heartbeat::LivePending;
use migration_worker::shard_processor::{ProcessOutcome, ShardProcessor};
use migration_worker::throughput::ThroughputCounter;
use mongoose::copy::{special_not_copied_notice, CopySummary};
use mongoose::manifest::{self, LocalManifest};
use mongoose::progress::{persist_shard_results_then, write_shard_jsonl, CopyProgress};
use mongoose::verify::content::{CheckResult, ContentChecker, TodoEntry};
use mongoose::verify::{self, MismatchKind, VerifyParams, VerifyReport, VerifyStatus};
use mongoose::workdir::WorkDir;
use parquet::arrow::ArrowWriter;
use std::path::Path;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

/// One source entry as the walker reports it.
#[derive(Clone)]
struct WalkerRow {
    path: &'static [u8],
    file_type: &'static str,
    permissions: u16,
    size: u64,
    inode: u64,
    nlink: u32,
}

fn wrow(path: &'static [u8], file_type: &'static str, permissions: u16, inode: u64) -> WalkerRow {
    WalkerRow {
        path,
        file_type,
        permissions,
        size: 0,
        inode,
        nlink: 1,
    }
}

const FILE: &[u8] = b"/file.bin";
const FIFO: &[u8] = b"/fifo-\xff"; // not UTF-8
const SOCK: &[u8] = b"/run/app.sock";
const BLK: &[u8] = b"/dev/sdz";
const CHR: &[u8] = b"/dev/ttyZ9";

/// One regular file and all four special types.
fn source_tree() -> Vec<WalkerRow> {
    vec![
        WalkerRow {
            size: 11,
            ..wrow(FILE, "file", 0o644, 100)
        },
        wrow(FIFO, "fifo", 0o600, 101),
        wrow(SOCK, "socket", 0o660, 102),
        wrow(BLK, "block_device", 0o640, 103),
        wrow(CHR, "char_device", 0o620, 104),
    ]
}

/// Write one walker-schema part: the legacy UTF-8 `path` plus the raw
/// `path_bytes`, as a current walker does.
fn write_walker_part(path: &Path, rows: &[WalkerRow]) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("path", DataType::Utf8, false),
        Field::new("path_bytes", DataType::Binary, false),
        Field::new("file_type", DataType::Utf8, false),
        Field::new("permissions", DataType::UInt16, false),
        Field::new("mtime_us", DataType::Int64, true),
        Field::new("inode", DataType::UInt64, false),
        Field::new("fsid", DataType::UInt64, true),
        Field::new("nlink", DataType::UInt32, false),
        Field::new("uid", DataType::UInt32, false),
        Field::new("gid", DataType::UInt32, false),
        Field::new("size", DataType::UInt64, false),
    ]));
    let mut path_b = StringBuilder::new();
    let mut raw_b = BinaryBuilder::new();
    let mut ft_b = StringBuilder::new();
    let mut perm_b = UInt16Builder::new();
    let mut mtime_b = Int64Builder::new();
    let mut inode_b = UInt64Builder::new();
    let mut fsid_b = UInt64Builder::new();
    let mut nlink_b = UInt32Builder::new();
    let mut uid_b = UInt32Builder::new();
    let mut gid_b = UInt32Builder::new();
    let mut size_b = UInt64Builder::new();
    for r in rows {
        path_b.append_value(String::from_utf8_lossy(r.path));
        raw_b.append_value(r.path);
        ft_b.append_value(r.file_type);
        perm_b.append_value(r.permissions);
        mtime_b.append_value(1_700_000_000_000_000);
        inode_b.append_value(r.inode);
        fsid_b.append_value(9);
        nlink_b.append_value(r.nlink);
        uid_b.append_value(1000);
        gid_b.append_value(1000);
        size_b.append_value(r.size);
    }
    let arrays: Vec<ArrayRef> = vec![
        Arc::new(path_b.finish()),
        Arc::new(raw_b.finish()),
        Arc::new(ft_b.finish()),
        Arc::new(perm_b.finish()),
        Arc::new(mtime_b.finish()),
        Arc::new(inode_b.finish()),
        Arc::new(fsid_b.finish()),
        Arc::new(nlink_b.finish()),
        Arc::new(uid_b.finish()),
        Arc::new(gid_b.finish()),
        Arc::new(size_b.finish()),
    ];
    let batch = RecordBatch::try_new(schema.clone(), arrays).unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut w = ArrowWriter::try_new(std::fs::File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn endpoint(url: &str) -> Endpoint {
    Endpoint {
        kind: EndpointKind::Nfs,
        url: url.into(),
        root: "/".into(),
    }
}

/// Scan fixture -> canonical shards -> local manifest: what `prepare`
/// and each `sync` pass do.
fn rewrite_and_build(wd: &WorkDir, rows: &[WalkerRow]) -> LocalManifest {
    let scan_dir = wd.root().join("scan-fixture");
    write_walker_part(&scan_dir.join("part-r00-00000.parquet"), rows);
    std::fs::create_dir_all(wd.canonical_dir()).unwrap();
    mig_walker_rewrite::run_rewrite(&mig_walker_rewrite::Cli {
        input: scan_dir,
        output: wd.canonical_dir(),
        source_root: "/".into(),
        walker_version: "nfs-walker test (fixture)".into(),
        resume: true,
        report: Some(wd.rewrite_json()),
        verbose: false,
    })
    .unwrap();
    manifest::build(
        wd,
        "run-special",
        endpoint("nfs://s/e"),
        endpoint("nfs://d/e"),
        MigrationOptions::default(),
    )
    .unwrap()
}

fn read_rows(shard: &Path) -> Vec<RowView> {
    ShardReader::open(shard)
        .unwrap()
        .into_rows()
        .unwrap()
        .collect::<migration_core::Result<_>>()
        .unwrap()
}

/// There is no NFS in this test. A row that asks for a context pair
/// fails, and the assertions on `files_failed` catch it.
struct NoNfs;

#[async_trait]
impl LibnfsContextPool for NoNfs {
    async fn acquire(&self) -> anyhow::Result<ContextPair> {
        anyhow::bail!("a row tried to reach NFS in a test that has none")
    }
}

/// The real `Mover`, except that a regular-file copy is recorded
/// instead of performed. Every other row, every special node included,
/// is decided by `Mover::move_one`.
struct RecordingMover {
    real: Mover,
    created: Mutex<Vec<Vec<u8>>>,
}

impl RecordingMover {
    fn new(downgrades: DowngradeSink) -> Self {
        let cfg = MoverConfig::from_options(
            "nfs://s/e".into(),
            "nfs://d/e".into(),
            "/".into(),
            "/".into(),
            &MigrationOptions::default(),
        );
        Self {
            real: Mover::new(cfg, Arc::new(NoNfs), "test-host", downgrades, Fence::new()),
            created: Mutex::new(Vec::new()),
        }
    }

    fn created(&self) -> Vec<Vec<u8>> {
        self.created.lock().unwrap().clone()
    }
}

#[async_trait]
impl FileMover for RecordingMover {
    async fn move_one(&self, row: &RowView) -> MoveOutcome {
        if row.file_type == FileTypeTag::Regular {
            self.created.lock().unwrap().push(row.path.clone());
            return MoveOutcome {
                row_id: row.row_id,
                strategy: Strategy::LibnfsIoUring,
                bytes_moved: row.size,
                torn: false,
                result: Ok(()),
            };
        }
        self.real.move_one(row).await
    }

    async fn move_hardlink(&self, row: &RowView, link_target: &[u8]) -> MoveOutcome {
        self.real.move_hardlink(row, link_target).await
    }

    fn downgrade_sink(&self) -> &DowngradeSink {
        self.real.downgrade_sink()
    }
}

struct Copied {
    outcome: ProcessOutcome,
    created: Vec<Vec<u8>>,
    downgrades_jsonl: Vec<u8>,
    failures_jsonl: Vec<u8>,
    live: Arc<LivePending>,
}

/// Process one shard exactly as `mongoose copy` does, up to the point
/// where it persists the results.
async fn copy_shard(shard_name: &str, shard: &Path) -> Copied {
    let downgrades = DowngradeSink::new();
    let failures = FailureSink::new();
    downgrades.set_current_shard(shard_name);
    failures.set_current_shard(shard_name);
    let mover = Arc::new(RecordingMover::new(downgrades.clone()));
    let live = Arc::new(LivePending::default());
    let mut processor = ShardProcessor {
        mover: Arc::clone(&mover) as Arc<dyn FileMover>,
        live: Arc::clone(&live),
        fence: Fence::new(),
        budget: BatchBudget::default(),
        inflight: InflightLimiter::new(&InflightProfile::default()),
        failures: failures.clone(),
        throughput: ThroughputCounter::new(),
        dir_restamp: Vec::new(),
        fsid_ungrouped_warned: false,
        emitter: EventEmitter::disabled(),
        run_control: None,
        stop: CancellationToken::new(),
    };
    let outcome = processor.process(shard).await.unwrap();
    Copied {
        outcome,
        created: mover.created(),
        downgrades_jsonl: downgrades.drain_jsonl(),
        failures_jsonl: failures.drain_jsonl(),
        live,
    }
}

fn parse_downgrades(body: &[u8]) -> Vec<DowngradeRecord> {
    body.split(|&b| b == b'\n')
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_slice(l).unwrap())
        .collect()
}

fn record_path(r: &DowngradeRecord) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(&r.path_b64)
        .unwrap()
}

/// Every file and symlink reads back identical: the content phase is
/// not what this test is about.
struct AllContentMatches;

#[async_trait]
impl ContentChecker for AllContentMatches {
    async fn check(&self, _entry: &TodoEntry) -> CheckResult {
        CheckResult::Match
    }
}

/// Run the cutover verification of the source index against a
/// destination that holds `dest_rows`.
async fn cutover_verify(
    root: &Path,
    name: &str,
    source: &LocalManifest,
    dest_rows: &[WalkerRow],
) -> VerifyReport {
    let src_wd = WorkDir::new(root);
    let dst_wd = WorkDir::new(root.join(format!("dest-{name}")));
    let dest = rewrite_and_build(&dst_wd, dest_rows);
    let pass_wd = WorkDir::new(root.join(format!("pass-{name}")));
    std::fs::create_dir_all(pass_wd.root()).unwrap();
    let params = VerifyParams {
        run_id: source.run_id.clone(),
        pass: 1,
        source: source.source.clone(),
        dest: source.dest.clone(),
        options: MigrationOptions::default(),
        source_shards: source
            .shards
            .iter()
            .map(|s| src_wd.shard_path(&s.path))
            .collect(),
        dest_shards: dest
            .shards
            .iter()
            .map(|s| dst_wd.shard_path(&s.path))
            .collect(),
        concurrency: 2,
        buckets: 4,
    };
    verify::run(
        &pass_wd,
        &params,
        Arc::new(AllContentMatches),
        CancellationToken::new(),
    )
    .await
    .unwrap()
}

fn count(report: &VerifyReport, kind: MismatchKind) -> u64 {
    report.mismatches_by_kind.get(&kind).copied().unwrap_or(0)
}

fn mismatch_paths(root: &Path, name: &str, kind: MismatchKind) -> Vec<Vec<u8>> {
    let body = std::fs::read_to_string(
        root.join(format!("pass-{name}"))
            .join(verify::MISMATCHES_FILE),
    )
    .unwrap();
    let mut paths: Vec<Vec<u8>> = body
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<verify::Mismatch>(l).unwrap())
        .filter(|m| m.kind == kind)
        .map(|m| m.path().unwrap())
        .collect();
    paths.sort();
    paths
}

#[tokio::test]
async fn special_nodes_are_omitted_truthfully_and_block_cutover() {
    let tmp = tempfile::tempdir().unwrap();
    let wd = WorkDir::new(tmp.path());

    // ---- 1. prepare: canonical types and modes ---------------------
    let m = rewrite_and_build(&wd, &source_tree());
    assert_eq!(m.shards.len(), 1);
    let shard = &m.shards[0];
    let shard_path = wd.shard_path(&shard.path);
    let rows = read_rows(&shard_path);
    let canonical: Vec<(&[u8], FileTypeTag, u32)> = rows
        .iter()
        .map(|r| (r.path.as_slice(), r.file_type, r.mode))
        .collect();
    assert_eq!(
        canonical,
        [
            (FILE, FileTypeTag::Regular, S_IFREG | 0o644),
            (FIFO, FileTypeTag::Fifo, S_IFIFO | 0o600),
            (SOCK, FileTypeTag::Socket, S_IFSOCK | 0o660),
            (BLK, FileTypeTag::BlockDev, S_IFBLK | 0o640),
            (CHR, FileTypeTag::CharDev, S_IFCHR | 0o620),
        ],
        "no special node is turned into a regular file"
    );

    // ---- 2 and 3. copy: one file created, four omissions -----------
    let copied = copy_shard(shard.file_name(), &shard_path).await;
    assert_eq!(
        copied.created,
        [FILE.to_vec()],
        "only the regular file is copied"
    );
    let o = &copied.outcome;
    assert_eq!(o.rows_total, 5);
    assert_eq!(o.files_ok, 1, "one success, not five");
    assert_eq!(o.files_special_not_copied, 4);
    assert_eq!(o.files_failed, 0, "an omission is not a failure");
    assert_eq!(o.files_fenced, 0);
    assert_eq!(o.bytes_moved, 11, "special nodes move no bytes");
    assert_eq!(
        o.rows_processed(),
        o.rows_total,
        "every row is accounted for"
    );
    assert!(!o.interrupted && !o.fenced);
    assert!(
        copied.failures_jsonl.is_empty(),
        "nothing for a retry to pick up"
    );
    // The live counters the heartbeat and ticker read agree.
    assert_eq!(copied.live.rows_done.load(Relaxed), 5);
    assert_eq!(copied.live.files_ok.load(Relaxed), 1);
    assert_eq!(copied.live.files_special_not_copied.load(Relaxed), 4);
    assert_eq!(copied.live.files_failed.load(Relaxed), 0);
    assert_eq!(copied.live.bytes_moved.load(Relaxed), 11);

    // ---- 4. durable records, persisted the way the copy loop does --
    let mut progress = CopyProgress::load_or_fresh(&wd, &m.run_id, m.shards.len() as u64).unwrap();
    let (failure_file, downgrade_file, ()) = persist_shard_results_then(
        &wd.failures_dir(),
        &wd.downgrades_dir(),
        shard.stem(),
        &copied.failures_jsonl,
        &copied.downgrades_jsonl,
        write_shard_jsonl,
        || {
            progress.record_shard(&shard.path, &copied.outcome, 0.0);
            progress.write(&wd)
        },
    )
    .unwrap();
    assert_eq!(failure_file, None);
    let downgrade_file =
        downgrade_file.expect("the omissions are on disk before the shard commits");
    assert!(downgrade_file.starts_with(wd.downgrades_dir()));

    let mut records = parse_downgrades(&std::fs::read(&downgrade_file).unwrap());
    records.sort_by_key(|r| r.row_id);
    let got: Vec<(Vec<u8>, DowngradeKind)> = records
        .iter()
        .map(|r| (record_path(r), r.downgrade))
        .collect();
    let special = |node| DowngradeKind::SpecialNotCopied { node };
    assert_eq!(
        got,
        [
            (FIFO.to_vec(), special(SpecialNode::Fifo)),
            (SOCK.to_vec(), special(SpecialNode::Socket)),
            (BLK.to_vec(), special(SpecialNode::BlockDev)),
            (CHR.to_vec(), special(SpecialNode::CharDev)),
        ],
        "one record per omitted entry, paths byte for byte (the fifo's is not UTF-8)"
    );
    for (record, row) in records.iter().zip(&rows[1..]) {
        assert_eq!(record.row_id, row.row_id);
        assert_eq!(record.shard, shard.file_name());
    }

    // ---- 5. the shard is complete; a re-run does not retry it ------
    let resumed = CopyProgress::load_or_fresh(&wd, &m.run_id, m.shards.len() as u64).unwrap();
    assert!(
        resumed.is_completed(&shard.path),
        "the copy loop skips a completed shard"
    );
    assert_eq!(resumed.files_ok, 1);
    assert_eq!(resumed.files_special_not_copied, 4);
    assert_eq!(resumed.files_failed, 0);
    assert_eq!(resumed.bytes_moved, 11);

    let mut summary = CopySummary {
        shards_total: 1,
        shards_done: 1,
        ..CopySummary::default()
    };
    summary.add_shard(&copied.outcome);
    assert_eq!(
        summary.headline(),
        "complete: 1/1 shards, 1 files ok (0 torn), 0 failed, 4 special NOT copied, \
         11 bytes moved"
    );
    let notice = special_not_copied_notice(summary.files_special_not_copied, &wd.downgrades_dir());
    assert!(
        notice.contains("4 fifo, socket, or device entries were NOT copied"),
        "{notice}"
    );
    assert!(
        notice.contains(&wd.downgrades_dir().display().to_string()),
        "{notice}"
    );
    assert!(notice.contains("stays blocked"), "{notice}");

    // ---- 6. cutover: four special_not_copied, and it fails ---------
    let dest_after_copy = vec![source_tree()[0].clone()];
    let report = cutover_verify(tmp.path(), "after-copy", &m, &dest_after_copy).await;
    assert_eq!(report.status, VerifyStatus::Fail, "cutover must not pass");
    assert_eq!(report.mismatches_total, 4);
    assert_eq!(count(&report, MismatchKind::SpecialNotCopied), 4);
    assert_eq!(report.namespace.special_not_copied, 4);
    assert_eq!(report.namespace.matched, 1, "the regular file");
    assert_eq!(
        mismatch_paths(tmp.path(), "after-copy", MismatchKind::SpecialNotCopied),
        [BLK.to_vec(), CHR.to_vec(), FIFO.to_vec(), SOCK.to_vec()],
        "raw paths, the non-UTF-8 one included"
    );

    // ---- 7. the operator recreates one node, and gets one wrong ----
    // The fifo now exists with the same type, mode, and owner; a
    // regular file sits where the socket should be.
    let mut dest_fixed = dest_after_copy.clone();
    dest_fixed.push(source_tree()[1].clone());
    dest_fixed.push(WalkerRow {
        size: 3,
        ..wrow(SOCK, "file", 0o660, 202)
    });
    let report = cutover_verify(tmp.path(), "recreated", &m, &dest_fixed).await;
    assert_eq!(
        report.status,
        VerifyStatus::Fail,
        "two device nodes are still missing"
    );
    assert_eq!(count(&report, MismatchKind::SpecialNotCopied), 2);
    assert_eq!(
        mismatch_paths(tmp.path(), "recreated", MismatchKind::SpecialNotCopied),
        [BLK.to_vec(), CHR.to_vec()],
        "the recreated fifo is no longer reported"
    );
    assert_eq!(count(&report, MismatchKind::FileType), 1);
    assert_eq!(
        mismatch_paths(tmp.path(), "recreated", MismatchKind::FileType),
        [SOCK.to_vec()],
        "a wrong type is a file_type mismatch, not special_not_copied"
    );
    assert_eq!(report.mismatches_total, 3);
    assert_eq!(report.namespace.matched, 2, "the file and the fifo");
    assert_eq!(
        report.content.files_read, 1,
        "only the source's regular file is read back; the stray file at the socket path is not"
    );

    // Every node recreated with the right type: the trees match.
    let mut dest_complete = dest_after_copy;
    dest_complete.extend(source_tree()[1..].iter().cloned());
    let report = cutover_verify(tmp.path(), "complete", &m, &dest_complete).await;
    assert_eq!(report.status, VerifyStatus::Pass);
    assert_eq!(report.mismatches_total, 0);
    assert_eq!(report.namespace.matched, 5);
}

/// A special node with more than one link, in a filesystem-scoped
/// hardlink group: every row is its own omission. Nothing is created
/// for the later rows to link against, so none of them may be sent to
/// the hardlink path, where it would fail.
#[tokio::test]
async fn hardlinked_special_nodes_are_each_an_omission_not_a_failed_link() {
    let tmp = tempfile::tempdir().unwrap();
    let wd = WorkDir::new(tmp.path());
    let link = |path: &'static [u8]| WalkerRow {
        nlink: 2,
        ..wrow(path, "fifo", 0o600, 777)
    };
    let m = rewrite_and_build(&wd, &[link(b"/a/pipe"), link(b"/b/pipe-link")]);
    let shard = &m.shards[0];
    let shard_path = wd.shard_path(&shard.path);
    let rows = read_rows(&shard_path);
    assert!(rows
        .iter()
        .all(|r| r.nlink == Some(2) && r.inode == Some(777) && r.fsid == Some(9)));

    let copied = copy_shard(shard.file_name(), &shard_path).await;

    assert_eq!(copied.outcome.files_special_not_copied, 2);
    assert_eq!(
        copied.outcome.files_failed, 0,
        "no link against a node that was never created"
    );
    assert_eq!(copied.outcome.files_ok, 0);
    assert!(copied.failures_jsonl.is_empty());
    assert!(copied.created.is_empty());
    let mut paths: Vec<Vec<u8>> = parse_downgrades(&copied.downgrades_jsonl)
        .iter()
        .map(record_path)
        .collect();
    paths.sort();
    assert_eq!(paths, [b"/a/pipe".to_vec(), b"/b/pipe-link".to_vec()]);
}
