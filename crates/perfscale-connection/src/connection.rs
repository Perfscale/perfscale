//! The [`Connection`] trait — what a parked handle must provide.

/// A live connection handle that can be parked in a
/// [`ConnectionRegistry`](crate::ConnectionRegistry).
///
/// Implement this on the concrete per-family handle type (an open
/// WebSocket, a gRPC channel, a DB pool, …). The registry only ever stores,
/// moves, and drops handles; the two items below exist for the family code
/// around it:
///
/// * [`label`](Connection::label) feeds diagnostics and log lines, and
/// * [`close`](Connection::close) gives the family a graceful-shutdown hook
///   for its explicit `*-close` step.
///
/// The `Send` bound mirrors how engines use the registry: handles cross
/// `.await` points inside a virtual-user task, so they must be `Send` just
/// as they were when each family kept its own hand-rolled map.
///
/// # Implementing
///
/// ```
/// use perfscale_connection::Connection;
///
/// struct WsHandle {
///     url: String,      // kept for log lines
///     // stream: …,     // the real socket state
/// }
///
/// impl Connection for WsHandle {
///     fn label(&self) -> &str {
///         &self.url
///     }
/// }
/// ```
pub trait Connection: Send {
    /// Short human-readable label for diagnostics and log lines — a driver
    /// name (`"postgres"`), a target URL, or a sanitized `host:port/database`
    /// pair.
    ///
    /// Never include credentials: this string may end up in run logs.
    fn label(&self) -> &str;

    /// Gracefully close the connection, consuming the handle.
    ///
    /// The default implementation simply drops the handle, which is the
    /// right behavior for connections whose teardown is a close-on-drop
    /// socket. Override it when the protocol has a real goodbye (a WebSocket
    /// Close handshake, a transaction rollback hook, …) and call it from the
    /// family's explicit `*-close` step:
    ///
    /// ```ignore
    /// let conn = registry.take(&id).ok_or("unknown connection id")?;
    /// conn.close(); // graceful; nothing is put back
    /// ```
    ///
    /// Note that [`ConnectionRegistry::drain`](crate::ConnectionRegistry::drain)
    /// does **not** call this — end-of-iteration teardown is deliberately an
    /// abrupt drop, so a parked handle never outlives its iteration even
    /// when a scenario forgot to close it.
    fn close(self)
    where
        Self: Sized,
    {
        drop(self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    /// A handle with an observable drop, so tests can tell plain teardown
    /// (what the default `close` does) apart from an overridden hook.
    struct DropOnly {
        label: String,
        dropped: Arc<AtomicBool>,
    }

    impl DropOnly {
        fn new(label: &str) -> (Self, Arc<AtomicBool>) {
            let dropped = Arc::new(AtomicBool::new(false));
            (
                Self {
                    label: label.into(),
                    dropped: Arc::clone(&dropped),
                },
                dropped,
            )
        }
    }

    impl Drop for DropOnly {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    impl Connection for DropOnly {
        fn label(&self) -> &str {
            &self.label
        }
        // `close` deliberately left at its default: plain drop.
    }

    #[test]
    fn default_close_just_drops_the_handle() {
        let (conn, dropped) = DropOnly::new("wss://example.com");
        conn.close();
        assert!(dropped.load(Ordering::SeqCst), "default close() drops");
    }

    #[test]
    fn overridden_close_runs_the_graceful_hook() {
        struct Graceful {
            closed: Arc<AtomicBool>,
        }

        impl Connection for Graceful {
            fn label(&self) -> &str {
                "graceful"
            }

            fn close(self) {
                self.closed.store(true, Ordering::SeqCst);
            }
        }

        let closed = Arc::new(AtomicBool::new(false));
        let conn = Graceful {
            closed: Arc::clone(&closed),
        };
        conn.close();
        assert!(closed.load(Ordering::SeqCst), "override ran");
    }

    #[test]
    fn label_is_readable_through_a_trait_object() {
        let (conn, _dropped) = DropOnly::new("postgres://host/db");
        let boxed: Box<dyn Connection> = Box::new(conn);
        assert_eq!(boxed.label(), "postgres://host/db");
    }

    /// Handles cross `.await` points inside virtual-user tasks; the trait's
    /// `Send` bound is what makes that sound, so pin it at compile time.
    #[test]
    fn connection_handles_are_send() {
        fn assert_send<T: Connection>() {}
        assert_send::<DropOnly>();
    }
}
