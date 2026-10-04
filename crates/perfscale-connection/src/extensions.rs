//! The [`ExtensionRegistries`] — a type-erased parking lot for connection
//! families this crate does not know about.
//!
//! This is **the** extension point for downstream pro action families
//! (`pro/webrtc-*`, future `pro/*`): a closed crate cannot add a typed field
//! to the engine's `Resources`, so it parks its live connections here
//! instead, keyed by the handle's [`TypeId`]. Each family gets-or-inserts its
//! own typed [`ConnectionRegistry`] with its own id prefix (`"rtc"` →
//! `"rtc-1"`, `"rtc-2"`, …) and from then on uses the exact same
//! insert/take/put_back semantics as the built-in families. The engine's
//! iteration-end drain drains every extension registry, so a parked pro
//! connection never outlives its VU iteration either.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::{Connection, ConnectionRegistry};

/// Type-erased view of a stored [`ConnectionRegistry`]: enough for the
/// engine to drain it at iteration end and report live counts, without
/// knowing the handle type.
trait ErasedRegistry: Send {
    fn drain(&self) -> usize;
    fn len(&self) -> usize;
    fn as_any(&self) -> &dyn Any;
}

impl<C: Connection + 'static> ErasedRegistry for ConnectionRegistry<C> {
    fn drain(&self) -> usize {
        ConnectionRegistry::drain(self)
    }

    fn len(&self) -> usize {
        ConnectionRegistry::len(self)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

struct Inner {
    registries: HashMap<TypeId, Box<dyn ErasedRegistry>>,
}

/// Type-erased registries for downstream connection families, keyed by the
/// family's handle [`TypeId`].
///
/// # Usage (downstream pro crate)
///
/// ```
/// use perfscale_connection::{Connection, ExtensionRegistries};
///
/// struct RtcPeer { /* webrtc-rs peer connection state */ }
/// impl Connection for RtcPeer {
///     fn label(&self) -> &str { "rtc" }
/// }
///
/// let extras = ExtensionRegistries::new();
/// let registry = extras.registry::<RtcPeer>("rtc");
/// let id = registry.insert(RtcPeer {});
/// assert_eq!(id, "rtc-1");
/// ```
///
/// `registry::<C>(prefix)` is a get-or-insert: the first call creates the
/// family's [`ConnectionRegistry`] with the given id prefix, later calls
/// return a handle to the same registry (clones share one pool, exactly like
/// cloning a [`ConnectionRegistry`] itself). One handle type = one family:
/// passing the same `C` with a different prefix returns the original
/// registry unchanged — pick one prefix per family and stick to it.
///
/// # Cloning
///
/// Like [`ConnectionRegistry`], cloning produces an alias of the same map,
/// so a per-VU execution context and its clones all see one set of
/// extension registries.
pub struct ExtensionRegistries {
    inner: Arc<Mutex<Inner>>,
}

impl ExtensionRegistries {
    /// Create an empty set of extension registries.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                registries: HashMap::new(),
            })),
        }
    }

    /// Get the family's registry, creating it with id prefix `prefix` on
    /// first use. The returned handle shares the stored pool; it stays valid
    /// for the family's whole lifetime.
    pub fn registry<C: Connection + 'static>(&self, prefix: &str) -> ConnectionRegistry<C> {
        let mut inner = self.inner.lock().unwrap();
        let entry = inner
            .registries
            .entry(TypeId::of::<C>())
            .or_insert_with(|| Box::new(ConnectionRegistry::<C>::new(prefix)));
        entry
            .as_any()
            .downcast_ref::<ConnectionRegistry<C>>()
            .expect("TypeId keys the map, so the stored type always matches")
            .clone()
    }

    /// Drop every connection parked in every extension family and return the
    /// total dropped, so the caller can decide whether the teardown is worth
    /// a log line. Same abrupt-drop semantics as
    /// [`ConnectionRegistry::drain`]; id counters are not reset. The
    /// registries themselves survive — a family registered before the drain
    /// keeps its registry and prefix.
    pub fn drain(&self) -> usize {
        let inner = self.inner.lock().unwrap();
        inner.registries.values().map(|r| r.drain()).sum()
    }

    /// Total connections parked across all extension families.
    pub fn len(&self) -> usize {
        let inner = self.inner.lock().unwrap();
        inner.registries.values().map(|r| r.len()).sum()
    }

    /// Whether no extension family has anything parked.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Number of distinct families registered so far.
    pub fn families(&self) -> usize {
        self.inner.lock().unwrap().registries.len()
    }
}

impl Default for ExtensionRegistries {
    fn default() -> Self {
        Self::new()
    }
}

// Manual `Clone`: alias the same map, like `ConnectionRegistry::clone`.
impl Clone for ExtensionRegistries {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl std::fmt::Debug for ExtensionRegistries {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "ExtensionRegistries({} families, {} live)",
            self.families(),
            self.len()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Handle with an observable drop, so drain tests can see the teardown.
    struct RtcConn {
        dropped: Arc<AtomicBool>,
    }

    impl RtcConn {
        fn new() -> (Self, Arc<AtomicBool>) {
            let dropped = Arc::new(AtomicBool::new(false));
            (
                Self {
                    dropped: Arc::clone(&dropped),
                },
                dropped,
            )
        }
    }

    impl Drop for RtcConn {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    impl Connection for RtcConn {
        fn label(&self) -> &str {
            "rtc"
        }
    }

    /// A second, unrelated family — proves there is no cross-talk.
    struct FixConn;

    impl Connection for FixConn {
        fn label(&self) -> &str {
            "fix"
        }
    }

    #[test]
    fn registry_mints_ids_with_the_family_prefix() {
        let extras = ExtensionRegistries::new();
        let registry = extras.registry::<RtcConn>("rtc");
        let (a, _) = RtcConn::new();
        let (b, _) = RtcConn::new();
        assert_eq!(registry.insert(a), "rtc-1");
        assert_eq!(registry.insert(b), "rtc-2");
        assert_eq!(extras.len(), 2);
        assert_eq!(extras.families(), 1);
    }

    #[test]
    fn get_or_insert_returns_the_same_pool() {
        let extras = ExtensionRegistries::new();
        let first = extras.registry::<RtcConn>("rtc");
        let (conn, _) = RtcConn::new();
        let id = first.insert(conn);

        let again = extras.registry::<RtcConn>("rtc");
        assert_eq!(again.prefix(), "rtc");
        assert!(again.take(&id).is_some(), "second handle sees the pool");
        assert_eq!(extras.families(), 1, "no second registry was created");
    }

    #[test]
    fn drain_drops_every_family_and_keeps_the_registries() {
        let extras = ExtensionRegistries::new();
        let (conn, dropped) = RtcConn::new();
        extras.registry::<RtcConn>("rtc").insert(conn);
        extras.registry::<FixConn>("fix").insert(FixConn);

        assert_eq!(extras.drain(), 2);
        assert_eq!(extras.drain(), 0, "second drain finds nothing");
        assert!(dropped.load(Ordering::SeqCst), "handle was dropped");
        assert!(extras.is_empty());

        // The registry itself survives: the family keeps its prefix and its
        // id counter keeps counting (stale ids never resolve again).
        let (conn, _) = RtcConn::new();
        assert_eq!(extras.registry::<RtcConn>("rtc").insert(conn), "rtc-2");
        assert_eq!(extras.families(), 2);
    }

    #[test]
    fn two_types_do_not_cross_talk() {
        let extras = ExtensionRegistries::new();
        let rtc = extras.registry::<RtcConn>("rtc");
        let fix = extras.registry::<FixConn>("fix");

        let (conn, _) = RtcConn::new();
        let rtc_id = rtc.insert(conn);
        let fix_id = fix.insert(FixConn);
        assert_eq!(rtc_id, "rtc-1");
        assert_eq!(fix_id, "fix-1");
        assert_eq!(extras.families(), 2);

        // A handle type never sees another family's entries, even under an
        // id the other family minted.
        assert!(fix.take(&rtc_id).is_none());
        assert!(rtc.take(&fix_id).is_none());
        assert!(rtc.take(&rtc_id).is_some());
        assert!(fix.take(&fix_id).is_some());
    }

    #[test]
    fn clones_alias_the_same_map() {
        let extras = ExtensionRegistries::new();
        let alias = extras.clone();

        let (conn, _) = RtcConn::new();
        let id = extras.registry::<RtcConn>("rtc").insert(conn);
        assert_eq!(alias.len(), 1);
        assert!(
            alias.registry::<RtcConn>("rtc").take(&id).is_some(),
            "clone sees the same pool"
        );
    }

    #[test]
    fn debug_reports_families_and_live_count() {
        let extras = ExtensionRegistries::new();
        let (conn, _) = RtcConn::new();
        extras.registry::<RtcConn>("rtc").insert(conn);
        extras.registry::<FixConn>("fix").insert(FixConn);
        assert_eq!(
            format!("{extras:?}"),
            "ExtensionRegistries(2 families, 2 live)"
        );
    }
}
