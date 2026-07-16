//! Live-connection registry — stateful resources that span steps.
//!
//! `Context.vars` holds only JSON, so an open WebSocket cannot live there.
//! Instead, `std/ws-connect@v1` parks the socket here and returns a JSON-safe
//! **Connection ID** (`"ws-1"`, `"ws-2"`, …); later steps (`ws-send`,
//! `ws-recv`, `ws-ping`, `ws-close`) look the socket up by that id.
//!
//! Scope: one VU iteration. The runner drains the registry after every
//! iteration, dropping whatever a scenario left open — an abrupt TCP drop, no
//! Close handshake (use `std/ws-close@v1` for a graceful shutdown). A Live
//! Connection therefore never outlives its iteration, and ids never leak
//! across iterations or VUs.
//!
//! Steps within a VU run strictly sequentially, so the registry hands a
//! connection out by *removing* it (`take`) and expects it back (`put_back`)
//! — no lock is ever held across an `.await`, and a scenario that somehow
//! references one id twice concurrently gets a clean "unknown id" error
//! instead of a deadlock.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::generate::Gen;

/// A connected WebSocket, plain or TLS.
pub(crate) type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// One live WebSocket plus the per-connection state that must survive between
/// steps.
pub(crate) struct WsConn {
    pub stream: WsStream,
    /// Target URL, kept for log lines.
    pub url: String,
    /// `${…}` generator — `${seq}` keeps counting across send steps on the
    /// same connection.
    pub generator: Gen,
    /// When the last `ws-send` finished writing, so a following `ws-recv`
    /// with an until-condition can report the send→match Message RTT.
    pub last_send: Option<Instant>,
    /// Data messages consumed incidentally by `ws-ping` while it waited for
    /// the pong. A later `ws-recv` drains these first, so nothing is lost.
    pub pending: std::collections::VecDeque<serde_json::Value>,
}

#[derive(Default)]
struct Inner {
    next_id: u64,
    conns: HashMap<String, WsConn>,
}

/// Shared handle to a VU's live connections. Cloning shares the same
/// registry (the `Context` derives `Clone`; both copies must see one pool).
#[derive(Clone, Default)]
pub(crate) struct Resources {
    inner: Arc<Mutex<Inner>>,
}

impl Resources {
    /// Park a connection and mint its Connection ID.
    pub(crate) fn insert(&self, conn: WsConn) -> String {
        let mut inner = self.inner.lock().unwrap();
        inner.next_id += 1;
        let id = format!("ws-{}", inner.next_id);
        inner.conns.insert(id.clone(), conn);
        id
    }

    /// Remove a connection for exclusive use by one step. Must be returned
    /// via [`put_back`](Self::put_back) unless the step closes it.
    pub(crate) fn take(&self, id: &str) -> Option<WsConn> {
        self.inner.lock().unwrap().conns.remove(id)
    }

    /// Return a connection taken with [`take`](Self::take).
    pub(crate) fn put_back(&self, id: &str, conn: WsConn) {
        self.inner
            .lock()
            .unwrap()
            .conns
            .insert(id.to_string(), conn);
    }

    /// Drop every parked connection (iteration-end auto-close). Returns how
    /// many were dropped so the caller can decide whether to log.
    pub(crate) fn drain(&self) -> usize {
        let mut inner = self.inner.lock().unwrap();
        let n = inner.conns.len();
        inner.conns.clear();
        n
    }
}

impl std::fmt::Debug for Resources {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.inner.lock().unwrap();
        write!(f, "Resources({} live)", inner.conns.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Registry bookkeeping needs a `WsConn` with a real stream type, but not
    /// a real WebSocket handshake — wrap one side of a loopback TCP pair.
    async fn loopback_conn(listener: &tokio::net::TcpListener) -> WsConn {
        let addr = listener.local_addr().unwrap();
        let (client, _server) = tokio::join!(TcpStream::connect(addr), listener.accept());
        let stream = WebSocketStream::from_raw_socket(
            MaybeTlsStream::Plain(client.unwrap()),
            tokio_tungstenite::tungstenite::protocol::Role::Client,
            None,
        )
        .await;
        WsConn {
            stream,
            url: "ws://test".into(),
            generator: Gen::new(1),
            last_send: None,
            pending: Default::default(),
        }
    }

    #[tokio::test]
    async fn insert_take_put_back_roundtrip() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let res = Resources::default();

        let id = res.insert(loopback_conn(&listener).await);
        assert_eq!(id, "ws-1");

        let conn = res.take(&id).expect("present");
        assert!(res.take(&id).is_none(), "take removes");
        res.put_back(&id, conn);
        assert!(res.take(&id).is_some(), "put_back restores");
    }

    #[tokio::test]
    async fn ids_are_unique_and_drain_clears() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let res = Resources::default();

        assert_eq!(res.insert(loopback_conn(&listener).await), "ws-1");
        assert_eq!(res.insert(loopback_conn(&listener).await), "ws-2");

        assert_eq!(res.drain(), 2);
        assert_eq!(res.drain(), 0, "second drain finds nothing");
        assert!(res.take("ws-1").is_none());
    }

    #[tokio::test]
    async fn clones_share_one_pool() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let res = Resources::default();
        let alias = res.clone();

        let id = res.insert(loopback_conn(&listener).await);
        assert!(alias.take(&id).is_some(), "clone sees the same pool");
    }
}
