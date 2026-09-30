//! Per-file strategy selection.
//!
//! Decided per-file from the row kind, size, and hardlink state. Regular
//! non-empty files use the libnfs data path; symlinks and directories use
//! their metadata-only paths. Fifos, sockets, and device nodes are recognized
//! and deliberately not created: that is an omission with its own strategy,
//! counter, and durable record, never a generic skip.

use migration_core::schema::FileTypeTag;
use migration_core::shard::RowView;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    /// Regular-file libnfs READ/WRITE.
    ///
    /// The historical variant name is retained because it appears in mover
    /// outcomes and debug logging. The active sync and bucketed async paths do
    /// not use io_uring fixed buffers.
    LibnfsIoUring,
    /// Symlink — `READLINK` (or use cached `symlink_target`) → `SYMLINK`
    /// on dest. No data path.
    Symlink,
    /// Hardlink to a previously-copied inode within this shard.
    HardlinkExisting,
    /// Empty file — `CREATE` only, no data.
    Empty,
    /// Directory entry — ensure dir exists, then apply
    /// mode/owner/mtime. No data path. Must run after all
    /// non-dir rows in the same shard so file commits don't
    /// restamp the dir's mtime; the shard processor enforces this.
    DirAttrs,
    /// Fifo, socket, block device, or character device. The mover does
    /// not create these, so the row is a processed **omission**: no
    /// destination RPC, no bytes, a `SPECIAL_NOT_COPIED` downgrade record,
    /// and `files_special_not_copied` instead of `files_ok`. Selected for
    /// those four types only.
    SpecialNotCopied,
    /// The row has no canonical type. The shard reader rejects such a row
    /// before it reaches the mover, so this is reachable only through a
    /// row built by hand; it is a per-file failure. It is never treated as
    /// a special node: an unimplemented or corrupt type must not hide
    /// behind the omission path.
    UnknownType,
}

#[derive(Debug, Clone, Copy)]
pub struct StrategyContext {
    /// Set of inodes already copied in this shard, for hardlink
    /// resolution. Caller maintains this; we just consult it.
    pub already_copied_inode: bool,
}

pub fn pick(row: &RowView, ctx: &StrategyContext) -> Strategy {
    // Exhaustive, with no wildcard: a tag added to `FileTypeTag` does not
    // compile until it is given a strategy here.
    match row.file_type {
        FileTypeTag::Regular => {
            if row.inode.is_some() && ctx.already_copied_inode {
                Strategy::HardlinkExisting
            } else if row.size == 0 {
                Strategy::Empty
            } else {
                Strategy::LibnfsIoUring
            }
        }
        FileTypeTag::Symlink => Strategy::Symlink,
        FileTypeTag::Dir => Strategy::DirAttrs,
        FileTypeTag::Fifo | FileTypeTag::Socket | FileTypeTag::BlockDev | FileTypeTag::CharDev => {
            Strategy::SpecialNotCopied
        }
        FileTypeTag::Unknown => Strategy::UnknownType,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use migration_core::schema::FileTypeTag;

    fn ctx(already_copied_inode: bool) -> StrategyContext {
        StrategyContext {
            already_copied_inode,
        }
    }

    fn row(size: u64, file_type: FileTypeTag) -> RowView {
        RowView {
            row_id: 1,
            path: b"/file".to_vec(),
            size,
            mtime_sec: None,
            mtime_nsec: None,
            atime_sec: None,
            atime_nsec: None,
            mode: 0o100644,
            uid: None,
            gid: None,
            nlink: None,
            inode: None,
            fsid: None,
            xattr_blob: None,
            symlink_target: None,
            file_type,
        }
    }

    #[test]
    fn empty_files_pick_empty() {
        let r = row(0, FileTypeTag::Regular);
        assert_eq!(pick(&r, &ctx(false)), Strategy::Empty);
    }

    #[test]
    fn symlinks_pick_symlink() {
        let r = row(0, FileTypeTag::Symlink);
        assert_eq!(pick(&r, &ctx(false)), Strategy::Symlink);
    }

    #[test]
    fn dirs_pick_dir_attrs() {
        let r = row(0, FileTypeTag::Dir);
        assert_eq!(pick(&r, &ctx(false)), Strategy::DirAttrs);
    }

    #[test]
    fn regular_files_pick_libnfs() {
        let r = row(1 << 20, FileTypeTag::Regular);
        assert_eq!(pick(&r, &ctx(false)), Strategy::LibnfsIoUring);
    }

    #[test]
    fn regular_strategy_keeps_compatibility_debug_label() {
        assert_eq!(format!("{:?}", Strategy::LibnfsIoUring), "LibnfsIoUring");
    }

    #[test]
    fn copied_inodes_pick_hardlink() {
        let mut r = row(1 << 20, FileTypeTag::Regular);
        r.inode = Some(42);
        assert_eq!(pick(&r, &ctx(true)), Strategy::HardlinkExisting);
        assert_eq!(pick(&r, &ctx(false)), Strategy::LibnfsIoUring);
    }

    #[test]
    fn the_four_special_types_pick_special_not_copied() {
        for file_type in [
            FileTypeTag::Fifo,
            FileTypeTag::Socket,
            FileTypeTag::BlockDev,
            FileTypeTag::CharDev,
        ] {
            // Whatever the size or hardlink state says.
            for (size, inode, copied) in [(0, None, false), (4096, Some(7), true)] {
                let mut r = row(size, file_type);
                r.inode = inode;
                assert_eq!(
                    pick(&r, &ctx(copied)),
                    Strategy::SpecialNotCopied,
                    "{file_type:?}"
                );
            }
        }
    }

    /// `SpecialNotCopied` is for the four special types and nothing
    /// else. Every other tag, `Unknown` included, gets its own strategy.
    #[test]
    fn nothing_but_a_special_type_picks_special_not_copied() {
        for file_type in [
            FileTypeTag::Unknown,
            FileTypeTag::Regular,
            FileTypeTag::Dir,
            FileTypeTag::Symlink,
        ] {
            for size in [0, 4096] {
                for copied in [false, true] {
                    let mut r = row(size, file_type);
                    r.inode = Some(9);
                    assert_ne!(
                        pick(&r, &ctx(copied)),
                        Strategy::SpecialNotCopied,
                        "{file_type:?} size={size} copied={copied}"
                    );
                }
            }
        }
        for file_type in FileTypeTag::ALL {
            assert_eq!(
                pick(&row(1, file_type), &ctx(false)) == Strategy::SpecialNotCopied,
                file_type.is_special(),
                "{file_type:?}"
            );
        }
    }

    /// An untyped row is a failure, not an omission.
    #[test]
    fn unknown_type_is_never_skipped() {
        let r = row(4096, FileTypeTag::Unknown);
        assert_eq!(pick(&r, &ctx(false)), Strategy::UnknownType);
        assert_eq!(pick(&r, &ctx(true)), Strategy::UnknownType);
    }
}
