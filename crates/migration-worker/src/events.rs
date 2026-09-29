//! In-process progress events from the shard processor.
//!
//! [`EventEmitter`] is the sender the shard processor holds; with the
//! `distributed` feature, `coord_driver` drains the channel and forwards
//! coalesced events to the coordinator. It lives outside `coord_driver`
//! so builds without the coordinator HTTP client (mongoose) can still
//! construct [`EventEmitter::disabled`].

/// In-process event the shard processor (and any future caller)
/// hands to the coord_driver for forwarding to the coord. The
/// processor does NOT carry `worker_id` — it isn't known at
/// processor-construction time (register completes asynchronously).
/// The driver materializes the full
/// [`EventKind`](migration_control_protocol::schema::EventKind) envelope by
/// attaching `worker_id` and `job_id` at POST time.
///
/// v1 only emits per-file progress counts (Ok / Failed / Fenced).
/// Richer detail (`ErrorEmitted` with class + path) can be added in
/// a follow-on step without changing the channel shape — just add a
/// new variant.
#[derive(Debug, Clone)]
pub enum WorkerEventDraft {
    /// One file completed successfully — fold into the next
    /// coalesced `ProgressDelta`.
    ProgressOk { bytes: u64 },
    /// One file failed (per-file failure, not a fence trip). Folded
    /// into `errors_delta` on the next ProgressDelta. The full
    /// `ErrorEmitted` event (class, path) is a future enhancement.
    ProgressFailed,
    /// One file bailed out at the mover's R8 fence check. Surfaced
    /// to coord as `errors_delta` for visibility but not flagged as
    /// a worker failure — the next reclaimer copies the row.
    ProgressFenced,
}

/// Sender-side handle wrapping `Option<Sender<WorkerEventDraft>>`.
/// Construct with [`EventEmitter::disabled`] for legacy / no-coord
/// mode — every call becomes a no-op. Construct via
/// [`EventEmitter::from_sender`] when the orchestrator has wired the
/// channel.
///
/// All methods use `try_send` so the caller (shard processor) is
/// never blocked. On full channel the draft is dropped and a counter
/// could be bumped (deferred to a future revision — for now the
/// coord just sees lower progress numbers).
#[derive(Debug, Clone)]
pub struct EventEmitter {
    tx: Option<tokio::sync::mpsc::Sender<WorkerEventDraft>>,
}

impl EventEmitter {
    pub fn disabled() -> Self {
        Self { tx: None }
    }

    pub fn from_sender(tx: tokio::sync::mpsc::Sender<WorkerEventDraft>) -> Self {
        Self { tx: Some(tx) }
    }

    pub fn is_enabled(&self) -> bool {
        self.tx.is_some()
    }

    pub fn progress_ok(&self, bytes: u64) {
        if let Some(tx) = &self.tx {
            let _ = tx.try_send(WorkerEventDraft::ProgressOk { bytes });
        }
    }

    pub fn progress_failed(&self) {
        if let Some(tx) = &self.tx {
            let _ = tx.try_send(WorkerEventDraft::ProgressFailed);
        }
    }

    pub fn progress_fenced(&self) {
        if let Some(tx) = &self.tx {
            let _ = tx.try_send(WorkerEventDraft::ProgressFenced);
        }
    }
}

impl Default for EventEmitter {
    fn default() -> Self {
        Self::disabled()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // EventEmitter — disabled / enabled / drop behavior
    // =========================================================================

    #[test]
    fn disabled_emitter_silently_swallows_drafts() {
        let e = EventEmitter::disabled();
        assert!(!e.is_enabled());
        // None of these should panic or block.
        e.progress_ok(1024);
        e.progress_failed();
        e.progress_fenced();
    }

    #[tokio::test]
    async fn enabled_emitter_pushes_drafts_to_channel() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let e = EventEmitter::from_sender(tx);
        assert!(e.is_enabled());
        e.progress_ok(1024);
        e.progress_ok(2048);
        e.progress_failed();
        e.progress_fenced();

        let mut got = Vec::new();
        while let Ok(d) = rx.try_recv() {
            got.push(d);
        }
        assert_eq!(got.len(), 4);
        assert!(matches!(
            got[0],
            WorkerEventDraft::ProgressOk { bytes: 1024 }
        ));
        assert!(matches!(
            got[1],
            WorkerEventDraft::ProgressOk { bytes: 2048 }
        ));
        assert!(matches!(got[2], WorkerEventDraft::ProgressFailed));
        assert!(matches!(got[3], WorkerEventDraft::ProgressFenced));
    }

    #[tokio::test]
    async fn full_channel_drops_drafts_without_blocking() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(2);
        let e = EventEmitter::from_sender(tx);
        // Fill the channel.
        e.progress_ok(1);
        e.progress_ok(2);
        // These should silently drop.
        e.progress_ok(3);
        e.progress_ok(4);

        let mut got = 0;
        while rx.try_recv().is_ok() {
            got += 1;
        }
        assert_eq!(got, 2, "exactly the channel capacity should land");
    }
}
