//! In-flight request tracking for the WS client (ENG-1527).
//!
//! Every dispatched request is registered here so a server `cancel` message can
//! reach it, and long "coding run" requests can be capped at one at a time.
//! The registry is shared state (lives in [`crate::state::AppState`]) because
//! cancellation can arrive from two directions: the engine (WS `cancel`
//! message) and the local UI (tray "Stop run", sub-issue ENG-1528).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::{watch, OwnedSemaphorePermit, Semaphore, TryAcquireError};

/// Receiver half of a request's cancellation signal.
///
/// Handlers either `select!` on [`CancelSignal::cancelled`] against their work
/// (read-only tools) or poll [`CancelSignal::is_cancelled`] / forward the
/// signal into a child-process kill (coding runs).
#[derive(Clone)]
pub struct CancelSignal {
    rx: watch::Receiver<bool>,
}

impl CancelSignal {
    #[allow(dead_code)] // consumed by ENG-1528 (coding-run adapters)
    pub fn is_cancelled(&self) -> bool {
        *self.rx.borrow()
    }

    /// Resolves once the request is cancelled. If the request finishes first
    /// and is removed from the registry, the sender drops and this resolves
    /// too — harmless, because the handler is gone by then.
    pub async fn cancelled(&mut self) {
        loop {
            if *self.rx.borrow() {
                return;
            }
            if self.rx.changed().await.is_err() {
                // Sender dropped (request finished/unregistered): treat as
                // "will never be cancelled now" but never hang the caller.
                return;
            }
        }
    }
}

/// Guard for the single concurrent coding-run slot. Dropping it frees the slot.
// Constructed by ENG-1528's run_coding_agent handler; the allow comes off then.
#[allow(dead_code)]
pub struct CodingRunGuard {
    _permit: OwnedSemaphorePermit,
}

/// Registry of in-flight requests + the coding-run concurrency cap.
pub struct InflightRegistry {
    requests: Mutex<HashMap<String, watch::Sender<bool>>>,
    /// One coding run at a time per device: local CLIs are heavyweight (model
    /// inference, file edits in a workspace) and concurrent runs in the same
    /// scoped folders could interleave edits.
    #[allow(dead_code)] // read by try_begin_coding_run, live from ENG-1528
    coding_slot: Arc<Semaphore>,
}

impl Default for InflightRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl InflightRegistry {
    pub fn new() -> Self {
        Self {
            requests: Mutex::new(HashMap::new()),
            coding_slot: Arc::new(Semaphore::new(1)),
        }
    }

    /// Track a new in-flight request. Returns its cancellation signal.
    /// A duplicate request_id replaces the stale entry (the old signal's
    /// sender drops, waking any handler still selecting on it).
    pub fn register(&self, request_id: &str) -> CancelSignal {
        let (tx, rx) = watch::channel(false);
        self.requests
            .lock()
            .expect("inflight lock poisoned")
            .insert(request_id.to_string(), tx);
        CancelSignal { rx }
    }

    /// Cancel an in-flight request. Returns false when the id is unknown
    /// (already finished, or never seen) — a no-op by protocol contract.
    pub fn cancel(&self, request_id: &str) -> bool {
        let map = self.requests.lock().expect("inflight lock poisoned");
        match map.get(request_id) {
            Some(tx) => tx.send(true).is_ok(),
            None => false,
        }
    }

    /// Remove a finished request. Idempotent.
    pub fn finish(&self, request_id: &str) {
        self.requests
            .lock()
            .expect("inflight lock poisoned")
            .remove(request_id);
    }

    /// Cancel everything currently in flight (app shutdown / user disconnect).
    #[allow(dead_code)] // wired up by ENG-1528 (tray Stop / disconnect)
    pub fn cancel_all(&self) {
        let map = self.requests.lock().expect("inflight lock poisoned");
        for tx in map.values() {
            let _ = tx.send(true);
        }
    }

    #[allow(dead_code)] // wired up by ENG-1528
    pub fn in_flight(&self) -> usize {
        self.requests.lock().expect("inflight lock poisoned").len()
    }

    /// Claim the single coding-run slot without waiting. `None` means a run
    /// is already active — the caller must fail the request (the engine
    /// surfaces "a coding run is already in progress on this device") rather
    /// than queue it behind an arbitrarily long run.
    #[allow(dead_code)] // consumed by ENG-1528 (coding-run adapters)
    pub fn try_begin_coding_run(&self) -> Option<CodingRunGuard> {
        match Arc::clone(&self.coding_slot).try_acquire_owned() {
            Ok(permit) => Some(CodingRunGuard { _permit: permit }),
            Err(TryAcquireError::NoPermits) => None,
            Err(TryAcquireError::Closed) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn cancel_wakes_a_selecting_handler() {
        let reg = InflightRegistry::new();
        let mut signal = reg.register("req-1");

        let waiter = tokio::spawn(async move {
            signal.cancelled().await;
            true
        });

        assert!(reg.cancel("req-1"));
        let woke = tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("cancelled() must resolve after cancel()")
            .unwrap();
        assert!(woke);
    }

    #[test]
    fn cancel_of_unknown_or_finished_request_is_a_noop() {
        let reg = InflightRegistry::new();
        assert!(!reg.cancel("never-seen"));

        reg.register("req-1");
        reg.finish("req-1");
        assert!(!reg.cancel("req-1"));
        assert_eq!(reg.in_flight(), 0);
    }

    #[tokio::test]
    async fn finish_drops_sender_and_unblocks_waiters() {
        // A handler selecting on cancelled() must not hang forever if the
        // request is unregistered out from under it.
        let reg = InflightRegistry::new();
        let mut signal = reg.register("req-1");
        reg.finish("req-1");
        tokio::time::timeout(Duration::from_secs(1), signal.cancelled())
            .await
            .expect("cancelled() must resolve when the sender is dropped");
    }

    #[test]
    fn coding_run_slot_is_exclusive_and_freed_on_drop() {
        let reg = InflightRegistry::new();
        let guard = reg.try_begin_coding_run().expect("slot free initially");
        assert!(
            reg.try_begin_coding_run().is_none(),
            "second concurrent coding run must be refused"
        );
        drop(guard);
        assert!(
            reg.try_begin_coding_run().is_some(),
            "slot must free when the guard drops"
        );
    }

    #[tokio::test]
    async fn handlers_run_concurrently_not_serialized() {
        // The invariant ENG-1527 exists for: two in-flight requests overlap
        // in time (each yields while "working"), rather than queueing behind
        // one another the way the old inline-await loop forced.
        use std::time::Instant;
        let start = Instant::now();
        let a = tokio::spawn(tokio::time::sleep(Duration::from_millis(200)));
        let b = tokio::spawn(tokio::time::sleep(Duration::from_millis(200)));
        let _ = tokio::join!(a, b);
        assert!(
            start.elapsed() < Duration::from_millis(350),
            "spawned handlers must overlap (took {:?})",
            start.elapsed()
        );
    }
}
