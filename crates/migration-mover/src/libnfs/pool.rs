//! `LibnfsContextPool` trait and the `SimplePool` implementation used
//! in M2.
//!
//! Per the M2 design (decision #1), the pool surface is built now even
//! though M2 only ever holds one source/dest pair. Bolting concurrency
//! onto a `Mutex<Pair>` later is the kind of refactor that breaks
//! subtle correctness invariants — better to introduce the trait
//! before the data path needs it.
//!
//! The M2 `SimplePool` holds a single pre-mounted pair returned to a
//! tokio `mpsc::UnboundedChannel` on guard drop. Acquire blocks if the
//! pair is held. M3 reshapes this into an N-pair pool driving
//! concurrent copies; the trait surface should not need to change.

use super::NfsContext;
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

/// Single in-flight (src, dst) pair handed out by [`LibnfsContextPool`].
/// Returns the pair to the pool on drop.
pub struct ContextPair {
    src: Option<NfsContext>,
    dst: Option<NfsContext>,
    return_to: mpsc::UnboundedSender<(NfsContext, NfsContext)>,
}

impl ContextPair {
    /// Mutable borrow of the source context. Caller is the sole user
    /// of this context until the guard drops, satisfying libnfs's
    /// no-concurrent-use rule.
    pub fn src(&mut self) -> &mut NfsContext {
        self.src.as_mut().expect("ContextPair::src after drop")
    }

    /// Mutable borrow of the destination context.
    pub fn dst(&mut self) -> &mut NfsContext {
        self.dst.as_mut().expect("ContextPair::dst after drop")
    }

    /// Disjoint mutable borrows of (src, dst) so two callers — e.g.
    /// the read closure and the write closure of `stream_copy` — can
    /// each hold an independent `&mut NfsContext` without a second
    /// borrow of the whole pair. Both contexts must be present.
    pub fn split(&mut self) -> (&mut NfsContext, &mut NfsContext) {
        let s = self
            .src
            .as_mut()
            .expect("ContextPair::split src after drop");
        let d = self
            .dst
            .as_mut()
            .expect("ContextPair::split dst after drop");
        (s, d)
    }
}

#[cfg(test)]
impl ContextPair {
    /// Test-only pair with no mounted contexts. For unit tests of
    /// strategy arms that never touch the src/dst contexts and of
    /// `run_with_pair`'s outcome assembly
    /// (F41). Calling `src()`/`dst()`/`split()` on it panics; `Drop`
    /// is a no-op (both slots are `None`).
    pub(crate) fn unmounted_for_tests() -> Self {
        let (tx, _rx) = mpsc::unbounded_channel();
        Self {
            src: None,
            dst: None,
            return_to: tx,
        }
    }
}

impl Drop for ContextPair {
    fn drop(&mut self) {
        let src = self.src.take();
        let dst = self.dst.take();
        if let (Some(s), Some(d)) = (src, dst) {
            // UnboundedSender::send only fails if the receiver was
            // dropped, which means the pool itself is gone — no
            // recipient cares about our pair anymore.
            let _ = self.return_to.send((s, d));
        }
    }
}

#[async_trait]
pub trait LibnfsContextPool: Send + Sync {
    /// Acquire a (src, dst) pair. Awaits if every pair is currently
    /// held by another task. Caller drops the returned guard to
    /// release.
    async fn acquire(&self) -> anyhow::Result<ContextPair>;
}

/// M2 single-pair pool. Built once at worker startup with the source
/// and destination URLs from the manifest. Equivalent to
/// `MultiPool::build(.., 1)` and kept as a small specialization for
/// tests and very-low-concurrency configurations.
pub struct SimplePool {
    inner: MultiPool,
}

impl SimplePool {
    /// `rpc_timeout_ms` (F12): per-RPC timeout applied to both
    /// contexts at creation; `0` = leave the libnfs default. See
    /// [`super::DEFAULT_RPC_TIMEOUT_MS`].
    pub fn build(src_url: &str, dst_url: &str, rpc_timeout_ms: u32) -> anyhow::Result<Arc<Self>> {
        Ok(Arc::new(Self {
            inner: MultiPool::build_inner(src_url, dst_url, &[], &[], 1, rpc_timeout_ms)?,
        }))
    }
}

#[async_trait]
impl LibnfsContextPool for SimplePool {
    async fn acquire(&self) -> anyhow::Result<ContextPair> {
        self.inner.acquire().await
    }
}

/// M3 multi-pair pool. Pre-mounts N (src, dst) context pairs and hands
/// them out to concurrent acquirers. Pairs are returned via the
/// guard's Drop, same as `SimplePool`. The Mutex around the receiver
/// serializes acquire moments but each acquire is a non-blocking
/// channel `recv()` once a pair is available, so contention is on the
/// (cheap) lock, not the (expensive) work.
pub struct MultiPool {
    /// Per pair: the (source, destination) address it connected to, or
    /// `None` for the URL's host name. For the operator's log.
    targets: Vec<(Option<String>, Option<String>)>,
    receiver: Mutex<mpsc::UnboundedReceiver<(NfsContext, NfsContext)>>,
    sender: mpsc::UnboundedSender<(NfsContext, NfsContext)>,
    capacity: usize,
}

impl MultiPool {
    /// Mount `n` (src, dst) pairs against the same URLs and return the
    /// pool. Mount failures during seeding return early — already-mounted
    /// pairs drop cleanly via `NfsContext::Drop`.
    ///
    /// `rpc_timeout_ms` (F12): per-RPC timeout applied to every
    /// context at creation; `0` = leave the libnfs default. See
    /// [`super::DEFAULT_RPC_TIMEOUT_MS`].
    pub fn build(
        src_url: &str,
        dst_url: &str,
        n: usize,
        rpc_timeout_ms: u32,
    ) -> anyhow::Result<Arc<Self>> {
        Self::build_spread(src_url, dst_url, &[], &[], n, rpc_timeout_ms)
    }

    /// [`Self::build`], spreading each side's connections over several
    /// addresses of its server. Pair `i` connects its source to
    /// `src_targets[i % len]` and its destination to
    /// `dst_targets[i % len]`, so `n` pairs land evenly on every
    /// address. An empty list connects that side by the URL's host
    /// name, as [`Self::build`] does.
    ///
    /// An address that fails to mount is logged and skipped for the
    /// rest of the pool; its pairs go to the next address in turn. The
    /// build fails only when no address of a side mounts.
    pub fn build_spread(
        src_url: &str,
        dst_url: &str,
        src_targets: &[String],
        dst_targets: &[String],
        n: usize,
        rpc_timeout_ms: u32,
    ) -> anyhow::Result<Arc<Self>> {
        Ok(Arc::new(Self::build_inner(
            src_url,
            dst_url,
            src_targets,
            dst_targets,
            n,
            rpc_timeout_ms,
        )?))
    }

    fn build_inner(
        src_url: &str,
        dst_url: &str,
        src_targets: &[String],
        dst_targets: &[String],
        n: usize,
        rpc_timeout_ms: u32,
    ) -> anyhow::Result<Self> {
        if n == 0 {
            anyhow::bail!("MultiPool requires n >= 1, got 0");
        }
        use migration_core::latency::Side;
        let srcs = mount_spread(n, src_targets, |i, target| {
            mount_one(src_url, Side::Src, target, i, rpc_timeout_ms)
        })
        .map_err(|e| anyhow::anyhow!("source: {e}"))?;
        let dsts = mount_spread(n, dst_targets, |i, target| {
            mount_one(dst_url, Side::Dst, target, i, rpc_timeout_ms)
        })
        .map_err(|e| anyhow::anyhow!("destination: {e}"))?;

        let (sender, receiver) = mpsc::unbounded_channel();
        let mut targets = Vec::with_capacity(n);
        for ((src_target, src), (dst_target, dst)) in srcs.into_iter().zip(dsts) {
            targets.push((src_target, dst_target));
            sender
                .send((src, dst))
                .map_err(|_| anyhow::anyhow!("seed send failed (receiver dropped)"))?;
        }
        Ok(Self {
            receiver: Mutex::new(receiver),
            sender,
            capacity: n,
            targets,
        })
    }

    /// The address each pair's source and destination connection uses,
    /// in pair order; `None` means the URL's host name.
    pub fn targets(&self) -> &[(Option<String>, Option<String>)] {
        &self.targets
    }

    /// Number of pairs the pool was built with. Constant for the life
    /// of the pool.
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

#[async_trait]
impl LibnfsContextPool for MultiPool {
    async fn acquire(&self) -> anyhow::Result<ContextPair> {
        let mut rx = self.receiver.lock().await;
        let (src, dst) = rx
            .recv()
            .await
            .ok_or_else(|| anyhow::anyhow!("pool channel closed"))?;
        Ok(ContextPair {
            src: Some(src),
            dst: Some(dst),
            return_to: self.sender.clone(),
        })
    }
}

/// Mount context `i` of one side, at `target` or by the URL's host.
fn mount_one(
    url: &str,
    side: migration_core::latency::Side,
    target: Option<&str>,
    i: usize,
    rpc_timeout_ms: u32,
) -> anyhow::Result<NfsContext> {
    let mut ctx = NfsContext::mount_url_via(url, target, rpc_timeout_ms).map_err(|e| {
        let via = target.map(|t| format!(" via {t}")).unwrap_or_default();
        anyhow::anyhow!("mount {url}{via} (#{i}): {e}")
    })?;
    ctx.set_side(side);
    Ok(ctx)
}

/// Mount `n` contexts for one side of the pool. With no `targets`,
/// every context connects by the URL's host name. Otherwise context `i`
/// starts at `targets[i % len]`; an address whose mount fails is
/// logged, never tried again for this pool, and the context moves on
/// to the next address in turn. Fails only when no address mounts.
///
/// `mount(i, target)` mounts context `i`; generic so the rotation and
/// failover rules are testable without a server.
fn mount_spread<T>(
    n: usize,
    targets: &[String],
    mut mount: impl FnMut(usize, Option<&str>) -> anyhow::Result<T>,
) -> anyhow::Result<Vec<(Option<String>, T)>> {
    let mut out = Vec::with_capacity(n);
    if targets.is_empty() {
        for i in 0..n {
            out.push((None, mount(i, None)?));
        }
        return Ok(out);
    }
    let mut unusable = vec![false; targets.len()];
    for i in 0..n {
        let mut last_err = None;
        let mut mounted = None;
        for offset in 0..targets.len() {
            let t = (i + offset) % targets.len();
            if unusable[t] {
                continue;
            }
            match mount(i, Some(&targets[t])) {
                Ok(ctx) => {
                    mounted = Some((Some(targets[t].clone()), ctx));
                    break;
                }
                Err(e) => {
                    tracing::warn!(
                        address = %targets[t],
                        error = %format!("{e:#}"),
                        "address failed to mount; spreading its connections over the rest",
                    );
                    unusable[t] = true;
                    last_err = Some(e);
                }
            }
        }
        match mounted {
            Some(m) => out.push(m),
            None => {
                return Err(last_err.unwrap_or_else(|| {
                    anyhow::anyhow!("no address of {} mounted", targets.join(", "))
                }))
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn addrs(n: usize) -> Vec<String> {
        (1..=n).map(|i| format!("172.200.204.{i}")).collect()
    }

    fn per_address(out: &[(Option<String>, usize)]) -> BTreeMap<String, usize> {
        let mut m = BTreeMap::new();
        for (t, _) in out {
            *m.entry(t.clone().expect("a target")).or_insert(0) += 1;
        }
        m
    }

    #[test]
    fn connections_spread_evenly_over_every_address() {
        let targets = addrs(6);
        let out = mount_spread(32, &targets, |i, _| Ok(i)).unwrap();
        let counts = per_address(&out);
        assert_eq!(counts.len(), 6, "every address carries connections");
        assert!(counts.values().all(|&c| c == 5 || c == 6), "{counts:?}");
        assert_eq!(counts.values().sum::<usize>(), 32);
        // Pair i starts at address i mod 6.
        assert_eq!(out[0].0.as_deref(), Some("172.200.204.1"));
        assert_eq!(out[7].0.as_deref(), Some("172.200.204.2"));
    }

    #[test]
    fn no_targets_connects_every_context_by_name() {
        let mut seen = Vec::new();
        let out = mount_spread(3, &[], |i, t| {
            seen.push(t.map(str::to_string));
            Ok(i)
        })
        .unwrap();
        assert_eq!(seen, [None, None, None]);
        assert!(out.iter().all(|(t, _)| t.is_none()));
    }

    #[test]
    fn a_failing_address_is_tried_once_and_its_share_moves_on() {
        let targets = addrs(4);
        let mut attempts: BTreeMap<String, usize> = BTreeMap::new();
        let out = mount_spread(8, &targets, |i, t| {
            let t = t.unwrap().to_string();
            *attempts.entry(t.clone()).or_insert(0) += 1;
            if t == "172.200.204.2" {
                anyhow::bail!("ECONNREFUSED")
            }
            Ok(i)
        })
        .unwrap();
        assert_eq!(attempts["172.200.204.2"], 1, "never retried");
        let counts = per_address(&out);
        assert!(!counts.contains_key("172.200.204.2"));
        assert_eq!(counts.values().sum::<usize>(), 8);
        assert_eq!(out.len(), 8);
    }

    #[test]
    fn every_address_failing_is_an_error_naming_the_last_failure() {
        let targets = addrs(3);
        let mut calls = 0;
        let err = mount_spread(4, &targets, |_, t| -> anyhow::Result<usize> {
            calls += 1;
            anyhow::bail!("down: {}", t.unwrap())
        })
        .unwrap_err();
        assert_eq!(calls, 3, "each address tried once");
        assert!(
            format!("{err:#}").contains("down: 172.200.204.3"),
            "{err:#}"
        );
    }

    #[test]
    fn a_mount_error_without_targets_stops_the_build() {
        let err = mount_spread(4, &[], |i, _| -> anyhow::Result<usize> {
            if i == 2 {
                anyhow::bail!("timeout")
            }
            Ok(i)
        })
        .unwrap_err();
        assert!(format!("{err:#}").contains("timeout"));
    }
}
