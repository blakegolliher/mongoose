//! Parquet index schema.
//!
//! This module is the single source of truth for the column names,
//! types, and ordering the mover expects to find in an index parquet
//! shard. The walker (`nfs-walker`) writes shards conforming to this
//! schema; the mover reads them.
//!
//! Authoritative spec lives in `SCHEMA_CONTRACT.md` at the workspace
//! root — vendored byte-identical in `nfs-walker`. When code disagrees
//! with the contract, the contract wins.

use arrow::datatypes::{DataType, Field, Schema};
use std::sync::Arc;

// =============================================================================
// Versioning. Bumped when SCHEMA_CONTRACT.md bumps.
// =============================================================================

/// Schema format version. Walker stamps `migration.format_version`
/// into the parquet KV footer; mover refuses any other value.
pub const FORMAT_VERSION: u32 = 1;

/// Operational contract version. Mismatch is a WARN, not an error —
/// the parquet schema can be unchanged while the contract document
/// gains a clarification.
pub const CONTRACT_VERSION: u32 = 1;

// =============================================================================
// Parquet KV footer key constants. See SCHEMA_CONTRACT.md
// "Parquet file metadata (KV footer)".
// =============================================================================

pub const KV_FORMAT_VERSION: &str = "migration.format_version";
pub const KV_CONTRACT_VERSION: &str = "migration.contract_version";
pub const KV_SHARD_INDEX: &str = "migration.shard_index";
pub const KV_WALKER_VERSION: &str = "migration.walker_version";
pub const KV_ROW_COUNT: &str = "migration.row_count";

// =============================================================================
// Column names — use these constants everywhere, never literal strings.
// =============================================================================

pub const COL_ROW_ID: &str = "row_id";
pub const COL_PATH: &str = "path";
pub const COL_SIZE: &str = "size";
pub const COL_MTIME_SEC: &str = "mtime_sec";
pub const COL_MTIME_NSEC: &str = "mtime_nsec";
pub const COL_ATIME_SEC: &str = "atime_sec";
pub const COL_ATIME_NSEC: &str = "atime_nsec";
pub const COL_MODE: &str = "mode";
pub const COL_UID: &str = "uid";
pub const COL_GID: &str = "gid";
pub const COL_NLINK: &str = "nlink";
pub const COL_INODE: &str = "inode";
pub const COL_FSID: &str = "fsid";
pub const COL_XATTR_BLOB: &str = "xattr_blob";
pub const COL_SYMLINK_TARGET: &str = "symlink_target";
pub const COL_FILE_TYPE: &str = "file_type";

/// Columns the mover *requires* to function. If any are missing from a
/// shard's actual schema, the mover refuses the shard with
/// `Error::MissingColumn`.
pub const REQUIRED_COLUMNS: &[&str] = &[COL_ROW_ID, COL_PATH, COL_SIZE, COL_MODE, COL_FILE_TYPE];

// =============================================================================
// File-type tag values — must match what the walker emits.
// =============================================================================
//
// These mirror POSIX d_type / S_IFMT values but as a small enum so the
// parquet column is a UINT8 rather than the full mode bits. `mode`
// carries the same type in its `S_IFMT` bits, and the two must agree:
// everything that writes or reads a canonical row goes through the
// mappings below, so there is one definition of each.

/// The type bits of a POSIX `mode`. Spelled out rather than taken
/// from `libc`: they are part of the on-disk contract, not of the
/// platform the mover happens to run on.
pub const S_IFMT: u32 = 0o170000;
pub const S_IFIFO: u32 = 0o010000;
pub const S_IFCHR: u32 = 0o020000;
pub const S_IFDIR: u32 = 0o040000;
pub const S_IFBLK: u32 = 0o060000;
pub const S_IFREG: u32 = 0o100000;
pub const S_IFLNK: u32 = 0o120000;
pub const S_IFSOCK: u32 = 0o140000;

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileTypeTag {
    Unknown = 0,
    Regular = 1,
    Dir = 2,
    Symlink = 3,
    Fifo = 4,
    Socket = 5,
    BlockDev = 6,
    CharDev = 7,
}

impl FileTypeTag {
    /// The seven tags a canonical shard may carry, in tag order.
    /// `Unknown` is an in-memory sentinel and is not one of them.
    pub const ALL: [FileTypeTag; 7] = [
        Self::Regular,
        Self::Dir,
        Self::Symlink,
        Self::Fifo,
        Self::Socket,
        Self::BlockDev,
        Self::CharDev,
    ];

    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Regular,
            2 => Self::Dir,
            3 => Self::Symlink,
            4 => Self::Fifo,
            5 => Self::Socket,
            6 => Self::BlockDev,
            7 => Self::CharDev,
            _ => Self::Unknown,
        }
    }

    /// True if this entry has data the mover should copy.
    pub fn has_data(self) -> bool {
        matches!(self, Self::Regular)
    }

    /// A fifo, socket, or device node: a recognized type the mover
    /// does not create. Exhaustive on purpose, so a new tag has to be
    /// placed here deliberately.
    pub fn is_special(self) -> bool {
        match self {
            Self::Fifo | Self::Socket | Self::BlockDev | Self::CharDev => true,
            Self::Unknown | Self::Regular | Self::Dir | Self::Symlink => false,
        }
    }

    /// The `S_IFMT` bits a row with this tag carries in `mode`.
    /// `None` for `Unknown`, which no row carries.
    pub fn mode_type_bits(self) -> Option<u32> {
        match self {
            Self::Unknown => None,
            Self::Regular => Some(S_IFREG),
            Self::Dir => Some(S_IFDIR),
            Self::Symlink => Some(S_IFLNK),
            Self::Fifo => Some(S_IFIFO),
            Self::Socket => Some(S_IFSOCK),
            Self::BlockDev => Some(S_IFBLK),
            Self::CharDev => Some(S_IFCHR),
        }
    }

    /// The tag for the type bits of `mode`, or `None` when they are
    /// not one of the seven types.
    pub fn from_mode(mode: u32) -> Option<Self> {
        let bits = mode & S_IFMT;
        Self::ALL
            .into_iter()
            .find(|tag| tag.mode_type_bits() == Some(bits))
    }

    /// The tag for the `file_type` string of a legacy walker shard.
    ///
    /// These seven strings are the walker's whole output for that
    /// column; it never writes anything else. `None` for every other
    /// value, including `unknown`, the empty string, a different
    /// case, and MIME-style values: the caller must reject the row,
    /// never guess a type for it.
    pub fn from_walker_file_type(value: &str) -> Option<Self> {
        match value {
            "file" => Some(Self::Regular),
            "directory" => Some(Self::Dir),
            "symlink" => Some(Self::Symlink),
            "fifo" => Some(Self::Fifo),
            "socket" => Some(Self::Socket),
            "block_device" => Some(Self::BlockDev),
            "char_device" => Some(Self::CharDev),
            _ => None,
        }
    }

    /// Lowercase name for messages and records.
    pub fn name(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Regular => "regular",
            Self::Dir => "dir",
            Self::Symlink => "symlink",
            Self::Fifo => "fifo",
            Self::Socket => "socket",
            Self::BlockDev => "block_dev",
            Self::CharDev => "char_dev",
        }
    }

    /// `Ok` when `mode`'s type bits are the ones this tag requires.
    /// The error says what was found, for the caller to attach a row
    /// to. `Unknown` never agrees with anything.
    pub fn check_mode(self, mode: u32) -> Result<(), String> {
        let bits = mode & S_IFMT;
        match self.mode_type_bits() {
            Some(expected) if expected == bits => Ok(()),
            Some(expected) => Err(format!(
                "file_type={} ({}) requires mode type bits {expected:#o}, but mode {mode:#o} has \
                 {bits:#o} ({})",
                self as u8,
                self.name(),
                Self::from_mode(mode).map_or("no recognized type", Self::name),
            )),
            None => Err(format!(
                "file_type={} ({}) is not a canonical type",
                self as u8,
                self.name()
            )),
        }
    }
}

// =============================================================================
// Arrow schema constructor — what we expect a shard to look like.
// =============================================================================

/// Construct the canonical Arrow schema. Any shard's actual schema must
/// be a superset of `REQUIRED_COLUMNS` with matching types; extra
/// columns are allowed and ignored.
pub fn canonical_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new(COL_ROW_ID, DataType::UInt64, false),
        // Raw bytes — POSIX paths are not guaranteed UTF-8.
        Field::new(COL_PATH, DataType::Binary, false),
        Field::new(COL_SIZE, DataType::UInt64, false),
        Field::new(COL_MTIME_SEC, DataType::Int64, true),
        Field::new(COL_MTIME_NSEC, DataType::Int32, true),
        Field::new(COL_ATIME_SEC, DataType::Int64, true),
        Field::new(COL_ATIME_NSEC, DataType::Int32, true),
        Field::new(COL_MODE, DataType::UInt32, false),
        Field::new(COL_UID, DataType::UInt32, true),
        Field::new(COL_GID, DataType::UInt32, true),
        Field::new(COL_NLINK, DataType::UInt32, true),
        Field::new(COL_INODE, DataType::UInt64, true),
        // Source filesystem identifier; combined with `inode` to
        // disambiguate hardlinks across underlying filesystems within
        // an export. Nullable per contract; mover falls back to
        // grouping by inode alone with a one-time WARN.
        Field::new(COL_FSID, DataType::UInt64, true),
        // Reserved for future xattr support; NULL until walker emits it.
        Field::new(COL_XATTR_BLOB, DataType::Binary, true),
        Field::new(COL_SYMLINK_TARGET, DataType::Binary, true),
        Field::new(COL_FILE_TYPE, DataType::UInt8, false),
    ]))
}

/// Compose a `row_id` from `(shard_index, row_offset_within_shard)`.
///
/// Shard index occupies the high 24 bits, row offset the low 40. This
/// gives us up to 16M shards × ~1T rows/shard. Materialized at write
/// time by the walker — never derived at read time, because predicate
/// pushdown can reorder rows within a row group.
pub fn make_row_id(shard_index: u32, row_in_shard: u64) -> u64 {
    debug_assert!(shard_index < (1 << 24), "shard index overflow");
    debug_assert!(row_in_shard < (1 << 40), "row offset overflow");
    ((shard_index as u64) << 40) | (row_in_shard & ((1 << 40) - 1))
}

pub fn split_row_id(row_id: u64) -> (u32, u64) {
    let shard_index = (row_id >> 40) as u32;
    let row_in_shard = row_id & ((1 << 40) - 1);
    (shard_index, row_in_shard)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The contract's table, one row per tag. Every mapping in this
    /// module is checked against it.
    const TABLE: [(FileTypeTag, u8, u32, &str); 7] = [
        (FileTypeTag::Regular, 1, S_IFREG, "file"),
        (FileTypeTag::Dir, 2, S_IFDIR, "directory"),
        (FileTypeTag::Symlink, 3, S_IFLNK, "symlink"),
        (FileTypeTag::Fifo, 4, S_IFIFO, "fifo"),
        (FileTypeTag::Socket, 5, S_IFSOCK, "socket"),
        (FileTypeTag::BlockDev, 6, S_IFBLK, "block_device"),
        (FileTypeTag::CharDev, 7, S_IFCHR, "char_device"),
    ];

    #[test]
    fn every_tag_maps_to_its_value_mode_bits_and_walker_string() {
        assert_eq!(FileTypeTag::ALL.len(), TABLE.len());
        for (tag, value, bits, walker) in TABLE {
            assert_eq!(tag as u8, value, "{tag:?}");
            assert_eq!(FileTypeTag::from_u8(value), tag, "{tag:?}");
            assert_eq!(tag.mode_type_bits(), Some(bits), "{tag:?}");
            assert_eq!(FileTypeTag::from_mode(bits | 0o7777), Some(tag), "{tag:?}");
            assert_eq!(
                FileTypeTag::from_walker_file_type(walker),
                Some(tag),
                "{walker}"
            );
            assert!(FileTypeTag::ALL.contains(&tag));
        }
    }

    /// The POSIX values, written out. `mig-walker-rewrite` checks the
    /// same constants against `libc`.
    #[test]
    fn mode_type_bits_are_the_posix_values() {
        assert_eq!(S_IFMT, 0o170000);
        assert_eq!(S_IFREG, 0o100000);
        assert_eq!(S_IFDIR, 0o040000);
        assert_eq!(S_IFLNK, 0o120000);
        assert_eq!(S_IFIFO, 0o010000);
        assert_eq!(S_IFSOCK, 0o140000);
        assert_eq!(S_IFBLK, 0o060000);
        assert_eq!(S_IFCHR, 0o020000);
    }

    #[test]
    fn unknown_has_no_mode_bits_and_is_not_canonical() {
        assert_eq!(FileTypeTag::Unknown.mode_type_bits(), None);
        assert!(!FileTypeTag::ALL.contains(&FileTypeTag::Unknown));
        assert!(FileTypeTag::Unknown.check_mode(S_IFREG | 0o644).is_err());
        for value in [0u8, 8, 9, 255] {
            assert_eq!(FileTypeTag::from_u8(value), FileTypeTag::Unknown, "{value}");
        }
    }

    #[test]
    fn walker_strings_outside_the_table_have_no_tag() {
        for value in [
            "unknown",
            "",
            "File",
            "FILE",
            "Directory",
            "FIFO",
            "regular",
            "dir",
            "text/plain",
            "application/pdf",
            "inode/directory",
            " file",
            "file ",
            "pipe",
            "device",
        ] {
            assert_eq!(
                FileTypeTag::from_walker_file_type(value),
                None,
                "{value:?} must not be given a type"
            );
        }
    }

    #[test]
    fn modes_without_a_recognized_type_have_no_tag() {
        for mode in [0, 0o644, 0o7777, 0o030000 | 0o644, 0o170000, 0o110000] {
            assert_eq!(FileTypeTag::from_mode(mode), None, "{mode:#o}");
        }
    }

    /// Every tag accepts exactly its own type bits.
    #[test]
    fn check_mode_accepts_only_the_tags_own_type_bits() {
        for (tag, _, bits, _) in TABLE {
            for (_, _, other, _) in TABLE {
                let mode = other | 0o750;
                if other == bits {
                    assert!(tag.check_mode(mode).is_ok(), "{tag:?} {mode:#o}");
                } else {
                    let err = tag.check_mode(mode).unwrap_err();
                    assert!(
                        err.contains(&format!("{mode:#o}")),
                        "{tag:?} {mode:#o}: {err}"
                    );
                }
            }
            let err = tag.check_mode(0o644).unwrap_err();
            assert!(err.contains("no recognized type"), "{tag:?}: {err}");
        }
    }

    #[test]
    fn only_the_four_special_tags_are_special() {
        for (tag, ..) in TABLE {
            let special = matches!(
                tag,
                FileTypeTag::Fifo
                    | FileTypeTag::Socket
                    | FileTypeTag::BlockDev
                    | FileTypeTag::CharDev
            );
            assert_eq!(tag.is_special(), special, "{tag:?}");
        }
        assert!(!FileTypeTag::Unknown.is_special());
    }

    #[test]
    fn row_id_round_trip() {
        let cases = [
            (0u32, 0u64),
            (1, 1),
            (42, 1_000_000),
            ((1 << 24) - 1, (1 << 40) - 1),
        ];
        for (shard, row) in cases {
            let id = make_row_id(shard, row);
            assert_eq!(split_row_id(id), (shard, row));
        }
    }
}
