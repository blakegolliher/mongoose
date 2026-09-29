//! Compatibility check for canonical shards written by the parquet 54
//! build.
//!
//! Work directories from earlier builds hold canonical shards that
//! `ShardReader` must still load after a parquet upgrade: a resumed
//! copy or a later sync pass reads them without rewriting them. The
//! fixture is a real canonical shard produced by the production writer
//! (`mig-walker-rewrite`'s `rewrite_shard`, ZSTD with the contract KV
//! footer) from that crate's `synthetic_walker_batch("/src-test")`
//! test input, as shard 3 with `walker_version = "fixture"`, at mongoose
//! `e422bc0` with parquet 54.3.1.
//!
//! Never regenerate the fixture: a file written by the current parquet
//! version would no longer test anything. The `created_by` assertion
//! below fails if that happens.

use migration_core::schema::FileTypeTag;
use migration_core::shard::ShardReader;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::basic::Compression;
use std::path::PathBuf;

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/canonical-shard-parquet54.parquet")
}

#[test]
fn fixture_was_written_by_parquet_54() {
    let builder =
        ParquetRecordBatchReaderBuilder::try_new(std::fs::File::open(fixture()).unwrap()).unwrap();
    let metadata = builder.metadata();
    assert_eq!(
        metadata.file_metadata().created_by(),
        Some("parquet-rs version 54.3.1")
    );
    for column in metadata.row_group(0).columns() {
        assert!(
            matches!(column.compression(), Compression::ZSTD(_)),
            "{} is {:?}, expected ZSTD",
            column.column_path(),
            column.compression()
        );
    }
}

#[test]
fn shard_reader_loads_a_parquet_54_canonical_shard() {
    let reader = ShardReader::open(&fixture()).expect("ShardReader::open");
    assert_eq!(reader.rows(), 3);
    assert_eq!(reader.shard_index(), Some(3));

    let rows: Vec<_> = reader
        .into_rows()
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let base = 3u64 << 40;
    let actual: Vec<_> = rows
        .iter()
        .map(|r| {
            (
                r.row_id,
                r.path.as_slice(),
                r.size,
                r.mode,
                r.file_type,
                (r.mtime_sec, r.mtime_nsec),
                (r.atime_sec, r.atime_nsec),
                (r.uid, r.gid, r.nlink, r.inode, r.fsid),
                (r.xattr_blob.is_none(), r.symlink_target.is_none()),
            )
        })
        .collect();
    assert_eq!(
        actual,
        vec![
            (
                base,
                b"/".as_slice(),
                4096,
                0o040755,
                FileTypeTag::Dir,
                (Some(1_700_000_000), Some(1_000)),
                (Some(1_700_000_000), Some(10_000)),
                (Some(1000), Some(1000), Some(2), Some(100), None),
                (true, true),
            ),
            (
                base + 1,
                b"/file.bin".as_slice(),
                1234,
                0o100644,
                FileTypeTag::Regular,
                (Some(1_700_000_000), Some(2_000)),
                (None, None),
                (Some(1000), Some(1000), Some(1), Some(101), None),
                (true, true),
            ),
            (
                base + 2,
                b"/link".as_slice(),
                0,
                0o120777,
                FileTypeTag::Symlink,
                (None, None),
                (Some(-1), Some(998_500_000)),
                (Some(1000), Some(1000), Some(1), Some(102), None),
                (true, true),
            ),
        ]
    );
}
