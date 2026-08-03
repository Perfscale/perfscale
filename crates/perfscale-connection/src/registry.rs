//! The [`ConnectionRegistry`] — parking lot for one family's live handles.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::Connection;

struct Inner<C> {
    /// Ids minted so far. Monotonic and deliberately **not** reset by
    /// [`drain`](ConnectionRegistry::drain): an id is unique per registry
    /// lifetime, so a stale id from an earlier iteration can never
    /// accidentally resolve to a fresh connection.
    next_id: u64,
    conns: HashMap<String, C>,
}

// Manual `Default`: `next_id: 0` + empty map needs no `C: Default` bound.
impl<C> Default for Inner<C> {
    fn default() -> Self {
        Self {
            next_id: 0,
            conns: HashMap::new(),
        }
    }
}

/// A registry of named, live connections for one protocol family.
///
/// Handles are parked under minted **Connection IDs** of the form
/// `{prefix}-{n}` (`"ws-1"`, `"grpc-2"`, `"db-1"`, …), where `n` counts up
/// from 1 per registry. The prefix is fixed at construction; one registry
/// serves exactly one family, which keeps every family's id format stable
/// and makes [`drain`](Self::drain) a drain-by-prefix operation.
///
/// # Concurrency model
///
/// The registry is built for engines whose steps run **strictly
/// sequentially** within one virtual user: a step removes its handle
/// ([`take`](Self::take)), uses it without holding any lock — possibly
/// across many `.await` points — and returns it ([`put_back`](Self::put_back)).
/// A scenario that somehow references the same id twice concurrently simply
/// gets `None` from the second `take` and can report a clean "unknown id"
/// error instead of deadlocking.
///
/// # Cloning
///
/// Cloning a registry produces an alias of the *same* pool (shared
/// `Arc<Mutex<…>>`), the same way a per-VU execution context and its clones
/// must all see one set of live connections.
pub struct ConnectionRegistry<C: Connection> {
    /// Id prefix, kept outside the lock so diagnostics never contend.
    prefix: String,
    inner: Arc<Mutex<Inner<C>>>,
}

impl<C: Connection> ConnectionRegistry<C> {
    /// Create an empty registry that mints ids as `{prefix}-{n}`.
    ///
    /// The prefix should be the family's short, user-facing name (`"ws"`,
    /// `"grpc"`, `"db"`) — it appears verbatim in the ids users see in step
    /// outputs and log lines, so changing it is a user-visible change.
    pub fn new(prefix: impl Into<String>) -> Self {
        Self {
            prefix: prefix.into(),
            inner: Arc::new(Mutex::new(Inner::default())),
        }
    }

    /// The id prefix this registry was built with.
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    /// Park a connection and mint its Connection ID (`"{prefix}-{n}"`,
    /// starting at 1).
    ///
    /// The counter only ever increases — ids are never reused, even after
    /// [`drain`](Self::drain) — so a stale id can never resolve to an
    /// unrelated connection.
    pub fn insert(&self, conn: C) -> String {
        let mut inner = self.inner.lock().unwrap();
        inner.next_id += 1;
        let id = format!("{}-{}", self.prefix, inner.next_id);
        inner.conns.insert(id.clone(), conn);
        id
    }

    /// Remove a connection for exclusive use by one step. Returns `None`
    /// for an unknown id (never connected, already closed, or currently
    /// taken by another step).
    ///
    /// The caller must either return the handle via
    /// [`put_back`](Self::put_back) or consume it (close the connection);
    /// otherwise the handle leaks out of the registry and the id stops
    /// resolving.
    pub fn take(&self, id: &str) -> Option<C> {
        self.inner.lock().unwrap().conns.remove(id)
    }

    /// Return a connection taken with [`take`](Self::take), parking it under
    /// the same id again. If the id is somehow occupied, the existing entry
    /// is replaced (and dropped) — `put_back` never fails, so a step's
    /// error paths can call it unconditionally.
    pub fn put_back(&self, id: &str, conn: C) {
        self.inner
            .lock()
            .unwrap()
            .conns
            .insert(id.to_string(), conn);
    }

    /// Drop every parked connection and return how many were dropped, so
    /// the caller can decide whether the teardown is worth a log line.
    ///
    /// Dropping is deliberately abrupt (plain `Drop`, not
    /// [`Connection::close`]): this is the end-of-iteration safety net for
    /// connections a scenario left open, and a parked handle must never
    /// outlive its iteration. The id counter is **not** reset.
    ///
    /// Because one registry holds exactly one family's prefix, draining this
    /// registry is the drain-by-prefix operation: other families' registries
    /// (and any non-connection caches the family keeps beside its registry)
    /// are untouched.
    pub fn drain(&self) -> usize {
        let mut inner = self.inner.lock().unwrap();
        let n = inner.conns.len();
        inner.conns.clear();
        n
    }

    /// Number of connections currently parked.
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().conns.len()
    }

    /// Whether no connections are currently parked.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

// Manual `Clone`: the shared pool means no `C: Clone` bound is needed.
impl<C: Connection> Clone for ConnectionRegistry<C> {
    fn clone(&self) -> Self {
        Self {
            prefix: self.prefix.clone(),
            inner: Arc::clone(&self.inner),
        }
    }
}

// Manual `Debug`: report the prefix and live count without requiring
// `C: Debug` (live handles rarely are).
impl<C: Connection> std::fmt::Debug for ConnectionRegistry<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "ConnectionRegistry({}: {} live)",
            self.prefix,
            self.len()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// A parked handle with a label and an observable `close()`, so tests
    /// can see both trait items at work without a real socket.
    struct TestConn {
        label: String,
        closed: Arc<AtomicBool>,
    }

    impl TestConn {
        fn new(label: &str) -> Self {
            Self {
                label: label.into(),
                closed: Arc::new(AtomicBool::new(false)),
            }
        }
    }

    impl Connection for TestConn {
        fn label(&self) -> &str {
            &self.label
        }

        fn close(self) {
            self.closed.store(true, Ordering::SeqCst);
        }
    }

    fn registry() -> ConnectionRegistry<TestConn> {
        ConnectionRegistry::new("ws")
    }

    #[test]
    fn ids_mint_with_prefix_and_count_up() {
        let reg = registry();
        assert_eq!(reg.insert(TestConn::new("a")), "ws-1");
        assert_eq!(reg.insert(TestConn::new("b")), "ws-2");
        assert_eq!(reg.len(), 2);
        assert!(!reg.is_empty());
    }

    #[test]
    fn prefixes_are_independent_counters() {
        let ws = ConnectionRegistry::<TestConn>::new("ws");
        let grpc = ConnectionRegistry::<TestConn>::new("grpc");
        let db = ConnectionRegistry::<TestConn>::new("db");
        assert_eq!(ws.insert(TestConn::new("a")), "ws-1");
        assert_eq!(grpc.insert(TestConn::new("a")), "grpc-1");
        assert_eq!(db.insert(TestConn::new("a")), "db-1");
        assert_eq!(grpc.insert(TestConn::new("b")), "grpc-2");
    }

    #[test]
    fn take_removes_and_put_back_restores() {
        let reg = registry();
        let id = reg.insert(TestConn::new("wss://example.com"));

        let conn = reg.take(&id).expect("parked");
        assert_eq!(conn.label(), "wss://example.com");
        assert!(reg.take(&id).is_none(), "take removes");
        assert!(reg.is_empty());

        reg.put_back(&id, conn);
        assert_eq!(reg.len(), 1);
        assert!(reg.take(&id).is_some(), "put_back restores");
    }

    #[test]
    fn unknown_ids_resolve_to_none() {
        let reg = registry();
        assert!(reg.take("ws-99").is_none());
        assert!(reg.take("grpc-1").is_none(), "wrong family prefix");
    }

    #[test]
    fn put_back_over_an_occupied_id_replaces() {
        let reg = registry();
        let id = reg.insert(TestConn::new("first"));
        // Pathological but defined: never fail, never leak the slot.
        reg.put_back(&id, TestConn::new("second"));
        assert_eq!(reg.len(), 1);
        assert_eq!(reg.take(&id).unwrap().label(), "second");
    }

    #[test]
    fn drain_drops_everything_and_reports_the_count() {
        let reg = registry();
        reg.insert(TestConn::new("a"));
        reg.insert(TestConn::new("b"));

        assert_eq!(reg.drain(), 2);
        assert_eq!(reg.drain(), 0, "second drain finds nothing");
        assert!(reg.take("ws-1").is_none());
        assert!(reg.is_empty());
    }

    #[test]
    fn drain_does_not_reset_the_id_counter() {
        let reg = registry();
        assert_eq!(reg.insert(TestConn::new("a")), "ws-1");
        assert_eq!(reg.drain(), 1);
        // A stale id from before the drain must never resolve again.
        assert_eq!(reg.insert(TestConn::new("b")), "ws-2");
    }

    #[test]
    fn clones_share_one_pool() {
        let reg = registry();
        let alias = reg.clone();

        let id = reg.insert(TestConn::new("a"));
        assert_eq!(alias.len(), 1);
        assert!(alias.take(&id).is_some(), "clone sees the same pool");
        assert!(reg.is_empty());
    }

    #[test]
    fn explicit_close_runs_the_graceful_hook() {
        let reg = registry();
        let id = reg.insert(TestConn::new("a"));

        let conn = reg.take(&id).expect("parked");
        let flag = Arc::clone(&conn.closed);
        conn.close();
        assert!(flag.load(Ordering::SeqCst), "close() hook ran");
        assert!(reg.take(&id).is_none(), "closed ids stay gone");
    }

    #[test]
    fn debug_shows_prefix_and_live_count() {
        let reg = registry();
        reg.insert(TestConn::new("a"));
        assert_eq!(format!("{reg:?}"), "ConnectionRegistry(ws: 1 live)");
    }
}
