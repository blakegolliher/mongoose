//! Phase 1 of cutover verification: join the source and destination
//! canonical indexes by path and compare everything the contract can
//! see without reading file bytes.
//!
//! Same mechanics as the resync classifier (`migration-resync`):
//! neither index is globally sorted, so both stream once into B
//! path-hash buckets; per bucket the source side loads into a map and
//! the destination side streams against it. Peak memory is
//! ~|source|/B entries.
//!
//! Outputs, under `<pass>/verify/`:
//!
//! - `mismatches.jsonl` — one [`Mismatch`] per line; the content
//!   phase appends to it later.
//! - `content-todo.bin` — the files and symlinks whose bytes/targets
//!   must be read back from both servers.
//! - `namespace.json` — checkpoint with [`NamespaceCounts`]; a
//!   complete one is reused on resume so the join never runs twice
//!   for one pass.

use super::content::{TodoEntry, TodoKind, TodoWriter, TODO_FILE};
use super::{Contract, Mismatch, MismatchKind, MISMATCHES_FILE};
use crate::util::{read_json_opt, write_json_atomic};
use crate::workdir::WorkDir;
use anyhow::{Context, Result};
use migration_core::schema::FileTypeTag;
use migration_core::shard::{RowView, ShardReader};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

/// Checkpoint file name under `verify/`.
pub const CHECKPOINT: &str = "namespace.json";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamespaceCounts {
    pub source_entries: u64,
    pub dest_entries: u64,
    /// Paths present on both sides with the same file type.
    pub matched: u64,
    pub missing: u64,
    pub extra: u64,
    pub special_not_copied: u64,
    pub file_type: u64,
    pub size: u64,
    pub mode: u64,
    pub owner: u64,
    pub mtime: u64,
    /// Every mismatch record this phase wrote.
    pub mismatches: u64,
    pub files_to_read: u64,
    pub bytes_to_read: u64,
    pub symlinks_to_read: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct NamespaceCheckpoint {
    complete: bool,
    counts: NamespaceCounts,
    mismatch_file_len: u64,
}

/// What [`ensure`] hands to the content phase.
#[derive(Debug, Clone)]
pub struct NamespaceOutcome {
    pub counts: NamespaceCounts,
    /// Byte length of `mismatches.jsonl` when this phase finished —
    /// where the content phase's appends begin.
    pub mismatch_file_len: u64,
}

/// One index entry: the subset of a canonical row the contract reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: Vec<u8>,
    pub file_type: FileTypeTag,
    pub size: u64,
    pub mode: u32,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub mtime: Option<(i64, i32)>,
}

impl From<RowView> for Entry {
    fn from(r: RowView) -> Self {
        Self {
            path: r.path,
            file_type: r.file_type,
            size: r.size,
            mode: r.mode,
            uid: r.uid,
            gid: r.gid,
            mtime: r.mtime_sec.map(|s| (s, r.mtime_nsec.unwrap_or(0))),
        }
    }
}

/// Result of comparing one path present on both sides.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Compared {
    pub mismatches: Vec<Mismatch>,
    /// Content still to be read back, when the metadata allows a
    /// match at all (same type; same size for files).
    pub todo: Option<TodoKind>,
}

pub fn type_name(t: FileTypeTag) -> &'static str {
    match t {
        FileTypeTag::Regular => "file",
        FileTypeTag::Dir => "dir",
        FileTypeTag::Symlink => "symlink",
        FileTypeTag::Fifo => "fifo",
        FileTypeTag::Socket => "socket",
        FileTypeTag::BlockDev => "blockdev",
        FileTypeTag::CharDev => "chardev",
        FileTypeTag::Unknown => "unknown",
    }
}

/// Fifo, socket, or device node: the mover's `Strategy::Skip`.
pub fn is_special(t: FileTypeTag) -> bool {
    matches!(
        t,
        FileTypeTag::Fifo | FileTypeTag::Socket | FileTypeTag::BlockDev | FileTypeTag::CharDev
    )
}

/// The contract, applied to one path present on both sides. Pure.
///
/// - A file type mismatch is reported alone: nothing else about the
///   two entries is comparable.
/// - Files: size, then mode/owner/mtime per the contract; content is
///   queued only when the sizes agree (a metadata mismatch does not
///   suppress the content check, so one run reports everything).
/// - Directories and special files: mode and owner. Directory mtimes
///   are outside the contract (see the module docs).
/// - Symlinks: the target is queued for READLINK; NFSv3 cannot set a
///   symlink's own mode/owner/times, so none is compared.
pub fn compare(src: &Entry, dst: &Entry, c: Contract) -> Compared {
    let mut out = Compared::default();
    if src.file_type != dst.file_type {
        out.mismatches.push(
            Mismatch::new(&src.path, MismatchKind::FileType)
                .expected(type_name(src.file_type))
                .actual(type_name(dst.file_type)),
        );
        return out;
    }
    match src.file_type {
        FileTypeTag::Regular => {
            if src.size != dst.size {
                out.mismatches.push(
                    Mismatch::new(&src.path, MismatchKind::Size)
                        .expected(src.size.to_string())
                        .actual(dst.size.to_string()),
                );
            }
            compare_attrs(src, dst, c, true, &mut out.mismatches);
            if src.size == dst.size && src.size > 0 {
                out.todo = Some(TodoKind::File { size: src.size });
            }
        }
        FileTypeTag::Symlink => out.todo = Some(TodoKind::Symlink),
        _ => compare_attrs(src, dst, c, false, &mut out.mismatches),
    }
    out
}

fn compare_attrs(src: &Entry, dst: &Entry, c: Contract, with_mtime: bool, out: &mut Vec<Mismatch>) {
    if c.mode && (src.mode & 0o7777) != (dst.mode & 0o7777) {
        out.push(
            Mismatch::new(&src.path, MismatchKind::Mode)
                .expected(format!("{:04o}", src.mode & 0o7777))
                .actual(format!("{:04o}", dst.mode & 0o7777)),
        );
    }
    if c.owner && (src.uid.is_some() || src.gid.is_some()) {
        // A null source id was never copied (NullOwner downgrade), so
        // it cannot be wrong on the destination.
        let uid_ok = src.uid.is_none() || src.uid == dst.uid;
        let gid_ok = src.gid.is_none() || src.gid == dst.gid;
        if !(uid_ok && gid_ok) {
            out.push(
                Mismatch::new(&src.path, MismatchKind::Owner)
                    .expected(owner_str(src.uid, src.gid))
                    .actual(owner_str(dst.uid, dst.gid)),
            );
        }
    }
    if with_mtime && c.mtime {
        if let Some(s) = src.mtime {
            // utimes carries microseconds (SETATTR via timeval), so the
            // nanosecond column can only match to that precision.
            let want = to_us(s);
            let have = dst.mtime.map(to_us);
            if have != Some(want) {
                out.push(
                    Mismatch::new(&src.path, MismatchKind::Mtime)
                        .expected(fmt_us(want))
                        .actual(have.map(fmt_us).unwrap_or_else(|| "-".into())),
                );
            }
        }
    }
}

fn to_us((sec, nsec): (i64, i32)) -> (i64, i32) {
    (sec, nsec / 1000)
}

fn fmt_us((sec, us): (i64, i32)) -> String {
    format!("{sec}.{us:06}")
}

fn owner_str(uid: Option<u32>, gid: Option<u32>) -> String {
    let one = |v: Option<u32>| v.map(|n| n.to_string()).unwrap_or_else(|| "-".into());
    format!("{}:{}", one(uid), one(gid))
}

/// Run (or reuse) the namespace phase for one pass.
pub fn ensure(
    pass_wd: &WorkDir,
    source_shards: &[PathBuf],
    dest_shards: &[PathBuf],
    contract: Contract,
    buckets: usize,
) -> Result<NamespaceOutcome> {
    let vdir = pass_wd.verify_dir();
    let cp_path = vdir.join(CHECKPOINT);
    if let Some(cp) = read_json_opt::<NamespaceCheckpoint>(&cp_path)? {
        if cp.complete {
            println!("  namespace checkpoint valid; not re-joining the indexes");
            return Ok(NamespaceOutcome {
                counts: cp.counts,
                mismatch_file_len: cp.mismatch_file_len,
            });
        }
    }
    // Anything from a torn earlier attempt must not mix with this one.
    if vdir.exists() {
        std::fs::remove_dir_all(&vdir).with_context(|| format!("clearing {}", vdir.display()))?;
    }
    let tmp = vdir.join("tmp");
    std::fs::create_dir_all(&tmp).with_context(|| format!("creating {}", tmp.display()))?;

    let mismatches_path = pass_wd.root().join(MISMATCHES_FILE);
    let mut mism = BufWriter::new(
        File::create(&mismatches_path)
            .with_context(|| format!("creating {}", mismatches_path.display()))?,
    );
    let mut todo = TodoWriter::create(&vdir.join(TODO_FILE))?;
    let buckets = buckets.max(1);

    let mut counts = NamespaceCounts {
        source_entries: partition(source_shards, &tmp, "src", buckets)?,
        dest_entries: partition(dest_shards, &tmp, "dst", buckets)?,
        ..NamespaceCounts::default()
    };

    for b in 0..buckets {
        let src_path = tmp.join(format!("src-{b:04}.bin"));
        let dst_path = tmp.join(format!("dst-{b:04}.bin"));
        let mut map: HashMap<Vec<u8>, Entry> = HashMap::new();
        for e in EntryReader::open(&src_path)? {
            let e = e?;
            map.insert(e.path.clone(), e);
        }
        for e in EntryReader::open(&dst_path)? {
            let d = e?;
            match map.remove(&d.path) {
                None => {
                    counts.extra += 1;
                    write_mismatch(
                        &mut mism,
                        &mut counts,
                        Mismatch::new(&d.path, MismatchKind::Extra).actual(type_name(d.file_type)),
                    )?;
                }
                Some(s) => {
                    let cmp = compare(&s, &d, contract);
                    if s.file_type == d.file_type {
                        counts.matched += 1;
                    }
                    for m in cmp.mismatches {
                        match m.kind {
                            MismatchKind::FileType => counts.file_type += 1,
                            MismatchKind::Size => counts.size += 1,
                            MismatchKind::Mode => counts.mode += 1,
                            MismatchKind::Owner => counts.owner += 1,
                            MismatchKind::Mtime => counts.mtime += 1,
                            _ => {}
                        }
                        write_mismatch(&mut mism, &mut counts, m)?;
                    }
                    if let Some(kind) = cmp.todo {
                        match kind {
                            TodoKind::File { size } => {
                                counts.files_to_read += 1;
                                counts.bytes_to_read += size;
                            }
                            TodoKind::Symlink => counts.symlinks_to_read += 1,
                        }
                        todo.push(&TodoEntry { path: s.path, kind })?;
                    }
                }
            }
        }
        // Source leftovers: nothing at that path on the destination.
        let mut leftovers: Vec<Entry> = map.into_values().collect();
        leftovers.sort_by(|a, b| a.path.cmp(&b.path));
        for s in leftovers {
            let m = if is_special(s.file_type) {
                counts.special_not_copied += 1;
                Mismatch::new(&s.path, MismatchKind::SpecialNotCopied)
                    .expected(type_name(s.file_type))
                    .detail("mongoose does not copy fifos, sockets, or device nodes")
            } else {
                counts.missing += 1;
                Mismatch::new(&s.path, MismatchKind::Missing).expected(type_name(s.file_type))
            };
            write_mismatch(&mut mism, &mut counts, m)?;
        }
        let _ = std::fs::remove_file(&src_path);
        let _ = std::fs::remove_file(&dst_path);
    }

    mism.flush()?;
    let file = mism.into_inner().context("flushing mismatches.jsonl")?;
    file.sync_all()?;
    let mismatch_file_len = file.metadata()?.len();
    todo.finish()?;
    let _ = std::fs::remove_dir_all(&tmp);

    write_json_atomic(
        &cp_path,
        &NamespaceCheckpoint {
            complete: true,
            counts: counts.clone(),
            mismatch_file_len,
        },
    )?;
    tracing::info!(
        source = counts.source_entries,
        dest = counts.dest_entries,
        matched = counts.matched,
        mismatches = counts.mismatches,
        files_to_read = counts.files_to_read,
        symlinks_to_read = counts.symlinks_to_read,
        "namespace verification complete",
    );
    Ok(NamespaceOutcome {
        counts,
        mismatch_file_len,
    })
}

fn write_mismatch(w: &mut impl Write, counts: &mut NamespaceCounts, m: Mismatch) -> Result<()> {
    serde_json::to_writer(&mut *w, &m)?;
    w.write_all(b"\n")?;
    counts.mismatches += 1;
    Ok(())
}

// ---------------------------------------------------------------------
// Partitioning
// ---------------------------------------------------------------------

/// Stream every shard of one side into its bucket files. Returns the
/// entry count seen.
fn partition(shards: &[PathBuf], tmp: &Path, tag: &str, buckets: usize) -> Result<u64> {
    let mut writers: Vec<BufWriter<File>> = (0..buckets)
        .map(|b| {
            let p = tmp.join(format!("{tag}-{b:04}.bin"));
            Ok(BufWriter::new(
                File::create(&p).with_context(|| format!("creating {}", p.display()))?,
            ))
        })
        .collect::<Result<_>>()?;
    let mut n = 0u64;
    for shard in shards {
        let reader =
            ShardReader::open(shard).with_context(|| format!("opening {}", shard.display()))?;
        let rows = reader
            .into_rows()
            .with_context(|| format!("reading {}", shard.display()))?;
        for row in rows {
            let row = row.with_context(|| format!("reading {}", shard.display()))?;
            let e = Entry::from(row);
            let b = (fnv1a64(&e.path) % buckets as u64) as usize;
            write_entry(&mut writers[b], &e)?;
            n += 1;
        }
    }
    for mut w in writers {
        w.flush()?;
    }
    Ok(n)
}

/// FNV-1a over path bytes: stable across runs, unlike SipHash.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

const FLAG_UID: u8 = 1 << 0;
const FLAG_GID: u8 = 1 << 1;
const FLAG_MTIME: u8 = 1 << 2;
/// Bytes after the path: type, flags, size, mode, uid, gid, mtime.
const FIXED_TAIL: usize = 1 + 1 + 8 + 4 + 4 + 4 + 8 + 4;

fn write_entry(w: &mut impl Write, e: &Entry) -> Result<()> {
    let mut flags = 0u8;
    let uid = e.uid.map_or(0, |v| {
        flags |= FLAG_UID;
        v
    });
    let gid = e.gid.map_or(0, |v| {
        flags |= FLAG_GID;
        v
    });
    let (mt_s, mt_n) = e.mtime.map_or((0, 0), |v| {
        flags |= FLAG_MTIME;
        v
    });
    w.write_all(&(e.path.len() as u32).to_le_bytes())?;
    w.write_all(&e.path)?;
    w.write_all(&[e.file_type as u8, flags])?;
    w.write_all(&e.size.to_le_bytes())?;
    w.write_all(&e.mode.to_le_bytes())?;
    w.write_all(&uid.to_le_bytes())?;
    w.write_all(&gid.to_le_bytes())?;
    w.write_all(&mt_s.to_le_bytes())?;
    w.write_all(&mt_n.to_le_bytes())?;
    Ok(())
}

struct EntryReader {
    r: BufReader<File>,
    path: PathBuf,
}

impl EntryReader {
    fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            r: BufReader::new(
                File::open(path).with_context(|| format!("opening {}", path.display()))?,
            ),
            path: path.to_path_buf(),
        })
    }

    fn read_body(&mut self, path_len: usize) -> Result<Entry> {
        let mut path = vec![0u8; path_len];
        self.r.read_exact(&mut path)?;
        let mut t = [0u8; FIXED_TAIL];
        self.r
            .read_exact(&mut t)
            .with_context(|| format!("torn entry in {}", self.path.display()))?;
        let flags = t[1];
        let le_u32 = |o: usize| u32::from_le_bytes(t[o..o + 4].try_into().unwrap());
        Ok(Entry {
            path,
            file_type: FileTypeTag::from_u8(t[0]),
            size: u64::from_le_bytes(t[2..10].try_into().unwrap()),
            mode: le_u32(10),
            uid: (flags & FLAG_UID != 0).then(|| le_u32(14)),
            gid: (flags & FLAG_GID != 0).then(|| le_u32(18)),
            mtime: (flags & FLAG_MTIME != 0).then(|| {
                (
                    i64::from_le_bytes(t[22..30].try_into().unwrap()),
                    i32::from_le_bytes(t[30..34].try_into().unwrap()),
                )
            }),
        })
    }
}

impl Iterator for EntryReader {
    type Item = Result<Entry>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut len = [0u8; 4];
        match self.r.read_exact(&mut len) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return None,
            Err(e) => {
                return Some(Err(e).with_context(|| format!("reading {}", self.path.display())))
            }
        }
        Some(self.read_body(u32::from_le_bytes(len) as usize))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::verify::content::TodoReader;
    use arrow::array::{
        ArrayRef, BinaryBuilder, Int32Builder, Int64Builder, UInt32Builder, UInt64Builder,
        UInt8Builder,
    };
    use arrow::record_batch::RecordBatch;
    use migration_core::schema::{self, make_row_id};
    use parquet::arrow::ArrowWriter;
    use parquet::file::properties::WriterProperties;
    use parquet::format::KeyValue;
    use std::sync::Arc;

    pub(crate) fn entry(path: &[u8], file_type: FileTypeTag, size: u64) -> Entry {
        Entry {
            path: path.to_vec(),
            file_type,
            size,
            mode: 0o100644,
            uid: Some(1000),
            gid: Some(1000),
            mtime: Some((1_700_000_000, 123_456_789)),
        }
    }

    fn file(path: &[u8], size: u64) -> Entry {
        entry(path, FileTypeTag::Regular, size)
    }

    fn kinds(c: &Compared) -> Vec<MismatchKind> {
        c.mismatches.iter().map(|m| m.kind).collect()
    }

    /// Write a canonical shard (full schema + KV footer) the way the
    /// rewrite would, so `ShardReader` accepts it.
    pub(crate) fn write_canonical_shard(path: &Path, shard_idx: u32, rows: &[Entry]) {
        let schema_arc = schema::canonical_schema();
        let mut row_id = UInt64Builder::new();
        let mut p = BinaryBuilder::new();
        let mut size = UInt64Builder::new();
        let mut mt_s = Int64Builder::new();
        let mut mt_n = Int32Builder::new();
        let mut at_s = Int64Builder::new();
        let mut at_n = Int32Builder::new();
        let mut mode = UInt32Builder::new();
        let mut uid = UInt32Builder::new();
        let mut gid = UInt32Builder::new();
        let mut nlink = UInt32Builder::new();
        let mut inode = UInt64Builder::new();
        let mut fsid = UInt64Builder::new();
        let mut xattr = BinaryBuilder::new();
        let mut symt = BinaryBuilder::new();
        let mut ft = UInt8Builder::new();
        for (i, r) in rows.iter().enumerate() {
            row_id.append_value(make_row_id(shard_idx, i as u64));
            p.append_value(&r.path);
            size.append_value(r.size);
            match r.mtime {
                Some((s, n)) => {
                    mt_s.append_value(s);
                    mt_n.append_value(n);
                }
                None => {
                    mt_s.append_null();
                    mt_n.append_null();
                }
            }
            at_s.append_null();
            at_n.append_null();
            mode.append_value(r.mode);
            uid.append_option(r.uid);
            gid.append_option(r.gid);
            nlink.append_value(1);
            inode.append_value(1000 + i as u64);
            fsid.append_null();
            xattr.append_null();
            symt.append_null();
            ft.append_value(r.file_type as u8);
        }
        let arrays: Vec<ArrayRef> = vec![
            Arc::new(row_id.finish()),
            Arc::new(p.finish()),
            Arc::new(size.finish()),
            Arc::new(mt_s.finish()),
            Arc::new(mt_n.finish()),
            Arc::new(at_s.finish()),
            Arc::new(at_n.finish()),
            Arc::new(mode.finish()),
            Arc::new(uid.finish()),
            Arc::new(gid.finish()),
            Arc::new(nlink.finish()),
            Arc::new(inode.finish()),
            Arc::new(fsid.finish()),
            Arc::new(xattr.finish()),
            Arc::new(symt.finish()),
            Arc::new(ft.finish()),
        ];
        let batch = RecordBatch::try_new(schema_arc.clone(), arrays).unwrap();
        let props = WriterProperties::builder()
            .set_key_value_metadata(Some(vec![
                KeyValue {
                    key: schema::KV_FORMAT_VERSION.into(),
                    value: Some(schema::FORMAT_VERSION.to_string()),
                },
                KeyValue {
                    key: schema::KV_SHARD_INDEX.into(),
                    value: Some(shard_idx.to_string()),
                },
            ]))
            .build();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut w =
            ArrowWriter::try_new(File::create(path).unwrap(), schema_arc, Some(props)).unwrap();
        w.write(&batch).unwrap();
        w.close().unwrap();
    }

    pub(crate) fn read_mismatches(pass_wd: &WorkDir) -> Vec<Mismatch> {
        let body = std::fs::read_to_string(pass_wd.root().join(MISMATCHES_FILE)).unwrap();
        body.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    // ---- compare(): the contract, one rule at a time -----------------

    #[test]
    fn identical_files_match_and_queue_content() {
        let a = file(b"/a", 10);
        let c = compare(&a, &a.clone(), Contract::strict());
        assert!(c.mismatches.is_empty());
        assert_eq!(c.todo, Some(TodoKind::File { size: 10 }));
    }

    #[test]
    fn empty_files_need_no_content_read() {
        let a = file(b"/a", 0);
        let c = compare(&a, &a.clone(), Contract::strict());
        assert!(c.mismatches.is_empty());
        assert_eq!(c.todo, None);
    }

    #[test]
    fn type_change_is_reported_alone() {
        let a = file(b"/a", 10);
        let mut d = a.clone();
        d.file_type = FileTypeTag::Dir;
        d.mode = 0o40755; // would also be a mode mismatch if compared
        let c = compare(&a, &d, Contract::strict());
        assert_eq!(kinds(&c), vec![MismatchKind::FileType]);
        assert_eq!(c.mismatches[0].expected.as_deref(), Some("file"));
        assert_eq!(c.mismatches[0].actual.as_deref(), Some("dir"));
        assert_eq!(c.todo, None);
    }

    #[test]
    fn size_change_skips_content_but_still_reports_attrs() {
        let a = file(b"/a", 10);
        let mut d = a.clone();
        d.size = 7;
        d.mode = 0o100600;
        let c = compare(&a, &d, Contract::strict());
        assert_eq!(kinds(&c), vec![MismatchKind::Size, MismatchKind::Mode]);
        assert_eq!(c.todo, None, "different sizes can never hash equal");
    }

    #[test]
    fn mode_compares_permission_bits_only() {
        let a = file(b"/a", 10);
        let mut d = a.clone();
        d.mode = 0o100644 | 0o1_000_000; // junk above S_IFMT is ignored
        assert!(compare(&a, &d, Contract::strict()).mismatches.is_empty());
        d.mode = 0o104644; // setuid bit is part of the contract
        let c = compare(&a, &d, Contract::strict());
        assert_eq!(kinds(&c), vec![MismatchKind::Mode]);
        assert_eq!(c.mismatches[0].expected.as_deref(), Some("0644"));
        assert_eq!(c.mismatches[0].actual.as_deref(), Some("4644"));
        assert_eq!(
            c.todo,
            Some(TodoKind::File { size: 10 }),
            "content still checked"
        );
    }

    #[test]
    fn owner_mismatch_and_null_source_owner() {
        let a = file(b"/a", 10);
        let mut d = a.clone();
        d.gid = Some(0);
        let c = compare(&a, &d, Contract::strict());
        assert_eq!(kinds(&c), vec![MismatchKind::Owner]);
        assert_eq!(c.mismatches[0].expected.as_deref(), Some("1000:1000"));
        assert_eq!(c.mismatches[0].actual.as_deref(), Some("1000:0"));

        // The copy never chowned a null source id (NullOwner downgrade),
        // so the destination value is not wrong.
        let mut s = a.clone();
        s.uid = None;
        s.gid = None;
        assert!(compare(&s, &d, Contract::strict()).mismatches.is_empty());

        // Destination lost the id entirely.
        let mut d2 = a.clone();
        d2.uid = None;
        let c = compare(&a, &d2, Contract::strict());
        assert_eq!(kinds(&c), vec![MismatchKind::Owner]);
        assert_eq!(c.mismatches[0].actual.as_deref(), Some("-:1000"));
    }

    #[test]
    fn mtime_compares_at_microseconds() {
        let a = file(b"/a", 10);
        let mut d = a.clone();
        d.mtime = Some((1_700_000_000, 123_456_000)); // ns truncated by utimes
        assert!(compare(&a, &d, Contract::strict()).mismatches.is_empty());
        d.mtime = Some((1_700_000_000, 123_457_000));
        let c = compare(&a, &d, Contract::strict());
        assert_eq!(kinds(&c), vec![MismatchKind::Mtime]);
        assert_eq!(
            c.mismatches[0].expected.as_deref(),
            Some("1700000000.123456")
        );
        assert_eq!(c.mismatches[0].actual.as_deref(), Some("1700000000.123457"));
        d.mtime = None;
        let c = compare(&a, &d, Contract::strict());
        assert_eq!(c.mismatches[0].actual.as_deref(), Some("-"));
        // Null source mtime was never applied (NullMtime downgrade).
        let mut s = a.clone();
        s.mtime = None;
        assert!(compare(&s, &d, Contract::strict()).mismatches.is_empty());
    }

    #[test]
    fn contract_switches_off_attrs_the_copy_never_preserved() {
        let a = file(b"/a", 10);
        let mut d = a.clone();
        d.mode = 0o100600;
        d.uid = Some(0);
        d.mtime = Some((1, 0));
        let off = Contract {
            mode: false,
            owner: false,
            mtime: false,
        };
        let c = compare(&a, &d, off);
        assert!(c.mismatches.is_empty());
        assert_eq!(
            c.todo,
            Some(TodoKind::File { size: 10 }),
            "content is never optional"
        );
    }

    #[test]
    fn directories_compare_mode_and_owner_but_not_mtime() {
        let mut a = entry(b"/d", FileTypeTag::Dir, 0);
        a.mode = 0o40755;
        let mut d = a.clone();
        d.mtime = Some((1, 0)); // bumped by later commits: outside the contract
        let c = compare(&a, &d, Contract::strict());
        assert!(c.mismatches.is_empty(), "{:?}", c.mismatches);
        assert_eq!(c.todo, None);
        d.mode = 0o40700;
        d.uid = Some(0);
        let c = compare(&a, &d, Contract::strict());
        assert_eq!(kinds(&c), vec![MismatchKind::Mode, MismatchKind::Owner]);
    }

    #[test]
    fn symlinks_queue_a_target_read_and_compare_no_attrs() {
        let mut a = entry(b"/l", FileTypeTag::Symlink, 0);
        a.mode = 0o120777;
        let mut d = a.clone();
        d.mode = 0o120755; // NFSv3 cannot set symlink mode
        d.uid = Some(0);
        d.mtime = None;
        let c = compare(&a, &d, Contract::strict());
        assert!(c.mismatches.is_empty(), "{:?}", c.mismatches);
        assert_eq!(c.todo, Some(TodoKind::Symlink));
    }

    #[test]
    fn special_files_present_on_both_sides_compare_attrs_only() {
        let a = entry(b"/fifo", FileTypeTag::Fifo, 0);
        let mut d = a.clone();
        d.mtime = None;
        let c = compare(&a, &d, Contract::strict());
        assert!(c.mismatches.is_empty());
        assert_eq!(c.todo, None);
    }

    // ---- ensure(): the join over real shards ---------------------------

    fn shards(dir: &Path, tag: &str, sides: &[Vec<Entry>]) -> Vec<PathBuf> {
        sides
            .iter()
            .enumerate()
            .map(|(i, rows)| {
                let p = dir.join(format!("{tag}-part-{i:04}.parquet"));
                write_canonical_shard(&p, i as u32, rows);
                p
            })
            .collect()
    }

    #[test]
    fn join_reports_every_namespace_class_and_queues_content() {
        let tmp = tempfile::tempdir().unwrap();
        let pass_wd = WorkDir::new(tmp.path().join("pass"));
        let mut dir_a = entry(b"/dir", FileTypeTag::Dir, 0);
        dir_a.mode = 0o40755;
        let link = entry(b"/link", FileTypeTag::Symlink, 0);
        let fifo = entry(b"/fifo", FileTypeTag::Fifo, 0);
        let mut small = file(b"/small", 5);
        small.mode = 0o100600;
        let src = vec![
            vec![
                file(b"/a", 10),
                file(b"/gone", 3),
                dir_a.clone(),
                link.clone(),
            ],
            vec![fifo, small.clone(), file(b"/empty", 0), file(b"/typed", 4)],
        ];
        let mut small_d = small.clone();
        small_d.mode = 0o100644;
        let mut typed_d = file(b"/typed", 4);
        typed_d.file_type = FileTypeTag::Dir;
        let dst = vec![vec![
            file(b"/a", 10),
            dir_a,
            link,
            small_d,
            file(b"/empty", 0),
            typed_d,
            file(b"/stray.partial", 1),
        ]];
        let s = shards(tmp.path(), "src", &src);
        let d = shards(tmp.path(), "dst", &dst);
        let out = ensure(&pass_wd, &s, &d, Contract::strict(), 3).unwrap();
        let c = &out.counts;
        assert_eq!(c.source_entries, 8);
        assert_eq!(c.dest_entries, 7);
        assert_eq!(c.matched, 5, "/a /dir /link /small /empty");
        assert_eq!(c.missing, 1, "/gone");
        assert_eq!(c.extra, 1, "/stray.partial");
        assert_eq!(c.special_not_copied, 1, "/fifo");
        assert_eq!(c.file_type, 1, "/typed");
        assert_eq!(c.mode, 1, "/small");
        assert_eq!(c.mismatches, 5);
        assert_eq!(
            c.files_to_read, 2,
            "/a and /small (mode mismatch still reads)"
        );
        assert_eq!(c.bytes_to_read, 15);
        assert_eq!(c.symlinks_to_read, 1);
        assert!(
            !pass_wd.verify_dir().join("tmp").exists(),
            "buckets cleaned up"
        );

        let recs = read_mismatches(&pass_wd);
        assert_eq!(recs.len(), 5);
        assert_eq!(
            out.mismatch_file_len,
            std::fs::metadata(pass_wd.root().join(MISMATCHES_FILE))
                .unwrap()
                .len()
        );
        let find = |k: MismatchKind| recs.iter().find(|m| m.kind == k).unwrap();
        assert_eq!(find(MismatchKind::Missing).path().unwrap(), b"/gone");
        assert_eq!(find(MismatchKind::Extra).path().unwrap(), b"/stray.partial");
        assert_eq!(
            find(MismatchKind::SpecialNotCopied).path().unwrap(),
            b"/fifo"
        );
        assert_eq!(find(MismatchKind::FileType).path().unwrap(), b"/typed");

        let mut todo: Vec<TodoEntry> = TodoReader::open(&pass_wd.verify_dir().join(TODO_FILE))
            .unwrap()
            .map(|e| e.unwrap())
            .collect();
        todo.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(
            todo,
            vec![
                TodoEntry {
                    path: b"/a".to_vec(),
                    kind: TodoKind::File { size: 10 }
                },
                TodoEntry {
                    path: b"/link".to_vec(),
                    kind: TodoKind::Symlink
                },
                TodoEntry {
                    path: b"/small".to_vec(),
                    kind: TodoKind::File { size: 5 }
                },
            ]
        );

        // A complete checkpoint is reused verbatim.
        let again = ensure(&pass_wd, &[], &[], Contract::strict(), 3).unwrap();
        assert_eq!(again.counts, out.counts);
    }

    #[test]
    fn identical_indexes_produce_no_mismatches() {
        let tmp = tempfile::tempdir().unwrap();
        let pass_wd = WorkDir::new(tmp.path().join("pass"));
        let rows: Vec<Entry> = (0..500)
            .map(|i| file(format!("/f/{i:04}").as_bytes(), i))
            .collect();
        let s = shards(tmp.path(), "src", std::slice::from_ref(&rows));
        let d = shards(tmp.path(), "dst", std::slice::from_ref(&rows));
        let out = ensure(&pass_wd, &s, &d, Contract::strict(), 16).unwrap();
        assert_eq!(out.counts.mismatches, 0);
        assert_eq!(out.counts.matched, 500);
        assert_eq!(
            out.counts.files_to_read, 499,
            "the size-0 file needs no read"
        );
        assert_eq!(read_mismatches(&pass_wd).len(), 0);
    }

    #[test]
    fn entry_encoding_round_trips_optionals_and_raw_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("b.bin");
        let mut with_none = file(b"/x\xff", 1);
        with_none.uid = None;
        with_none.mtime = None;
        let rows = vec![file(b"/a", 2), with_none.clone()];
        {
            let mut w = BufWriter::new(File::create(&p).unwrap());
            for r in &rows {
                write_entry(&mut w, r).unwrap();
            }
            w.flush().unwrap();
        }
        let back: Vec<Entry> = EntryReader::open(&p).unwrap().map(|e| e.unwrap()).collect();
        assert_eq!(back, rows);
    }

    #[test]
    fn torn_bucket_file_is_an_error_not_silence() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("b.bin");
        let mut w = BufWriter::new(File::create(&p).unwrap());
        write_entry(&mut w, &file(b"/a", 2)).unwrap();
        w.flush().unwrap();
        let mut bytes = std::fs::read(&p).unwrap();
        bytes.truncate(bytes.len() - 3);
        std::fs::write(&p, bytes).unwrap();
        let err = EntryReader::open(&p).unwrap().next().unwrap().unwrap_err();
        assert!(format!("{err:#}").contains("torn"), "{err:#}");
    }
}
