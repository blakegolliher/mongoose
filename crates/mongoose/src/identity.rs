//! Layer 3 of endpoint separation: what the two servers themselves
//! say, checked after mounting and before any destination write, on
//! every run.
//!
//! Two URL strings that differ can still name one directory: a host
//! by name and by address, two DNS aliases, a server reached through
//! two interfaces, a subdirectory exported twice under different
//! names. The code comments in the engine record that an overlapping
//! pair once truncated source files, so this module does not trust
//! spelling or DNS. For each mounted root it collects:
//!
//! - the **peer address** of the connected socket;
//! - the **root filehandle** libnfs got from MNT, and the root's
//!   `(fsid, fileid)` from a LOOKUP of `.`;
//! - the **ancestor chain**: LOOKUP of `..` repeated until the server
//!   returns the same object (the export top on most servers), an
//!   error, or the depth cap.
//!
//! Then it looks for the source root among the destination root's
//! ancestors and vice versa (and for equality), by filehandle bytes
//! on any server and by `(fsid, fileid)` once the servers are known
//! to be one (same name, shared address, same peer). A filehandle
//! match between what look like two servers is confirmed by a
//! **probe**: an empty directory made under the destination root and
//! looked up on the *source* connection through the destination's
//! own filehandle — visible means one server; ENOENT means two servers
//! that happen to hand out equal bytes (cloned images do); anything
//! else means separation cannot be proved, and the job is refused.
//!
//! On one server, the copy-time check also searches the source index
//! for a directory whose fileid equals the destination root's and
//! confirms it by LOOKUP from the source root. That catches a
//! destination export that is a bind mount or second export of a
//! directory inside the source tree, where `..` cannot leave the
//! export and the export names look unrelated.
//!
//! Not covered, and documented in `docs/REFERENCE.md`: a *source*
//! export that is a differently named alias of a directory inside the
//! destination tree. The copy then writes beside the source rather
//! than over it; the per-file self-target check still refuses any
//! path collision it can see.
//!
//! There is no override. If separation cannot be proved, mongoose
//! refuses.

use crate::endpoint::{canonical_ip, overlap_error, NameEvidence, NfsUrl, PathRelation};
use anyhow::{Context, Result};
use migration_core::schema::FileTypeTag;
use migration_core::shard::ShardReader;
use migration_mover::join_root;
use migration_mover::libnfs::raw::{self, Fh, Ident};
use migration_mover::libnfs::{ops, ContextPair, LibnfsContextPool, NfsContext};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;

/// Cap on `..` steps per root; deeper trees stop with `WalkEnd::DepthCap`.
pub const MAX_DEPTH: usize = 4096;

/// Why an ancestor walk stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalkEnd {
    /// `..` returned the same object: the top of what this mount can
    /// see.
    Top,
    Error(String),
    DepthCap,
}

/// What one mounted root looks like from its server.
#[derive(Debug, Clone)]
pub struct RootEvidence {
    pub peer: Option<IpAddr>,
    /// The export string libnfs mounted (the URL path).
    pub export: String,
    pub fh: Fh,
    pub ident: Option<Ident>,
    /// Parent first, then upward. Excludes the root itself.
    pub ancestors: Vec<(Fh, Option<Ident>)>,
    pub top: WalkEnd,
}

impl RootEvidence {
    pub fn describe(&self) -> String {
        format!(
            "peer {} export {} fh {} bytes ident {} ancestors {} ({})",
            self.peer
                .map(|p| p.to_string())
                .unwrap_or_else(|| "?".into()),
            self.export,
            self.fh.len(),
            self.ident
                .map(|i| format!("{:x}:{}", i.fsid, i.fileid))
                .unwrap_or_else(|| "?".into()),
            self.ancestors.len(),
            match &self.top {
                WalkEnd::Top => "reached the top".to_string(),
                WalkEnd::Error(e) => format!("stopped: {e}"),
                WalkEnd::DepthCap => "depth cap".to_string(),
            }
        )
    }
}

/// Collect [`RootEvidence`] for one mounted context.
pub fn gather(ctx: &mut NfsContext) -> Result<RootEvidence> {
    let fh = raw::root_fh(ctx).map_err(raw_err)?;
    let peer = ctx.peer_addr().map(|a| canonical_ip(a.ip()));
    let export = ctx.export_path();
    // A server may omit post-op attributes; identity then rests on
    // filehandle bytes alone.
    let ident = match raw::lookup_ident(ctx, &fh, b".") {
        Ok((_, ident)) => ident,
        Err(e) => {
            tracing::warn!(error = %e.detail, "LOOKUP . on the mounted root failed; using filehandle identity only");
            None
        }
    };
    let mut ancestors = Vec::new();
    let mut cur = fh.clone();
    let mut cur_ident = ident;
    let top = loop {
        if ancestors.len() >= MAX_DEPTH {
            break WalkEnd::DepthCap;
        }
        match raw::lookup_ident(ctx, &cur, b"..") {
            Ok((pfh, pident)) => {
                let same_object = pfh == cur || (pident.is_some() && pident == cur_ident);
                if same_object {
                    break WalkEnd::Top;
                }
                ancestors.push((pfh.clone(), pident));
                cur = pfh;
                cur_ident = pident;
            }
            Err(e) => break WalkEnd::Error(format!("{} ({})", e.tag, e.detail)),
        }
    };
    Ok(RootEvidence {
        peer,
        export,
        fh,
        ident,
        ancestors,
        top,
    })
}

fn raw_err(e: raw::RawError) -> anyhow::Error {
    anyhow::anyhow!("{}: {}", e.tag, e.detail)
}

/// What established an overlap finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Basis {
    /// Equal filehandle bytes. Definitive on one server; needs a probe
    /// when the servers are not otherwise known to be one.
    Fh,
    /// Equal `(fsid, fileid)` on a server already known to be one.
    Ident,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub relation: PathRelation,
    pub by: Basis,
}

/// Same server by anything the names or sockets say.
pub fn same_server(src: &RootEvidence, dst: &RootEvidence, names: &NameEvidence) -> bool {
    names.may_be_same_server() || matches!((src.peer, dst.peer), (Some(a), Some(b)) if a == b)
}

/// Pure comparison of two roots: equal, or one inside the other, by
/// filehandle bytes (always) and by identity (when `same_server`).
pub fn find_overlap(src: &RootEvidence, dst: &RootEvidence, same_server: bool) -> Option<Finding> {
    let same = |a_fh: &Fh, a_id: Option<Ident>, b_fh: &Fh, b_id: Option<Ident>| {
        same_object(a_fh, a_id, b_fh, b_id, same_server)
    };
    if let Some(by) = same(&src.fh, src.ident, &dst.fh, dst.ident) {
        return Some(Finding {
            relation: PathRelation::Same,
            by,
        });
    }
    for (fh, id) in &dst.ancestors {
        if let Some(by) = same(&src.fh, src.ident, fh, *id) {
            return Some(Finding {
                relation: PathRelation::DestInsideSource,
                by,
            });
        }
    }
    for (fh, id) in &src.ancestors {
        if let Some(by) = same(&dst.fh, dst.ident, fh, *id) {
            return Some(Finding {
                relation: PathRelation::SourceInsideDest,
                by,
            });
        }
    }
    None
}

/// Are these two objects one object? Filehandle bytes decide on any
/// server; `(fsid, fileid)` only once the servers are known to be one.
fn same_object(
    a_fh: &Fh,
    a_id: Option<Ident>,
    b_fh: &Fh,
    b_id: Option<Ident>,
    same_server: bool,
) -> Option<Basis> {
    if !a_fh.is_empty() && a_fh == b_fh {
        return Some(Basis::Fh);
    }
    if same_server {
        if let (Some(x), Some(y)) = (a_id, b_id) {
            if x.fsid == y.fsid && x.fileid == y.fileid {
                return Some(Basis::Ident);
            }
        }
    }
    None
}

/// Outcome of a passed separation check, for the operator line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Separation {
    pub same_server: bool,
    pub evidence: String,
}

/// The whole layer-3 check over one context pair from `pool`. Refuses
/// (with the same operator-facing message as the name checks) on any
/// proved or unprovable overlap. `index_shards` is the full source
/// index when one exists (copy and sync); empty otherwise.
pub async fn prove_separation(
    pool: &Arc<dyn LibnfsContextPool>,
    src: &NfsUrl,
    dst: &NfsUrl,
    names: &NameEvidence,
    index_shards: &[PathBuf],
) -> Result<Separation> {
    let pair = pool.acquire().await?;
    let src = src.clone();
    let dst = dst.clone();
    let names = names.clone();
    let shards = index_shards.to_vec();
    tokio::task::spawn_blocking(move || {
        let mut pair = pair;
        prove_with_pair(&mut pair, &src, &dst, &names, &shards)
    })
    .await
    .context("endpoint identity task panicked")?
}

fn prove_with_pair(
    pair: &mut ContextPair,
    src_url: &NfsUrl,
    dst_url: &NfsUrl,
    names: &NameEvidence,
    index_shards: &[PathBuf],
) -> Result<Separation> {
    let src = gather(pair.src()).context("inspecting the mounted source root")?;
    let dst = gather(pair.dst()).context("inspecting the mounted destination root")?;
    tracing::info!(source = %src.describe(), dest = %dst.describe(), "endpoint identity");

    let same = same_server(&src, &dst, names);
    let mut evidence = vec![names.describe()];
    if let (Some(a), Some(b)) = (src.peer, dst.peer) {
        evidence.push(if a == b {
            format!("both connections reach {a}")
        } else {
            format!("connections reach {a} and {b}")
        });
    }

    match find_overlap(&src, &dst, same) {
        Some(Finding {
            relation,
            by: Basis::Ident,
        }) => {
            return Err(overlap_error(
                src_url,
                dst_url,
                relation,
                &format!(
                    "{} and the mounted roots have the same server identity (fsid, fileid), or one root is an ancestor of the other",
                    evidence.join("; ")
                ),
            ));
        }
        Some(Finding {
            relation,
            by: Basis::Fh,
        }) if same => {
            return Err(overlap_error(
                src_url,
                dst_url,
                relation,
                &format!(
                    "{} and the mounted roots have equal filehandles, or one root's filehandle is an ancestor of the other",
                    evidence.join("; ")
                ),
            ));
        }
        Some(Finding {
            relation,
            by: Basis::Fh,
        }) => {
            // Two apparently different servers handing out equal
            // filehandle bytes: cloned images do that. Ask the servers.
            match probe(pair, &dst, dst_url) {
                ProbeResult::Visible => {
                    return Err(overlap_error(
                        src_url,
                        dst_url,
                        relation,
                        "a directory created under the destination root is visible through the \
                         source connection: the two names reach one server",
                    ));
                }
                ProbeResult::Absent => {
                    evidence.push(
                        "equal root filehandle bytes, but a probe directory created under the \
                         destination is not visible from the source: two servers"
                            .into(),
                    );
                }
                ProbeResult::Inconclusive(detail) => {
                    anyhow::bail!(
                        "cannot prove that source and destination are separate: the mounted roots \
                         have equal filehandle bytes and the probe could not decide ({detail}).\n  \
                         source: {}\n  dest:   {}\n\n  Refusing to start.",
                        src_url.url(),
                        dst_url.url(),
                    );
                }
            }
        }
        None => {}
    }

    if same {
        // One server, roots not equal or nested by ancestry. A
        // destination export that aliases a directory inside the
        // source tree still hides here; the source index knows every
        // directory's fileid.
        if let Some(dst_id) = dst.ident {
            if let Some(path) = index_dir_with_fileid(index_shards, dst_id.fileid)
                .context("searching the source index for the destination root")?
                .into_iter()
                .find(|p| confirm_path_is(pair.src(), &src.fh, p, dst_id))
            {
                return Err(overlap_error(
                    src_url,
                    dst_url,
                    PathRelation::DestInsideSource,
                    &format!(
                        "{}; the destination root is the source directory {}",
                        evidence.join("; "),
                        String::from_utf8_lossy(&path)
                    ),
                ));
            }
        }
        evidence.push(match (&src.top, &dst.top) {
            (WalkEnd::Top, WalkEnd::Top) => {
                "one server; ancestry of both roots walked to the top without meeting".into()
            }
            _ => format!(
                "one server; roots are distinct exports ({} vs {}) and neither is an ancestor of the other as far as the server lets `..` go",
                src.export, dst.export
            ),
        });
    } else {
        evidence.push("two servers".into());
    }
    Ok(Separation {
        same_server: same,
        evidence: evidence.join("; "),
    })
}

enum ProbeResult {
    Visible,
    Absent,
    Inconclusive(String),
}

/// Make an empty directory under the destination root and look it up
/// on the source connection through the destination root's own
/// filehandle. Cleaned up afterwards, best-effort.
fn probe(pair: &mut ContextPair, dst: &RootEvidence, dst_url: &NfsUrl) -> ProbeResult {
    let name = format!(
        ".mongoose-probe-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    if let Err(e) = raw::mkdir(pair.dst(), &dst.fh, name.as_bytes(), 0o700) {
        return ProbeResult::Inconclusive(format!("MKDIR of the probe failed: {}", e.detail));
    }
    let result = match raw::lookup(pair.src(), &dst.fh, name.as_bytes()) {
        Ok(_) => ProbeResult::Visible,
        Err(e) if e.tag == "ENOENT" || e.tag == "ESTALE" => ProbeResult::Absent,
        Err(e) => ProbeResult::Inconclusive(format!("LOOKUP of the probe failed: {}", e.detail)),
    };
    let _ = dst_url;
    let path = join_root(b"/", format!("/{name}").as_bytes());
    if let Err(e) = ops::rmdir(pair.dst(), &path) {
        tracing::warn!(
            probe = %name,
            error = %e.error,
            "could not remove the identity probe directory under the destination root",
        );
    }
    result
}

/// Paths of directory rows in `shards` whose `inode` is `fileid`.
pub fn index_dir_with_fileid(shards: &[PathBuf], fileid: u64) -> Result<Vec<Vec<u8>>> {
    let mut out = Vec::new();
    for shard in shards {
        let rows = ShardReader::open(shard)
            .with_context(|| format!("opening {}", shard.display()))?
            .into_rows()
            .with_context(|| format!("reading {}", shard.display()))?;
        for row in rows {
            let row = row.with_context(|| format!("reading {}", shard.display()))?;
            if row.file_type == FileTypeTag::Dir && row.inode == Some(fileid) {
                out.push(row.path);
            }
        }
    }
    Ok(out)
}

/// LOOKUP `path` component by component from `root_fh` on the source
/// connection and compare the result's identity with `want`.
fn confirm_path_is(ctx: &mut NfsContext, root_fh: &Fh, path: &[u8], want: Ident) -> bool {
    let mut cur = root_fh.clone();
    let mut ident = None;
    for comp in path.split(|&b| b == b'/').filter(|c| !c.is_empty()) {
        match raw::lookup_ident(ctx, &cur, comp) {
            Ok((fh, id)) => {
                cur = fh;
                ident = id;
            }
            Err(_) => return false,
        }
    }
    matches!(ident, Some(i) if i.fsid == want.fsid && i.fileid == want.fileid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// One synthetic ancestor: filehandle bytes and optional (fsid, fileid).
    type Anc<'a> = (&'a [u8], Option<(u64, u64)>);

    fn ev(fh: &[u8], ident: Option<(u64, u64)>, ancestors: &[Anc<'_>]) -> RootEvidence {
        let id = |o: Option<(u64, u64)>| {
            o.map(|(fsid, fileid)| Ident {
                fsid,
                fileid,
                ftype: 2,
            })
        };
        RootEvidence {
            peer: None,
            export: "/e".into(),
            fh: fh.to_vec(),
            ident: id(ident),
            ancestors: ancestors
                .iter()
                .map(|(f, i)| (f.to_vec(), id(*i)))
                .collect(),
            top: WalkEnd::Top,
        }
    }

    fn names(same_host: bool, shared: &[&str]) -> NameEvidence {
        NameEvidence {
            same_host,
            src_addrs: BTreeSet::new(),
            dst_addrs: BTreeSet::new(),
            shared: shared.iter().map(|s| s.parse().unwrap()).collect(),
        }
    }

    #[test]
    fn equal_filehandles_are_the_same_root_on_any_server() {
        let s = ev(b"AAAA", Some((1, 100)), &[]);
        let d = ev(b"AAAA", Some((9, 900)), &[]);
        assert_eq!(
            find_overlap(&s, &d, false),
            Some(Finding {
                relation: PathRelation::Same,
                by: Basis::Fh
            })
        );
    }

    #[test]
    fn identity_counts_only_on_one_server() {
        let s = ev(b"AAAA", Some((1, 100)), &[]);
        let d = ev(b"BBBB", Some((1, 100)), &[]);
        assert_eq!(
            find_overlap(&s, &d, false),
            None,
            "fsid/fileid collide across servers"
        );
        assert_eq!(
            find_overlap(&s, &d, true),
            Some(Finding {
                relation: PathRelation::Same,
                by: Basis::Ident
            })
        );
    }

    #[test]
    fn nesting_is_found_in_either_chain() {
        // dst = /export/data/backup, src = /export/data: src root is
        // dst's parent.
        let s = ev(b"DATA", Some((1, 10)), &[(b"EXPORT", Some((1, 1)))]);
        let d = ev(
            b"BACKUP",
            Some((1, 20)),
            &[(b"DATA", Some((1, 10))), (b"EXPORT", Some((1, 1)))],
        );
        assert_eq!(
            find_overlap(&s, &d, false).unwrap().relation,
            PathRelation::DestInsideSource
        );
        assert_eq!(
            find_overlap(&d, &s, false).unwrap().relation,
            PathRelation::SourceInsideDest
        );
        // Same nesting, different filehandle encodings (two exports of
        // one tree): only identity sees it, and only on one server.
        let d2 = ev(
            b"X1",
            Some((1, 20)),
            &[(b"X2", Some((1, 10))), (b"X3", Some((1, 1)))],
        );
        assert_eq!(find_overlap(&s, &d2, false), None);
        assert_eq!(
            find_overlap(&s, &d2, true),
            Some(Finding {
                relation: PathRelation::DestInsideSource,
                by: Basis::Ident
            })
        );
    }

    #[test]
    fn siblings_under_one_export_are_disjoint() {
        let s = ev(b"A", Some((1, 10)), &[(b"EXPORT", Some((1, 1)))]);
        let d = ev(b"B", Some((1, 11)), &[(b"EXPORT", Some((1, 1)))]);
        assert_eq!(find_overlap(&s, &d, true), None);
    }

    #[test]
    fn empty_filehandles_never_match() {
        let s = ev(b"", None, &[]);
        let d = ev(b"", None, &[]);
        assert_eq!(find_overlap(&s, &d, false), None);
    }

    #[test]
    fn same_server_from_names_or_peers() {
        let mut s = ev(b"A", None, &[]);
        let mut d = ev(b"B", None, &[]);
        assert!(!same_server(&s, &d, &names(false, &[])));
        assert!(same_server(&s, &d, &names(true, &[])));
        assert!(same_server(&s, &d, &names(false, &["10.0.0.5"])));
        s.peer = Some("10.0.0.5".parse().unwrap());
        d.peer = Some("10.0.0.5".parse().unwrap());
        assert!(same_server(&s, &d, &names(false, &[])), "same socket peer");
        d.peer = Some("10.0.0.6".parse().unwrap());
        assert!(!same_server(&s, &d, &names(false, &[])));
    }

    #[test]
    fn index_search_finds_directories_by_fileid() {
        use crate::verify::namespace::tests::write_canonical_shard;
        use crate::verify::namespace::Entry;
        let dir = tempfile::tempdir().unwrap();
        let mk = |path: &[u8], t: FileTypeTag| Entry {
            path: path.to_vec(),
            file_type: t,
            size: 0,
            mode: 0o40755,
            uid: None,
            gid: None,
            mtime: None,
        };
        // write_canonical_shard assigns inode 1000 + row index.
        let p = dir.path().join("part.parquet");
        write_canonical_shard(
            &p,
            0,
            &[
                mk(b"/a", FileTypeTag::Dir),
                mk(b"/a/f", FileTypeTag::Regular),
                mk(b"/a/tmp", FileTypeTag::Dir),
            ],
        );
        assert_eq!(
            index_dir_with_fileid(std::slice::from_ref(&p), 1002).unwrap(),
            vec![b"/a/tmp".to_vec()]
        );
        assert!(
            index_dir_with_fileid(std::slice::from_ref(&p), 1001)
                .unwrap()
                .is_empty(),
            "files do not count"
        );
        assert!(index_dir_with_fileid(std::slice::from_ref(&p), 7)
            .unwrap()
            .is_empty());
    }
}
