//! Per-invocation signal state shared by copy, sync, and verification.

use anyhow::{Context, Result};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use tokio::signal::unix::{signal, SignalKind};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    Sigint,
    Sigterm,
}

impl StopReason {
    pub const fn exit_code(self) -> u8 {
        match self {
            Self::Sigint => 130,
            Self::Sigterm => 143,
        }
    }
}

#[derive(Clone)]
pub struct StopState {
    inner: Arc<Inner>,
}

struct Inner {
    reason: AtomicU8,
    token: CancellationToken,
    listener_shutdown: CancellationToken,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.listener_shutdown.cancel();
    }
}

impl StopState {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                reason: AtomicU8::new(0),
                token: CancellationToken::new(),
                listener_shutdown: CancellationToken::new(),
            }),
        }
    }

    pub fn token(&self) -> CancellationToken {
        self.inner.token.clone()
    }

    pub fn reason(&self) -> Option<StopReason> {
        match self.inner.reason.load(Ordering::Acquire) {
            1 => Some(StopReason::Sigint),
            2 => Some(StopReason::Sigterm),
            _ => None,
        }
    }

    pub fn request(&self, reason: StopReason) {
        let value = match reason {
            StopReason::Sigint => 1,
            StopReason::Sigterm => 2,
        };
        if self
            .inner
            .reason
            .compare_exchange(0, value, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.inner.token.cancel();
        }
    }

    /// Install both listeners before starting work. If either handler cannot be
    /// installed, fail the operation instead of proceeding without reliable
    /// signal tracking.
    pub fn install() -> Result<Self> {
        let mut int = signal(SignalKind::interrupt()).context("installing SIGINT handler")?;
        let mut term = signal(SignalKind::terminate()).context("installing SIGTERM handler")?;
        let state = Self::new();
        let weak_state = Arc::downgrade(&state.inner);
        let listener_shutdown = state.inner.listener_shutdown.clone();
        tokio::spawn(async move {
            let first = tokio::select! {
                _ = listener_shutdown.cancelled() => return,
                _ = int.recv() => StopReason::Sigint,
                _ = term.recv() => StopReason::Sigterm,
            };
            tracing::info!(signal = ?first, "stop requested; finishing the batch in flight");
            if let Some(inner) = weak_state.upgrade() {
                StopState { inner }.request(first);
            } else {
                return;
            }
            loop {
                let later = tokio::select! {
                    _ = listener_shutdown.cancelled() => break,
                    _ = int.recv() => StopReason::Sigint,
                    _ = term.recv() => StopReason::Sigterm,
                };
                tracing::warn!(signal = ?later, first_signal = ?first, "already stopping; the first signal reason is retained (use SIGKILL to abandon the batch)");
            }
        });
        Ok(state)
    }
}

impl Default for StopState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{StopReason, StopState};

    #[test]
    fn first_signal_is_stable_and_cancels() {
        let state = StopState::new();
        assert!(!state.token().is_cancelled());
        state.request(StopReason::Sigterm);
        state.request(StopReason::Sigint);
        assert!(state.token().is_cancelled());
        assert_eq!(state.reason(), Some(StopReason::Sigterm));
    }

    #[test]
    fn separate_operations_have_separate_stop_state() {
        let first = StopState::new();
        let second = StopState::new();
        first.request(StopReason::Sigint);
        assert_eq!(first.reason(), Some(StopReason::Sigint));
        assert_eq!(second.reason(), None);
        assert!(!second.token().is_cancelled());
    }
}
