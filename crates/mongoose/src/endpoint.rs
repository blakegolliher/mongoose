//! `--src` / `--dst` handling: parse and normalize `nfs://` URLs, and
//! refuse a source/destination pair on the same server whose paths
//! overlap.
//!
//! The engine's own guard (`migration_core::overlap`) compares an
//! export URL plus a root *inside* it, and treats any two different
//! URLs as disjoint. mongoose lets operators put the whole path in
//! the URL (`nfs://host/export/sub`), so `nfs://h/export` vs
//! `nfs://h/export/backup` must be caught here — that is exactly the
//! truncate-your-source misconfiguration the engine guard exists for.

use anyhow::{bail, Context, Result};

/// A parsed `nfs://server[:port]/path[?opts]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NfsUrl {
    pub host: String,
    pub port: Option<u16>,
    /// Absolute, no trailing slash except for the bare root `/`.
    pub path: String,
    /// libnfs URL options after `?`, passed through untouched.
    pub query: Option<String>,
}

impl NfsUrl {
    /// Canonical form handed to the engine (walker mount + mover mount).
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

    fn components(&self) -> Vec<&str> {
        self.path.split('/').filter(|c| !c.is_empty()).collect()
    }

    fn same_server(&self, other: &NfsUrl) -> bool {
        self.host.eq_ignore_ascii_case(&other.host) && self.port == other.port
    }
}

/// Parse an operator-supplied URL. Accepts what libnfs accepts
/// (`nfs://host/path`, `nfs://host:port/path`, `nfs://[v6]/path`,
/// `?opt=val` suffixes) and rejects anything that is not an NFS URL
/// with a clear message naming the flag.
pub fn parse(flag: &str, raw: &str) -> Result<NfsUrl> {
    let raw = raw.trim();
    let rest = match raw.strip_prefix("nfs://") {
        Some(r) => r,
        None => bail!("{flag} must look like nfs://server/export[/path], got {raw:?}"),
    };
    let (rest, query) = match rest.split_once('?') {
        Some((r, q)) => (r, Some(q.to_string())),
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
    if host.is_empty() {
        bail!("{flag} is missing the server name: nfs://server/export[/path], got {raw:?}");
    }

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

/// Refuse a destination that is the source, contains it, or lies
/// inside it, on the same server. Different servers are assumed
/// disjoint (an alias or a second address for the same box cannot be
/// detected here; the mover's per-file self-target check remains the
/// last line of defense).
pub fn check_overlap(src: &NfsUrl, dst: &NfsUrl) -> Result<()> {
    if !src.same_server(dst) {
        return Ok(());
    }
    let s = src.components();
    let d = dst.components();
    let problem = if s == d {
        "source and destination are the same path"
    } else if d.starts_with(&s) {
        "the destination is inside the source: the copy would write into the tree it is reading"
    } else if s.starts_with(&d) {
        "the source is inside the destination: the copy would overwrite the tree it is reading"
    } else {
        return Ok(());
    };
    bail!(
        "source and destination overlap; refusing to start\n  \
         source: {}\n  \
         dest:   {}\n\n  \
         Problem: {problem}.\n\n  \
         Fix: copy to a different server, or to a path on this server that is\n  \
         neither inside the source nor a parent of it.",
        src.url(),
        dst.url(),
    )
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
    fn keeps_port_ipv6_and_query() {
        let u = p("nfs://10.0.0.5:2049/export?version=3&uid=0");
        assert_eq!(u.host, "10.0.0.5");
        assert_eq!(u.port, Some(2049));
        assert_eq!(u.query.as_deref(), Some("version=3&uid=0"));
        assert_eq!(u.url(), "nfs://10.0.0.5:2049/export?version=3&uid=0");

        let u = p("nfs://[fd00::1]:2049/export");
        assert_eq!(u.host, "fd00::1");
        assert_eq!(u.port, Some(2049));
        assert_eq!(u.url(), "nfs://[fd00::1]:2049/export");

        let u = p("nfs://[fd00::1]/export");
        assert_eq!(u.port, None);
        assert_eq!(u.url(), "nfs://[fd00::1]/export");
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
        ] {
            let err = parse("--dst", bad).unwrap_err();
            assert!(format!("{err:#}").contains("--dst"), "{bad}: {err:#}");
        }
    }

    #[test]
    fn overlap_refuses_same_nested_or_parent_paths_on_one_server() {
        let src = p("nfs://h/export/data");
        for dst in [
            "nfs://h/export/data",
            "nfs://h/export/data/",
            "nfs://H/export//data",
            "nfs://h/export/data/backup",
            "nfs://h/export",
            "nfs://h/",
        ] {
            let err = check_overlap(&src, &p(dst)).unwrap_err();
            assert!(format!("{err:#}").contains("overlap"), "{dst}: {err:#}");
        }
    }

    #[test]
    fn overlap_allows_siblings_other_servers_and_other_ports() {
        let src = p("nfs://h/export/data");
        for dst in [
            "nfs://h/export/data2",
            "nfs://h/export/copy-of-data",
            "nfs://h/other/data",
            "nfs://h2/export/data",
            "nfs://h:2050/export/data",
        ] {
            check_overlap(&src, &p(dst)).unwrap_or_else(|e| panic!("{dst}: {e:#}"));
        }
    }
}
