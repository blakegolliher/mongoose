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
//!    intersect may be one server. A round-robin DNS name answers each
//!    query with one of its addresses, so a name's address set is
//!    collected over repeated queries ([`resolve_host`]); the same set
//!    spreads the copy's connections over every address
//!    ([`connection_targets`]).
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
use std::collections::{BTreeSet, HashMap};
use std::net::{IpAddr, ToSocketAddrs};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

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
///
/// A round-robin DNS name, which is how a VAST cluster publishes its
/// virtual IPs, answers each query with one address. So beyond the
/// system resolver the name is queried repeatedly until it stops
/// showing new addresses ([`discover_addresses`]), once per process
/// and name. A name that resolves only to loopback (`localhost`, from
/// `/etc/hosts`) is not queried further, and DNS answers are added only
/// when they include an address the system resolver gave: a name
/// pinned in `/etc/hosts` to an address DNS does not serve keeps the
/// pin.
pub fn resolve_host(host: &str) -> Result<BTreeSet<IpAddr>> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(BTreeSet::from([canonical_ip(ip)]));
    }
    let addrs = (host, NFS_PORT)
        .to_socket_addrs()
        .with_context(|| format!("resolving {host}"))?;
    let set: BTreeSet<IpAddr> = addrs.map(|a| canonical_ip(a.ip())).collect();
    anyhow::ensure!(!set.is_empty(), "{host} resolved to no addresses");
    if set.iter().all(IpAddr::is_loopback) {
        return Ok(set);
    }
    Ok(merge_dns_answers(set, discovered_addresses(host)))
}

/// [`discover_addresses`] for `host` through the `host` command, once
/// per process: discovery against a time-rotated name takes seconds,
/// and one command resolves the same name several times (scan,
/// endpoint check, connection pools).
fn discovered_addresses(host: &str) -> BTreeSet<IpAddr> {
    static CACHE: OnceLock<Mutex<HashMap<String, BTreeSet<IpAddr>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    // Held across the discovery, so two threads asking for one name
    // wait for one search instead of running two.
    let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
    cache
        .entry(host.to_string())
        .or_insert_with(|| {
            let start = Instant::now();
            discover_addresses(|| dns_query(host), || start.elapsed())
        })
        .clone()
}

/// The system resolver's addresses for a name, plus those DNS served
/// when the two agree on at least one. Disjoint sets mean the system
/// resolver is not using DNS for this name (an `/etc/hosts` entry, a
/// split view), and its answer stands alone.
fn merge_dns_answers(system: BTreeSet<IpAddr>, dns: BTreeSet<IpAddr>) -> BTreeSet<IpAddr> {
    if system.is_disjoint(&dns) {
        return system;
    }
    system.union(&dns).copied().collect()
}

/// At least this many queries in a row must show nothing new before
/// the search stops; more for a name with more addresses (see
/// [`discover_addresses`]).
const DISCOVERY_MIN_STALE: usize = 16;
/// A name whose answer has never changed is done once it has gone this
/// long without a new address: longer than one slice of a time-rotated
/// name (under a second on a VAST cluster).
const DISCOVERY_STEADY_QUIET: Duration = Duration::from_secs(3);
/// A name whose answer changes is done after a full rotation without a
/// new address, or, should its answers stop changing, after this long
/// without one: longer than a DNS server's stall.
const DISCOVERY_ROTATING_QUIET: Duration = Duration::from_secs(10);
/// Hard caps for one name.
const DISCOVERY_MAX_QUERIES: usize = 1024;
const DISCOVERY_MAX_TIME: Duration = Duration::from_secs(20);
/// Failed queries, before any address is found, that end the search:
/// DNS does not answer for the name (it is listed only in `/etc/hosts`,
/// or no DNS server is reachable), and each failure can cost a timeout.
const DISCOVERY_MAX_EARLY_FAILURES: usize = 2;

/// Collect the addresses of a round-robin name by querying it until it
/// stops answering with new ones. `query` returns one answer's
/// addresses, or `None` when no query can be made at all (no DNS
/// client), which ends the search; `elapsed` is the time since the
/// search began.
///
/// Round-robin servers rotate in two ways, and the search waits out
/// both. Some pick an address per query: the search waits for
/// `max(16, 8 * found)` stale answers in a row, after which the chance
/// that a particular one of `k` uniformly served addresses is still
/// unseen is about `e^-8` (0.03%). Others, a VAST cluster's DNS among
/// them, serve one address for a fraction of a second, then the next,
/// in a fixed cycle: every query in a slice repeats its address, so
/// the search counts changes of answer, not queries, and stops once
/// as many changes as there are addresses (a full cycle) brought
/// nothing new. A name whose answer never changes (one address, or
/// every address in each answer) stops after 3 seconds without news; a
/// rotating one whose answers stop changing, after 10. Two failed
/// queries before any address is found end the search early; it never
/// runs past 1024 queries or 20 seconds.
pub fn discover_addresses(
    mut query: impl FnMut() -> Option<Vec<IpAddr>>,
    mut elapsed: impl FnMut() -> Duration,
) -> BTreeSet<IpAddr> {
    let mut found = BTreeSet::new();
    let mut previous: Option<BTreeSet<IpAddr>> = None;
    let mut rotating = false;
    let mut stale_queries = 0usize;
    let mut stale_changes = 0usize;
    let mut last_new = Duration::ZERO;
    let mut early_failures = 0usize;
    for _ in 0..DISCOVERY_MAX_QUERIES {
        let Some(answer) = query() else { break };
        let now = elapsed();
        let answer: BTreeSet<IpAddr> = answer.into_iter().map(canonical_ip).collect();
        if answer.is_empty() && found.is_empty() {
            early_failures += 1;
            if early_failures >= DISCOVERY_MAX_EARLY_FAILURES {
                break;
            }
        } else if answer.is_empty() {
            stale_queries += 1;
        } else {
            let changed = previous.as_ref().is_some_and(|p| *p != answer);
            rotating |= changed;
            let before = found.len();
            found.extend(answer.iter().copied());
            previous = Some(answer);
            if found.len() > before {
                last_new = now;
                stale_queries = 0;
                stale_changes = 0;
            } else {
                stale_queries += 1;
                stale_changes += usize::from(changed);
            }
        }
        if !found.is_empty() && stale_queries >= DISCOVERY_MIN_STALE.max(8 * found.len()) {
            let quiet = now.saturating_sub(last_new);
            let done = if rotating {
                stale_changes >= found.len() || quiet >= DISCOVERY_ROTATING_QUIET
            } else {
                quiet >= DISCOVERY_STEADY_QUIET
            };
            if done {
                break;
            }
        }
        if now >= DISCOVERY_MAX_TIME {
            break;
        }
    }
    found
}

/// One DNS query for `host` through the configured name server, past
/// the C library's resolver: `host -W 2 <name>`, the tool the embedded
/// scanner uses for the same purpose. `None` when `host` cannot be
/// run; an answer with no addresses (a failed query) is `Some(vec![])`.
fn dns_query(host: &str) -> Option<Vec<IpAddr>> {
    let out = std::process::Command::new("host")
        .args(["-W", "2", host])
        .output()
        .ok()?;
    Some(parse_host_output(&String::from_utf8_lossy(&out.stdout)))
}

/// The addresses in `host` output: lines ending `has address A` or
/// `has IPv6 address A`.
fn parse_host_output(out: &str) -> Vec<IpAddr> {
    out.lines()
        .filter(|l| l.contains(" has address ") || l.contains(" has IPv6 address "))
        .filter_map(|l| l.split_whitespace().last()?.parse().ok())
        .collect()
}

/// Where to open the libnfs connections to each side.
///
/// Each list holds every address of that side's host name, in a stable
/// order; the copy and verification pools give connection pair `i` the
/// address at `i` modulo the list's length, so a round-robin cluster
/// carries the load on all of its addresses instead of the one the
/// resolver happened to return. An empty list means connect by the
/// name, as before: a URL with an explicit port, or a name whose
/// addresses could not be listed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConnectionTargets {
    pub src: Vec<String>,
    pub dst: Vec<String>,
}

/// One line for a mounted pool: how many connection pairs there are,
/// and how many connections each address carries on each side.
/// `pairs` is one (source, destination) entry per pair; `None` is a
/// connection made by host name.
pub fn describe_pool(pairs: &[(Option<String>, Option<String>)]) -> String {
    let (src_n, src) = tally(pairs.iter().map(|p| p.0.as_deref()));
    let (dst_n, dst) = tally(pairs.iter().map(|p| p.1.as_deref()));
    let over = |n: usize| {
        if n == 1 {
            "1 address".to_string()
        } else {
            format!("{n} addresses")
        }
    };
    if src == dst {
        format!(
            "{} pairs, each side over {}: {src}",
            pairs.len(),
            over(src_n)
        )
    } else {
        format!(
            "{} pairs; source over {}: {src}; destination over {}: {dst}",
            pairs.len(),
            over(src_n),
            over(dst_n)
        )
    }
}

/// How many distinct addresses one side's connections use, and each
/// address with its connection count ("172.200.204.1 x6, ...").
fn tally<'a>(addrs: impl Iterator<Item = Option<&'a str>>) -> (usize, String) {
    let mut counts: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for addr in addrs {
        *counts.entry(addr.unwrap_or("host name")).or_insert(0) += 1;
    }
    let list = counts
        .iter()
        .map(|(addr, n)| format!("{addr} x{n}"))
        .collect::<Vec<_>>()
        .join(", ");
    (counts.len(), list)
}

/// The connection targets for a checked endpoint pair. Two names reuse
/// the address sets [`resolve_pair`] already collected; one name is
/// resolved here, once, for both sides.
pub fn connection_targets(src: &NfsUrl, dst: &NfsUrl, names: &NameEvidence) -> ConnectionTargets {
    let list = |set: &BTreeSet<IpAddr>| set.iter().map(IpAddr::to_string).collect::<Vec<_>>();
    let shared_name = if names.same_host {
        match resolve_host(&src.host) {
            Ok(set) => list(&set),
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), host = %src.host, "could not list the server's addresses; connecting by name");
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };
    let side = |url: &NfsUrl, addrs: &BTreeSet<IpAddr>| {
        if url.port.is_some() {
            Vec::new()
        } else if names.same_host {
            shared_name.clone()
        } else {
            list(addrs)
        }
    };
    ConnectionTargets {
        src: side(src, &names.src_addrs),
        dst: side(dst, &names.dst_addrs),
    }
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

    // ---- round-robin discovery and connection targets -----------------

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    /// A clock that moves `step_ms` forward each time it is read.
    fn ticking(step_ms: u64) -> impl FnMut() -> Duration {
        let mut now = Duration::ZERO;
        move || {
            now += Duration::from_millis(step_ms);
            now
        }
    }

    /// A name that answers each query with one of its addresses, picked
    /// pseudo-randomly per query.
    fn round_robin(addrs: &[&str]) -> impl FnMut() -> Option<Vec<IpAddr>> {
        let addrs: Vec<IpAddr> = addrs.iter().map(|a| ip(a)).collect();
        let mut state = 0x9e37_79b9_u64;
        move || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            Some(vec![addrs[(state >> 33) as usize % addrs.len()]])
        }
    }

    const VIPS: [&str; 6] = [
        "172.200.204.1",
        "172.200.204.2",
        "172.200.204.3",
        "172.200.204.4",
        "172.200.204.5",
        "172.200.204.6",
    ];

    fn all_vips() -> BTreeSet<IpAddr> {
        VIPS.iter().map(|a| ip(a)).collect()
    }

    #[test]
    fn discovery_finds_every_address_of_a_per_query_round_robin_name() {
        let found = discover_addresses(round_robin(&VIPS), ticking(30));
        assert_eq!(found, all_vips());

        let many: Vec<String> = (1..=40).map(|i| format!("10.1.0.{i}")).collect();
        let many: Vec<&str> = many.iter().map(String::as_str).collect();
        assert_eq!(
            discover_addresses(round_robin(&many), ticking(30)).len(),
            40
        );
    }

    /// var204's DNS: one address at a time for a slice of 0.2 to 1.8 s,
    /// in a fixed cycle, so every query inside a slice repeats it. At one
    /// query per 30 ms a run of 16 stale answers ends long before the
    /// slice does; only counting changes of answer finds them all.
    fn time_sliced(
        slices: &'static [(usize, u64)],
    ) -> (
        impl FnMut() -> Option<Vec<IpAddr>>,
        impl FnMut() -> Duration,
        std::rc::Rc<std::cell::Cell<u64>>,
    ) {
        let period: u64 = slices.iter().map(|&(_, ms)| ms).sum();
        let now = std::rc::Rc::new(std::cell::Cell::new(0u64));
        let query = {
            let now = now.clone();
            move || {
                now.set(now.get() + 30);
                let mut t = now.get() % period;
                for &(vip, ms) in slices {
                    if t < ms {
                        return Some(vec![ip(VIPS[vip])]);
                    }
                    t -= ms;
                }
                unreachable!("t is below the period")
            }
        };
        let clock = {
            let now = now.clone();
            move || Duration::from_millis(now.get())
        };
        (query, clock, now)
    }

    #[test]
    fn discovery_finds_every_address_of_a_time_sliced_rotation() {
        // The order and slice lengths var204 showed.
        const CYCLE: &[(usize, u64)] =
            &[(5, 330), (4, 870), (0, 840), (3, 1000), (1, 1000), (2, 350)];
        let (query, clock, now) = time_sliced(CYCLE);
        assert_eq!(discover_addresses(query, clock), all_vips());
        // All six are in by 4.0 s; one more cycle (4.4 s) confirms it.
        assert!(now.get() < 9_000, "stopped after {} ms", now.get());
    }

    #[test]
    fn a_stalled_slice_does_not_end_a_time_sliced_search_early() {
        // The server holds one address for 4 s partway through.
        const STALLED: &[(usize, u64)] =
            &[(5, 330), (4, 870), (0, 840), (3, 4000), (1, 1000), (2, 350)];
        let (query, clock, _) = time_sliced(STALLED);
        assert_eq!(discover_addresses(query, clock), all_vips());
    }

    #[test]
    fn discovery_of_a_name_that_never_changes_stops_after_a_quiet_spell() {
        // Slow queries: the stale count is what the search waits for.
        let mut queries = 0;
        let found = discover_addresses(
            || {
                queries += 1;
                Some(vec![ip("10.0.0.7")])
            },
            ticking(1000),
        );
        assert_eq!(found, BTreeSet::from([ip("10.0.0.7")]));
        assert_eq!(
            queries,
            1 + DISCOVERY_MIN_STALE,
            "one new answer, then 16 stale"
        );

        // Fast queries: the quiet time is (3 s at 10 ms a query).
        let mut queries = 0;
        discover_addresses(
            || {
                queries += 1;
                Some(vec![ip("10.0.0.7")])
            },
            ticking(10),
        );
        assert_eq!(queries, 1 + 300, "one new answer, then 3 s of stale ones");

        // Every address in every answer: found at once, then 3 s.
        let found = discover_addresses(|| Some(vec![ip("10.0.0.7"), ip("10.0.0.8")]), ticking(10));
        assert_eq!(found.len(), 2);
    }

    #[test]
    fn discovery_never_runs_past_its_time_cap() {
        let mut next = 0u32;
        let found = discover_addresses(
            || {
                next += 1;
                Some(vec![IpAddr::from(std::net::Ipv4Addr::from(
                    0x0a00_0000 + next,
                ))])
            },
            ticking(100),
        );
        assert_eq!(found.len(), 200, "20 s at 100 ms a query");
    }

    #[test]
    fn discovery_without_a_dns_client_finds_nothing_and_makes_one_attempt() {
        let mut queries = 0;
        let found = discover_addresses(
            || {
                queries += 1;
                None
            },
            ticking(10),
        );
        assert!(found.is_empty());
        assert_eq!(queries, 1);
    }

    #[test]
    fn discovery_collapses_mapped_v4_and_ignores_failed_answers() {
        let mut answers = vec![
            Some(vec![]),
            Some(vec![ip("::ffff:10.0.0.9")]),
            Some(vec![ip("10.0.0.9"), ip("fd00::9")]),
        ]
        .into_iter();
        let found = discover_addresses(|| answers.next().unwrap_or(Some(vec![])), ticking(10));
        assert_eq!(found, BTreeSet::from([ip("10.0.0.9"), ip("fd00::9")]));
    }

    #[test]
    fn discovery_gives_up_quickly_on_a_name_dns_does_not_answer() {
        let mut queries = 0;
        let found = discover_addresses(
            || {
                queries += 1;
                Some(vec![])
            },
            ticking(2000),
        );
        assert!(found.is_empty());
        assert_eq!(queries, DISCOVERY_MAX_EARLY_FAILURES);
    }

    #[test]
    fn dns_answers_extend_the_system_resolver_only_when_they_agree() {
        let set = |addrs: &[&str]| addrs.iter().map(|a| ip(a)).collect::<BTreeSet<_>>();
        // A cached round-robin answer: DNS shows the rest of the name.
        assert_eq!(
            merge_dns_answers(
                set(&["172.200.204.4"]),
                set(&["172.200.204.1", "172.200.204.4"])
            ),
            set(&["172.200.204.1", "172.200.204.4"])
        );
        // An /etc/hosts pin DNS knows nothing about stays the only address.
        assert_eq!(
            merge_dns_answers(set(&["10.9.9.9"]), set(&["172.200.204.1", "172.200.204.4"])),
            set(&["10.9.9.9"])
        );
        // No DNS answer at all.
        assert_eq!(
            merge_dns_answers(set(&["10.9.9.9"]), set(&[])),
            set(&["10.9.9.9"])
        );
    }

    #[test]
    fn host_output_is_parsed_for_v4_and_v6_addresses_only() {
        let out = "main.selab-var204.selab.vastdata.com has address 172.200.204.2\n\
                   alias.example is an alias for main.example.\n\
                   main.example has IPv6 address fd00::4\n\
                   main.example mail is handled by 10 mx.example.\n";
        assert_eq!(
            parse_host_output(out),
            vec![ip("172.200.204.2"), ip("fd00::4")]
        );
        assert!(parse_host_output("Host x not found: 3(NXDOMAIN)\n").is_empty());
    }

    fn evidence(same_host: bool, src: &[&str], dst: &[&str]) -> NameEvidence {
        let set = |v: &[&str]| v.iter().map(|a| ip(a)).collect::<BTreeSet<_>>();
        let (src_addrs, dst_addrs) = (set(src), set(dst));
        let shared = src_addrs.intersection(&dst_addrs).copied().collect();
        NameEvidence {
            same_host,
            src_addrs,
            dst_addrs,
            shared,
        }
    }

    #[test]
    fn two_names_use_the_address_sets_already_collected() {
        let t = connection_targets(
            &p("nfs://old.example/export"),
            &p("nfs://new.example/export"),
            &evidence(false, &["10.0.0.2", "10.0.0.1"], &["10.9.0.1"]),
        );
        assert_eq!(t.src, ["10.0.0.1", "10.0.0.2"], "stable order");
        assert_eq!(t.dst, ["10.9.0.1"]);
    }

    #[test]
    fn an_explicit_port_connects_by_name() {
        let t = connection_targets(
            &p("nfs://old.example:2050/export"),
            &p("nfs://new.example/export"),
            &evidence(false, &["10.0.0.1"], &["10.9.0.1"]),
        );
        assert!(t.src.is_empty());
        assert_eq!(t.dst, ["10.9.0.1"]);
    }

    #[test]
    fn one_literal_host_is_its_own_single_target() {
        let src = p("nfs://127.0.0.1/source");
        let dst = p("nfs://127.0.0.1/dest");
        let names = resolve_pair(&src, &dst).unwrap();
        let t = connection_targets(&src, &dst, &names);
        assert_eq!(t.src, ["127.0.0.1"]);
        assert_eq!(t.dst, ["127.0.0.1"]);
    }

    #[test]
    fn an_unresolvable_shared_name_connects_by_name() {
        let src = p("nfs://no-such-host.invalid/a");
        let dst = p("nfs://no-such-host.invalid/b");
        let names = resolve_pair(&src, &dst).unwrap();
        let t = connection_targets(&src, &dst, &names);
        assert_eq!(t, ConnectionTargets::default());
    }

    #[test]
    fn pool_description_counts_connections_per_address() {
        let pairs: Vec<(Option<String>, Option<String>)> = (0..8)
            .map(|i| {
                let a = format!("10.0.0.{}", i % 3 + 1);
                (Some(a.clone()), Some(a))
            })
            .collect();
        assert_eq!(
            describe_pool(&pairs),
            "8 pairs, each side over 3 addresses: 10.0.0.1 x3, 10.0.0.2 x3, 10.0.0.3 x2"
        );
        let by_name = vec![(None, Some("10.9.0.1".to_string())); 2];
        assert_eq!(
            describe_pool(&by_name),
            "2 pairs; source over 1 address: host name x2; destination over 1 address: 10.9.0.1 x2"
        );
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
