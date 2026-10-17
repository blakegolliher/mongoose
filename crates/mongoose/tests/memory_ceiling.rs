//! Peak memory must depend on mongoose's settings, never on the size of
//! the tree (`docs/work-items/BOUNDED_MEMORY.md`, step 8).
//!
//! A synthetic tree of 2,001,000 entries, about half of them
//! directories, goes through every phase that once held something per
//! entry, with no NFS:
//!
//! - **index:** the walker part is rewritten into one canonical shard
//!   and the manifest is built.
//! - **classify:** sync's join of the index against itself.
//! - **namespace:** the cutover's join of the index against itself.
//! - **content:** the cutover's read-back, with one file read slowly.
//! - **copy:** the shard goes through the shard processor and the copy
//!   loop's result persistence, with a downgrade for every file.
//! - **pending:** sync loads its pending set from those downgrades.
//!
//! The joins run with one bucket, so the whole index lands in it: about
//! what a 500M-entry tree puts in each of the 256 buckets mongoose uses.
//!
//! This binary's allocator counts the bytes held on the heap. Each
//! phase's peak above what was held when it started must stay under its
//! ceiling: about twice what the phase takes now, and a fraction of
//! what it took on this tree before it was bounded. The figures are in
//! the plan's step 8.

use async_trait::async_trait;
use migration_core::fence::Fence;
use migration_core::records::{DowngradeKind, Endpoint, EndpointKind, MigrationOptions};
use migration_core::schema::FileTypeTag;
use migration_core::shard::RowView;
use migration_mover::batch::{BatchBudget, InflightLimiter, InflightProfile};
use migration_mover::strategy::Strategy;
use migration_mover::{DowngradeSink, FailureSink, FileMover, MoveOutcome};
use migration_worker::events::EventEmitter;
use migration_worker::heartbeat::LivePending;
use migration_worker::shard_processor::ShardProcessor;
use migration_worker::throughput::ThroughputCounter;
use mongoose::manifest::{self, LocalManifest};
use mongoose::progress::{persist_shard_results_then, place_shard_jsonl, spill_path};
use mongoose::verify::content::{CheckResult, ContentChecker, TodoEntry};
use mongoose::verify::{self, Contract, VerifyParams, VerifyStatus};
use mongoose::workdir::WorkDir;
use nfs_walker::nfs::{DbEntry, EntryType};
use nfs_walker::parquet::{RowBuilder, RowContext};
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::properties::WriterProperties;
use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::HashSet;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// Top-level directories. Each holds `RUNS` directories of one file.
const PROJECTS: u64 = 1_000;
const RUNS: u64 = 1_000;
const FILES: u64 = PROJECTS * RUNS;
const ENTRIES: u64 = PROJECTS + 2 * FILES;
/// Rows per walker batch and row group.
const WALKER_BATCH: usize = 65_536;

const MB: usize = 1 << 20;

/// The system allocator, counting the bytes it holds for this binary
/// and the most it has held since [`measure`] last reset the peak.
struct Counting {
    live: AtomicUsize,
    peak: AtomicUsize,
}

impl Counting {
    fn grew(&self, bytes: usize) {
        let now = self.live.fetch_add(bytes, Relaxed) + bytes;
        self.peak.fetch_max(now, Relaxed);
    }
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = System.alloc(layout);
        if !p.is_null() {
            self.grew(layout.size());
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = System.alloc_zeroed(layout);
        if !p.is_null() {
            self.grew(layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout);
        self.live.fetch_sub(layout.size(), Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = System.realloc(ptr, layout, new_size);
        if !p.is_null() {
            if new_size >= layout.size() {
                self.grew(new_size - layout.size());
            } else {
                self.live.fetch_sub(layout.size() - new_size, Relaxed);
            }
        }
        p
    }
}

#[global_allocator]
static HEAP: Counting = Counting {
    live: AtomicUsize::new(0),
    peak: AtomicUsize::new(0),
};

/// Run `f` and return its result with the most heap it held above what
/// was held when it started.
fn measure<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let start = HEAP.live.load(Relaxed);
    HEAP.peak.store(start, Relaxed);
    let out = f();
    (out, HEAP.peak.load(Relaxed).saturating_sub(start))
}

/// Writes entries as the walker does, through its own row builder.
struct WalkerPart {
    rows: RowBuilder,
    out: ArrowWriter<File>,
    inode: u64,
}

impl WalkerPart {
    fn create(path: &Path) -> Self {
        let rows = RowBuilder::new(
            RowContext {
                scan_id: "memory-ceiling".into(),
                scan_timestamp_us: 1_700_000_000_000_000,
            },
            WALKER_BATCH,
        );
        let props = WriterProperties::builder()
            .set_compression(Compression::ZSTD(ZstdLevel::default()))
            .build();
        let out = ArrowWriter::try_new(
            File::create(path).unwrap(),
            rows.schema().clone(),
            Some(props),
        )
        .unwrap();
        Self {
            rows,
            out,
            inode: 1,
        }
    }

    fn push(&mut self, path: String, entry_type: EntryType) {
        let dir = entry_type == EntryType::Directory;
        self.inode += 1;
        // Sizes and times vary as a real scan's do, so the walker part
        // and the canonical shard compress like real ones.
        let h = mix(self.inode);
        // Never empty: an empty file has nothing to read back.
        let size = if dir { 4096 } else { 1 + h % (1 << 20) };
        let sec = 1_600_000_000 + (h >> 20) as i64 % 100_000_000;
        let nsec = Some(((h >> 8) % 1_000_000_000) as i32);
        self.rows
            .push_db_entry(&DbEntry {
                name: path.rsplit('/').next().unwrap().as_bytes().to_vec(),
                depth: path.matches('/').count() as u32,
                path: path.into_bytes(),
                entry_type,
                size,
                mode: Some(if dir { 0o755 } else { 0o644 }),
                uid: Some(1000 + (h % 7) as u32),
                gid: Some(1000),
                nlink: Some(if dir { 2 } else { 1 }),
                inode: self.inode,
                fsid: Some(1),
                mtime_sec: Some(sec),
                mtime_nsec: nsec,
                atime_sec: Some(sec + 3600),
                atime_nsec: nsec,
                ctime_sec: Some(sec),
                ctime_nsec: nsec,
                extension: (!dir).then(|| "dat".to_string()),
                blocks: size.div_ceil(512),
                ..DbEntry::default()
            })
            .unwrap();
        if self.rows.row_count() == WALKER_BATCH {
            self.flush();
        }
    }

    fn flush(&mut self) {
        let batch = self.rows.finish().unwrap();
        self.out.write(&batch).unwrap();
        self.out.flush().unwrap();
    }

    fn close(mut self) {
        if !self.rows.is_empty() {
            self.flush();
        }
        self.out.close().unwrap();
    }
}

/// splitmix64: spreads a counter into well-mixed bits.
fn mix(n: u64) -> u64 {
    let mut z = n.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// `/project-NNNN/run-NNNNN/sample-<hash>.dat`: wide, shallow, and as
/// many directories as files, the shape that grew the most.
fn write_tree(scan_dir: &Path) {
    std::fs::create_dir_all(scan_dir).unwrap();
    let mut part = WalkerPart::create(&scan_dir.join("part-r00-00000.parquet"));
    for p in 0..PROJECTS {
        let project = format!("/project-{p:04}");
        part.push(project.clone(), EntryType::Directory);
        for r in 0..RUNS {
            let run = format!("{project}/run-{r:05}");
            part.push(run.clone(), EntryType::Directory);
            let file = format!("{run}/sample-{:012x}.dat", mix(p * RUNS + r) >> 16);
            part.push(file, EntryType::File);
        }
    }
    part.close();
}

fn endpoint(url: &str) -> Endpoint {
    Endpoint {
        kind: EndpointKind::Nfs,
        url: url.into(),
        root: "/".into(),
    }
}

/// Rewrite the scan into `wd` and build its manifest, as `prepare` does.
fn index(wd: &WorkDir, scan_dir: &Path) -> LocalManifest {
    std::fs::create_dir_all(wd.canonical_dir()).unwrap();
    mig_walker_rewrite::run_rewrite(&mig_walker_rewrite::Cli {
        input: scan_dir.to_path_buf(),
        output: wd.canonical_dir(),
        source_root: "/".into(),
        walker_version: "memory-ceiling".into(),
        resume: true,
        report: Some(wd.rewrite_json()),
        verbose: false,
    })
    .unwrap();
    manifest::build(
        wd,
        "run-memory-ceiling",
        endpoint("nfs://s/e"),
        endpoint("nfs://d/e"),
        MigrationOptions::default(),
    )
    .unwrap()
}

/// Matches every entry without reading it, except the first one asked
/// for, which holds out until every other entry is done: the slow file
/// that once kept every later result in memory. It gives up waiting
/// once the others stop moving for a second, so a driver that waits on
/// it is slow here but never stuck.
struct OneSlowFile {
    started: AtomicBool,
    done: AtomicU64,
}

#[async_trait]
impl ContentChecker for OneSlowFile {
    async fn check(&self, _entry: &TodoEntry) -> CheckResult {
        if self.started.swap(true, Relaxed) {
            self.done.fetch_add(1, Relaxed);
            return CheckResult::Match;
        }
        let (mut seen, mut still) = (0, 0);
        while self.done.load(Relaxed) < FILES - 1 && still < 100 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let now = self.done.load(Relaxed);
            still = if now == seen { still + 1 } else { 0 };
            seen = now;
        }
        CheckResult::Match
    }
}

/// Copies nothing: every file succeeds with a downgrade recorded, as
/// when the destination cannot take the owner, and every directory is
/// stamped.
struct EveryFileDowngraded {
    downgrades: DowngradeSink,
}

#[async_trait]
impl FileMover for EveryFileDowngraded {
    async fn move_one(&self, row: &RowView) -> MoveOutcome {
        let dir = row.file_type == FileTypeTag::Dir;
        if !dir {
            self.downgrades
                .record(row.row_id, &row.path, DowngradeKind::NullOwner);
        }
        MoveOutcome {
            row_id: row.row_id,
            strategy: if dir {
                Strategy::DirAttrs
            } else {
                Strategy::LibnfsIoUring
            },
            bytes_moved: row.size,
            torn: false,
            result: Ok(()),
        }
    }

    async fn move_hardlink(&self, row: &RowView, _link_target: &[u8]) -> MoveOutcome {
        self.move_one(row).await
    }

    fn downgrade_sink(&self) -> &DowngradeSink {
        &self.downgrades
    }
}

/// Copy the index's one shard as `mongoose copy` does, through the
/// persistence of its results. Returns the files and directories done.
async fn copy(wd: &WorkDir, m: &LocalManifest) -> u64 {
    let shard = &m.shards[0];
    let downgrades = DowngradeSink::new();
    let failures = FailureSink::new();
    downgrades.set_current_shard(shard.file_name());
    failures.set_current_shard(shard.file_name());
    failures
        .spill_to(&spill_path(&wd.failures_dir(), shard.stem()).unwrap())
        .unwrap();
    downgrades
        .spill_to(&spill_path(&wd.downgrades_dir(), shard.stem()).unwrap())
        .unwrap();
    let mut processor = ShardProcessor {
        mover: Arc::new(EveryFileDowngraded {
            downgrades: downgrades.clone(),
        }),
        live: Arc::new(LivePending::default()),
        fence: Fence::new(),
        budget: BatchBudget::default(),
        inflight: InflightLimiter::new(&InflightProfile::default()),
        failures: failures.clone(),
        throughput: ThroughputCounter::new(),
        fsid_ungrouped_warned: false,
        emitter: EventEmitter::disabled(),
        run_control: None,
        stop: CancellationToken::new(),
    };
    let outcome = processor
        .process(&wd.shard_path(&shard.path))
        .await
        .unwrap();
    assert_eq!(outcome.files_failed, 0);
    let (failed, downgraded, ()) = persist_shard_results_then(
        &wd.failures_dir(),
        &wd.downgrades_dir(),
        shard.stem(),
        failures.finish_spill().unwrap(),
        downgrades.finish_spill().unwrap(),
        place_shard_jsonl,
        || Ok(()),
    )
    .unwrap();
    assert!(failed.is_none());
    assert!(downgraded.is_some());
    outcome.files_ok
}

struct Phase {
    name: &'static str,
    peak: usize,
    ceiling: usize,
    took: Duration,
}

/// Run one phase, recording its peak against its ceiling.
fn phase<T>(
    phases: &mut Vec<Phase>,
    name: &'static str,
    ceiling_mb: usize,
    f: impl FnOnce() -> T,
) -> T {
    let started = Instant::now();
    let (out, peak) = measure(f);
    phases.push(Phase {
        name,
        peak,
        ceiling: ceiling_mb * MB,
        took: started.elapsed(),
    });
    out
}

#[test]
fn no_phase_holds_memory_for_each_entry() {
    let tmp = tempfile::tempdir().unwrap();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let scan_dir = tmp.path().join("scan");
    write_tree(&scan_dir);
    let wd = WorkDir::new(tmp.path().join("job"));
    let mut phases = Vec::new();

    let m = phase(&mut phases, "index", 192, || index(&wd, &scan_dir));
    assert_eq!(m.total_rows, ENTRIES);
    let shards: Vec<PathBuf> = m.shards.iter().map(|s| wd.shard_path(&s.path)).collect();

    let counts = phase(&mut phases, "classify", 160, || {
        migration_resync::classify(
            &shards,
            &shards,
            &HashSet::new(),
            &tmp.path().join("classify"),
            1,
        )
        .unwrap()
    });
    assert_eq!(counts.unchanged, ENTRIES);

    let pass_wd = WorkDir::new(wd.pass_dir(1));
    std::fs::create_dir_all(pass_wd.root()).unwrap();
    // More room than the others: the join's map doubles as it fills, and
    // where the last doubling falls depends on the entries' sizes.
    let ns = phase(&mut phases, "namespace", 256, || {
        verify::namespace::ensure(
            &pass_wd,
            &shards,
            &shards,
            Contract::from_options(&m.options),
            1,
        )
        .unwrap()
    });
    assert_eq!(ns.counts.matched, ENTRIES);
    assert_eq!(ns.counts.files_to_read, FILES);
    let params = VerifyParams {
        run_id: m.run_id.clone(),
        pass: 1,
        source: m.source.clone(),
        dest: m.dest.clone(),
        options: m.options.clone(),
        source_shards: shards.clone(),
        dest_shards: shards.clone(),
        concurrency: 32,
        buckets: 1,
    };
    let checker = Arc::new(OneSlowFile {
        started: AtomicBool::new(false),
        done: AtomicU64::new(0),
    });
    // The namespace checkpoint is reused, so this is the read-back.
    let report = phase(&mut phases, "content", 32, || {
        rt.block_on(verify::run(
            &pass_wd,
            &params,
            checker,
            CancellationToken::new(),
        ))
        .unwrap()
    });
    assert_eq!(report.status, VerifyStatus::Pass);
    assert_eq!(report.content.files_read, FILES);

    let done = phase(&mut phases, "copy", 128, || rt.block_on(copy(&wd, &m)));
    assert_eq!(done, ENTRIES);

    let pending = phase(&mut phases, "pending", 32, || {
        mongoose::sync::collect_pending(&wd).unwrap()
    });
    assert!(pending.is_empty());

    let table: String = phases
        .iter()
        .map(|p| {
            format!(
                "  {:<9} peak {:>6.1} MB  ceiling {:>4} MB  {:>6.1} s\n",
                p.name,
                p.peak as f64 / MB as f64,
                p.ceiling / MB,
                p.took.as_secs_f64(),
            )
        })
        .collect();
    println!("{ENTRIES} entries\n{table}");
    let over: Vec<&str> = phases
        .iter()
        .filter(|p| p.peak > p.ceiling)
        .map(|p| p.name)
        .collect();
    assert!(
        over.is_empty(),
        "over the ceiling: {}\n{table}",
        over.join(", ")
    );
}
