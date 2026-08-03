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
