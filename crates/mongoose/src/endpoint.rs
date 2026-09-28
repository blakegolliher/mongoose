//! `--src` / `--dst` handling: parse and canonicalize `nfs://` URLs,
//! decide *by name* whether two endpoints may be the same server, and
//! refuse a pair whose paths overlap on one server.
//!
//! Endpoint separation is proved in three layers, weakest first:
//!
//! 1. **Canonical spelling** (this module). Host case, a trailing
//!    dot, IP literal forms (`[::ffff:10.0.0.5]` is `10.0.0.5`), the
//!    default port, and libnfs option order never make two spellings
//!    two servers.
//! 2. **Name resolution** (this module). Two names whose address sets
//!    intersect may be one server.
//! 3. **Mounted identity** ([`crate::identity`]). The server's own
//!    view — connected peer address, root filehandle, `(fsid,
//!    fileid)`, ancestry — proves the two roots disjoint before any
//!    destination write, on every run.
//!
//! String inequality is never taken as proof of two servers, and a
//! port or option difference is never taken as one either: the same
//! host on another port is treated as possibly the same server, and
//! the mounted check settles it.
//!
//! The engine's own guard (`migration_core::overlap`) compares an
//! export URL plus a root *inside* it and treats any two different
//! URL strings as disjoint. mongoose lets operators put the whole
//! path in the URL (`nfs://host/export/sub`), so `nfs://h/export` vs
//! `nfs://h/export/backup` is caught here — that is exactly the
//! truncate-your-source misconfiguration the engine guard exists for.

use anyhow::{bail, Context, Result};
use std::collections::BTreeSet;
use std::net::{IpAddr, ToSocketAddrs};

/// The NFS port; a URL that spells it is the same as one that does
/// not.
pub const NFS_PORT: u16 = 2049;

/// A parsed, canonical `nfs://server[:port]/path[?opts]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NfsUrl {
    /// Lowercase, no trailing dot; an IP literal in canonical text
    /// form (an IPv4-mapped IPv6 literal collapses to IPv4).
    pub host: String,
    /// `None` for the default NFS port, spelled or not.
    pub port: Option<u16>,
    /// Absolute, no trailing slash except for the bare root `/`.
    pub path: String,
    /// libnfs URL options after `?`: `k=v` pairs sorted and
    /// deduplicated. Passed through to libnfs; never part of identity.
    pub query: Option<String>,
}

impl NfsUrl {
    /// Canonical form handed to the engine (walker mount + mover mount)
    /// and recorded in the work dir.
    pub fn url(&self) -> String {
        let mut s = String::from("nfs://");
        if self.host.contains(':') {
            s.push('[');
            s.push_str(&self.host);
            s.push(']');
        } else {
            s.push_str(&self.host);
        }
        if let Some(p) = self.port {
            s.push(':');
            s.push_str(&p.to_string());
        }
        s.push_str(&self.path);
        if let Some(q) = &self.query {
            s.push('?');
            s.push_str(q);
        }
        s
    }

    /// The host as an IP literal, if it is one.
    pub fn host_ip(&self) -> Option<IpAddr> {
        self.host.parse().ok()
    }

    fn components(&self) -> Vec<&str> {
        self.path.split('/').filter(|c| !c.is_empty()).collect()
    }

    /// Same canonical host name. Port and options are deliberately
    /// ignored: a second NFS port on one host is still one host, and
    /// only the mounted check can say otherwise.
    pub fn same_host(&self, other: &NfsUrl) -> bool {
        self.host == other.host
    }
}

/// Parse an operator-supplied URL. Accepts what libnfs accepts
/// (`nfs://host/path`, `nfs://host:port/path`, `nfs://[v6]/path`,
/// `?opt=val` suffixes) and rejects anything that is not an NFS URL
/// with a clear message naming the flag. The result is canonical.
pub fn parse(flag: &str, raw: &str) -> Result<NfsUrl> {
    let raw = raw.trim();
    let rest = match raw.strip_prefix("nfs://") {
        Some(r) => r,
        None => bail!("{flag} must look like nfs://server/export[/path], got {raw:?}"),
    };
    let (rest, query) = match rest.split_once('?') {
        Some((r, q)) => (r, canonical_query(q)),
        None => (rest, None),
    };
    let Some(slash) = rest.find('/') else {
        bail!("{flag} is missing the export path: nfs://server/export[/path], got {raw:?}")
    };
    let (host_port, path) = rest.split_at(slash);
    if host_port.is_empty() {
        bail!("{flag} is missing the server name: nfs://server/export[/path], got {raw:?}");
    }

    let (host, port) = if let Some(v6) = host_port.strip_prefix('[') {
        let Some((h, after)) = v6.split_once(']') else {
            bail!("{flag}: unterminated IPv6 literal in {raw:?}")
        };
        let port = match after.strip_prefix(':') {
            Some(p) => Some(parse_port(flag, p)?),
            None if after.is_empty() => None,
            None => bail!("{flag}: unexpected {after:?} after the IPv6 literal in {raw:?}"),
        };
        (h.to_string(), port)
    } else if let Some((h, p)) = host_port.rsplit_once(':') {
        (h.to_string(), Some(parse_port(flag, p)?))
    } else {
        (host_port.to_string(), None)
    };
    let host = canonical_host(&host);
    if host.is_empty() {
        bail!("{flag} is missing the server name: nfs://server/export[/path], got {raw:?}");
    }
    let port = port.filter(|p| *p != NFS_PORT);

    // Collapse repeated slashes and drop the trailing one so two
    // spellings of the same path compare equal (and so the mover's
    // path joins never see `//`).
    let components: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
    let path = if components.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", components.join("/"))
    };

    Ok(NfsUrl {
        host,
        port,
        path,
        query,
    })
}

fn parse_port(flag: &str, p: &str) -> Result<u16> {
    p.parse::<u16>()
        .with_context(|| format!("{flag}: {p:?} is not a valid port"))
}

/// Lowercase, no trailing dot, IP literals in canonical text form.
pub fn canonical_host(raw: &str) -> String {
    let h = raw.trim().trim_end_matches('.');
    match h.parse::<IpAddr>() {
        Ok(ip) => canonical_ip(ip).to_string(),
        Err(_) => h.to_ascii_lowercase(),
    }
}

/// One text form per address: an IPv4-mapped IPv6 address is its
/// IPv4 address.
pub fn canonical_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6(v6),
        },
        v4 => v4,
    }
}

/// `k=v&k2=v2` with the pairs sorted and deduplicated; `None` when
/// nothing is left.
pub fn canonical_query(raw: &str) -> Option<String> {
    let mut parts: Vec<&str> = raw
        .split('&')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    parts.sort_unstable();
    parts.dedup();
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("&"))
    }
}

/// How two paths on one server relate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathRelation {
    Same,
    DestInsideSource,
    SourceInsideDest,
    Disjoint,
}

pub fn relate_paths(src: &NfsUrl, dst: &NfsUrl) -> PathRelation {
    let s = src.components();
    let d = dst.components();
    if s == d {
        PathRelation::Same
    } else if d.starts_with(&s) {
        PathRelation::DestInsideSource
    } else if s.starts_with(&d) {
        PathRelation::SourceInsideDest
    } else {
        PathRelation::Disjoint
    }
}

/// The operator-facing refusal for an overlapping pair. `how` says
/// what established that the two names are one server.
pub fn overlap_error(
    src: &NfsUrl,
    dst: &NfsUrl,
    relation: PathRelation,
    how: &str,
) -> anyhow::Error {
    let problem = match relation {
        PathRelation::Same => "source and destination are the same path",
        PathRelation::DestInsideSource => {
            "the destination is inside the source: the copy would write into the tree it is reading"
        }
        PathRelation::SourceInsideDest => {
            "the source is inside the destination: the copy would overwrite the tree it is reading"
        }
        PathRelation::Disjoint => unreachable!("disjoint paths are not an overlap"),
    };
    anyhow::anyhow!(
        "source and destination overlap; refusing to start\n  \
         source: {}\n  \
         dest:   {}\n\n  \
         Problem: {problem}.\n  \
         Same server: {how}.\n\n  \
         Fix: copy to a different server, or to a path on this server that is\n  \
         neither inside the source nor a parent of it.",
        src.url(),
        dst.url(),
    )
}

/// Layer 1: refuse a destination that is the source, contains it, or
/// lies inside it, when the two canonical host names are equal. No
/// network. Different names are not assumed to be different servers;
/// see [`check_overlap_resolved`] and [`crate::identity`].
pub fn check_overlap(src: &NfsUrl, dst: &NfsUrl) -> Result<()> {
    if !src.same_host(dst) {
        return Ok(());
    }
    match relate_paths(src, dst) {
        PathRelation::Disjoint => Ok(()),
        r => Err(overlap_error(src, dst, r, "same host name")),
    }
}

/// What the names say about the two servers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameEvidence {
    pub same_host: bool,
    pub src_addrs: BTreeSet<IpAddr>,
    pub dst_addrs: BTreeSet<IpAddr>,
    /// Addresses both names resolve to.
    pub shared: BTreeSet<IpAddr>,
}

impl NameEvidence {
    /// Conservative: one name, or one shared address, is enough.
    pub fn may_be_same_server(&self) -> bool {
        self.same_host || !self.shared.is_empty()
    }

    pub fn describe(&self) -> String {
        if self.same_host {
            "same host name".to_string()
        } else if !self.shared.is_empty() {
            format!(
                "both names resolve to {}",
                self.shared
                    .iter()
                    .map(|a| a.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        } else {
            format!(
                "different addresses ({} vs {})",
                join_addrs(&self.src_addrs),
                join_addrs(&self.dst_addrs)
            )
        }
    }
}

fn join_addrs(set: &BTreeSet<IpAddr>) -> String {
    set.iter()
        .map(|a| a.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Every address a host name resolves to (a literal resolves to
/// itself). Resolution failure is an error: such a mount would fail
/// anyway, and "unresolvable" must never read as "different".
pub fn resolve_host(host: &str) -> Result<BTreeSet<IpAddr>> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(BTreeSet::from([canonical_ip(ip)]));
    }
    let addrs = (host, NFS_PORT)
        .to_socket_addrs()
        .with_context(|| format!("resolving {host}"))?;
    let set: BTreeSet<IpAddr> = addrs.map(|a| canonical_ip(a.ip())).collect();
    anyhow::ensure!(!set.is_empty(), "{host} resolved to no addresses");
    Ok(set)
}

/// Layer 2: resolve both names and record what they share. One host
/// name needs no resolution to be one server.
pub fn resolve_pair(src: &NfsUrl, dst: &NfsUrl) -> Result<NameEvidence> {
    if src.same_host(dst) {
        return Ok(NameEvidence {
            same_host: true,
            src_addrs: BTreeSet::new(),
            dst_addrs: BTreeSet::new(),
            shared: BTreeSet::new(),
        });
    }
    let src_addrs = resolve_host(&src.host)?;
    let dst_addrs = resolve_host(&dst.host)?;
    let shared = src_addrs.intersection(&dst_addrs).copied().collect();
    Ok(NameEvidence {
        same_host: src.same_host(dst),
        src_addrs,
        dst_addrs,
        shared,
    })
}

/// Layers 1 + 2: refuse overlapping paths whenever the names may be
/// one server. Returns the evidence for the caller to record and to
/// seed the mounted check.
pub fn check_overlap_resolved(src: &NfsUrl, dst: &NfsUrl) -> Result<NameEvidence> {
    let evidence = resolve_pair(src, dst)?;
    if evidence.may_be_same_server() {
        match relate_paths(src, dst) {
            PathRelation::Disjoint => {}
            r => return Err(overlap_error(src, dst, r, &evidence.describe())),
        }
    }
    Ok(evidence)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(raw: &str) -> NfsUrl {
        parse("--src", raw).unwrap()
    }

    #[test]
    fn parses_plain_host_and_path() {
        let u = p("nfs://old.example.com/export/data");
        assert_eq!(u.host, "old.example.com");
        assert_eq!(u.port, None);
        assert_eq!(u.path, "/export/data");
        assert_eq!(u.query, None);
        assert_eq!(u.url(), "nfs://old.example.com/export/data");
    }

    #[test]
    fn normalizes_slashes_and_whitespace() {
        assert_eq!(p(" nfs://h/export/ ").url(), "nfs://h/export");
        assert_eq!(p("nfs://h//export//a/").url(), "nfs://h/export/a");
        assert_eq!(p("nfs://h/").path, "/");
        assert_eq!(p("nfs://h/").url(), "nfs://h/");
    }

    #[test]
    fn canonicalizes_host_spelling() {
        assert_eq!(p("nfs://OLD.Example.COM./export").host, "old.example.com");
        assert_eq!(
            p("nfs://OLD.Example.COM./export").url(),
            "nfs://old.example.com/export"
        );
        assert_eq!(p("nfs://10.0.0.5/e").host, "10.0.0.5");
        // IPv6 text forms collapse to one spelling.
        assert_eq!(p("nfs://[FD00:0:0:0:0:0:0:1]/e").host, "fd00::1");
        assert_eq!(p("nfs://[fd00::1]/e").url(), "nfs://[fd00::1]/e");
        // An IPv4-mapped IPv6 literal is the IPv4 address.
        assert_eq!(p("nfs://[::ffff:10.0.0.5]/e").host, "10.0.0.5");
        assert_eq!(p("nfs://[::ffff:10.0.0.5]/e").url(), "nfs://10.0.0.5/e");
        assert_eq!(canonical_host("Host."), "host");
    }

    #[test]
    fn default_port_is_the_absent_port() {
        assert_eq!(p("nfs://h:2049/e"), p("nfs://h/e"));
        assert_eq!(p("nfs://h:2049/e").url(), "nfs://h/e");
        assert_eq!(p("nfs://[fd00::1]:2049/e").url(), "nfs://[fd00::1]/e");
        let u = p("nfs://10.0.0.5:2050/export");
        assert_eq!(u.port, Some(2050));
        assert_eq!(u.url(), "nfs://10.0.0.5:2050/export");
    }

    #[test]
    fn keeps_port_ipv6_and_query() {
        let u = p("nfs://10.0.0.5:2050/export?version=3&uid=0");
        assert_eq!(u.host, "10.0.0.5");
        assert_eq!(u.port, Some(2050));
        assert_eq!(
            u.query.as_deref(),
            Some("uid=0&version=3"),
            "options sorted"
        );
        assert_eq!(u.url(), "nfs://10.0.0.5:2050/export?uid=0&version=3");
        assert_eq!(
            p("nfs://h/e?uid=0&version=3"),
            p("nfs://h/e?version=3&uid=0&version=3"),
            "order and repeats do not matter"
        );
        assert_eq!(p("nfs://h/e?").query, None);

        let u = p("nfs://[fd00::1]:2051/export");
        assert_eq!(u.host, "fd00::1");
        assert_eq!(u.port, Some(2051));
        assert_eq!(u.url(), "nfs://[fd00::1]:2051/export");
    }

    #[test]
    fn rejects_non_nfs_and_incomplete_urls() {
        for bad in [
            "old.example.com:/export",
            "http://h/export",
            "nfs://",
            "nfs:///export",
            "nfs://h",
            "nfs://h:notaport/export",
            "nfs://[fd00::1/export",
            "nfs://./export",
        ] {
            let err = parse("--dst", bad).unwrap_err();
            assert!(format!("{err:#}").contains("--dst"), "{bad}: {err:#}");
        }
    }

    #[test]
    fn same_host_ignores_port_and_options() {
        assert!(p("nfs://h/a").same_host(&p("nfs://H:2050/b?uid=0")));
        assert!(!p("nfs://h/a").same_host(&p("nfs://h2/a")));
    }

    #[test]
    fn relate_paths_matrix() {
        let src = p("nfs://h/export/data");
        assert_eq!(
            relate_paths(&src, &p("nfs://h/export/data/")),
            PathRelation::Same
        );
        assert_eq!(
            relate_paths(&src, &p("nfs://h/export/data/backup")),
            PathRelation::DestInsideSource
        );
        assert_eq!(
            relate_paths(&src, &p("nfs://h/export")),
            PathRelation::SourceInsideDest
        );
        assert_eq!(
            relate_paths(&src, &p("nfs://h/")),
            PathRelation::SourceInsideDest
        );
        assert_eq!(
            relate_paths(&src, &p("nfs://h/export/data2")),
            PathRelation::Disjoint
        );
        assert_eq!(
            relate_paths(&src, &p("nfs://h/other/data")),
            PathRelation::Disjoint
        );
    }

    #[test]
    fn overlap_refuses_same_nested_or_parent_paths_on_one_server() {
        let src = p("nfs://h.example.com/export/data");
        for dst in [
            "nfs://h.example.com/export/data",
            "nfs://h.example.com/export/data/",
            "nfs://H.EXAMPLE.COM/export//data",
            "nfs://h.example.com./export/data",
            "nfs://h.example.com:2049/export/data",
            "nfs://h.example.com:2050/export/data",
            "nfs://h.example.com/export/data?version=3",
            "nfs://h.example.com/export/data/backup",
            "nfs://h.example.com/export",
            "nfs://h.example.com/",
        ] {
            let err = check_overlap(&src, &p(dst)).unwrap_err();
            assert!(format!("{err:#}").contains("overlap"), "{dst}: {err:#}");
            assert!(
                format!("{err:#}").contains("same host name"),
                "{dst}: {err:#}"
            );
        }
        // IP literal spellings.
        let src = p("nfs://10.0.0.5/export");
        for dst in [
            "nfs://[::ffff:10.0.0.5]/export/x",
            "nfs://10.0.0.5:2049/export",
        ] {
            assert!(check_overlap(&src, &p(dst)).is_err(), "{dst}");
        }
        let src = p("nfs://[fd00::1]/export");
        assert!(check_overlap(&src, &p("nfs://[FD00:0:0:0:0:0:0:1]/export/sub")).is_err());
    }

    #[test]
    fn overlap_allows_siblings_and_other_hosts_by_name() {
        let src = p("nfs://h/export/data");
        for dst in [
            "nfs://h/export/data2",
            "nfs://h/export/copy-of-data",
            "nfs://h/other/data",
            "nfs://h:2050/export/other",
            "nfs://h2/export/data",
        ] {
            check_overlap(&src, &p(dst)).unwrap_or_else(|e| panic!("{dst}: {e:#}"));
        }
    }

    // ---- name resolution --------------------------------------------

    #[test]
    fn literals_resolve_to_themselves_without_dns() {
        assert_eq!(
            resolve_host("10.0.0.5").unwrap(),
            BTreeSet::from(["10.0.0.5".parse::<IpAddr>().unwrap()])
        );
        assert_eq!(
            resolve_host("::ffff:10.0.0.5").unwrap(),
            BTreeSet::from(["10.0.0.5".parse::<IpAddr>().unwrap()])
        );
    }

    #[test]
    fn loopback_alias_is_the_same_server() {
        // `localhost` resolves through /etc/hosts, never the network.
        let local = resolve_host("localhost").unwrap();
        assert!(!local.is_empty());
        let literal = local.iter().next().unwrap().to_string();
        let src = p("nfs://localhost/export/data");
        let dst = p(&format!("nfs://{}/export/data", bracket(&literal)));
        assert!(!src.same_host(&dst), "different spellings");
        let err = check_overlap_resolved(&src, &dst).unwrap_err();
        assert!(
            format!("{err:#}").contains("both names resolve to"),
            "{err:#}"
        );

        // Disjoint paths on the alias pair are allowed but flagged as
        // possibly one server, so the mounted check runs.
        let ev = check_overlap_resolved(&src, &p(&format!("nfs://{}/other", bracket(&literal))))
            .unwrap();
        assert!(ev.may_be_same_server());
        assert!(!ev.shared.is_empty());
    }

    fn bracket(ip: &str) -> String {
        if ip.contains(':') {
            format!("[{ip}]")
        } else {
            ip.to_string()
        }
    }

    #[test]
    fn same_host_name_skips_resolution() {
        // `no-such-host.invalid` cannot resolve; one name is enough.
        let src = p("nfs://no-such-host.invalid/a");
        let ev = resolve_pair(&src, &p("nfs://NO-SUCH-HOST.invalid./b")).unwrap();
        assert!(ev.same_host && ev.may_be_same_server());
        let err = check_overlap_resolved(&src, &p("nfs://no-such-host.invalid/a/sub")).unwrap_err();
        assert!(format!("{err:#}").contains("same host name"), "{err:#}");
    }

    #[test]
    fn distinct_literals_are_distinct_by_name() {
        let ev = check_overlap_resolved(&p("nfs://127.0.0.1/e"), &p("nfs://127.0.0.2/e")).unwrap();
        assert!(!ev.may_be_same_server());
        assert!(
            ev.describe().starts_with("different addresses"),
            "{}",
            ev.describe()
        );
    }

    #[test]
    fn unresolvable_name_is_an_error_not_a_pass() {
        let err =
            resolve_pair(&p("nfs://no-such-host.invalid/e"), &p("nfs://10.0.0.5/e")).unwrap_err();
        assert!(
            format!("{err:#}").contains("resolving no-such-host.invalid"),
            "{err:#}"
        );
    }
}
