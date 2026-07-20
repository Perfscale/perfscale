//! Live-connection registry — stateful resources that span steps.
//!
//! `Context.vars` holds only JSON, so an open WebSocket or gRPC channel
//! cannot live there. Instead, `std/ws-connect@v1` / `std/grpc-connect@v1`
//! park the socket/channel here and return a JSON-safe **Connection ID**
//! (`"ws-1"`, `"grpc-1"`, …); later steps look it up by that id. The gRPC
//! family additionally parks open streams (`"grpcs-1"`, …) and caches
//! reflection-fetched schemas per URL.
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

use prost_reflect::{DescriptorPool, DynamicMessage, MethodDescriptor};
use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tonic::transport::Channel;

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

/// One live gRPC channel (one HTTP/2 connection) plus its message schema.
///
/// gRPC has no sub-protocol negotiation, but the channel carries the
/// [`DescriptorPool`] the calls on it are decoded with, so `grpc-call` and
/// the `grpc-stream-*` family never need the schema parameters again.
pub(crate) struct GrpcConn {
    pub channel: Channel,
    /// Target URL as given (`grpc://` / `grpcs://`), kept for log lines.
    pub url: String,
    /// Message schema resolved at connect time (`descriptor_set` or
    /// `reflection: true`).
    pub pool: DescriptorPool,
    /// `${…}` generator — `${seq}` keeps counting across sends on the same
    /// channel (payload string leaves expand per call).
    pub generator: Gen,
    /// Metadata from `grpc-connect`, applied to every call on this channel;
    /// per-call metadata wins on key conflict.
    pub metadata: Vec<(String, String)>,
    /// Inbound message cap (bytes) for calls on this channel.
    pub max_recv_size: usize,
}

/// One open gRPC stream (client-streaming, bidi, or an in-progress
/// server-streaming call) parked between steps.
///
/// The RPC itself runs in a relay task spawned at open: it owns the tonic
/// response stream and forwards decoded messages (or the terminating
/// [`tonic::Status`]) over an mpsc channel. That decouples `stream-open`
/// from the server's initial-metadata timing — a client-streaming server
/// typically sends headers only after the client half-closes, so awaiting
/// them at open would deadlock the step sequence. A closed receiver means
/// the stream ended cleanly (status OK); an `Err` item is the final status.
pub(crate) struct GrpcStream {
    /// Request side — `Some` for client-streaming and bidi methods, `None`
    /// for server-streaming (whose single request was sent at open).
    pub sender: Option<tokio::sync::mpsc::Sender<DynamicMessage>>,
    /// Response side, fed by the relay task.
    pub receiver: tokio::sync::mpsc::Receiver<Result<DynamicMessage, tonic::Status>>,
    /// The method this stream is an instance of (kind, input/output types).
    pub method: MethodDescriptor,
    /// `${…}` generator — `${seq}` keeps counting across sends on the same
    /// stream (payload string leaves expand per send).
    pub generator: Gen,
    /// Channel URL, kept for log lines.
    pub url: String,
    /// When the last `grpc-stream-send` finished, so a following
    /// `grpc-stream-recv` with an until-condition can report the
    /// send→match Message RTT.
    pub last_send: Option<Instant>,
}

#[derive(Default)]
struct Inner {
    next_id: u64,
    conns: HashMap<String, WsConn>,
    next_grpc_id: u64,
    grpc_conns: HashMap<String, GrpcConn>,
    next_grpc_stream_id: u64,
    grpc_streams: HashMap<String, GrpcStream>,
    /// Reflection-fetched schema, per URL — repeated `grpc-connect` steps to
    /// the same server within one iteration reuse it.
    reflection_pools: HashMap<String, DescriptorPool>,
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

    /// Park a gRPC channel and mint its Connection ID (`grpc-1`, `grpc-2`, …).
    pub(crate) fn insert_grpc(&self, conn: GrpcConn) -> String {
        let mut inner = self.inner.lock().unwrap();
        inner.next_grpc_id += 1;
        let id = format!("grpc-{}", inner.next_grpc_id);
        inner.grpc_conns.insert(id.clone(), conn);
        id
    }

    /// Remove a gRPC channel for exclusive use by one step.
    pub(crate) fn take_grpc(&self, id: &str) -> Option<GrpcConn> {
        self.inner.lock().unwrap().grpc_conns.remove(id)
    }

    /// Return a gRPC channel taken with [`take_grpc`](Self::take_grpc).
    pub(crate) fn put_back_grpc(&self, id: &str, conn: GrpcConn) {
        self.inner
            .lock()
            .unwrap()
            .grpc_conns
            .insert(id.to_string(), conn);
    }

    /// Park an open gRPC stream and mint its Stream ID (`grpcs-1`, …).
    pub(crate) fn insert_grpc_stream(&self, stream: GrpcStream) -> String {
        let mut inner = self.inner.lock().unwrap();
        inner.next_grpc_stream_id += 1;
        let id = format!("grpcs-{}", inner.next_grpc_stream_id);
        inner.grpc_streams.insert(id.clone(), stream);
        id
    }

    /// Remove a gRPC stream for exclusive use by one step.
    pub(crate) fn take_grpc_stream(&self, id: &str) -> Option<GrpcStream> {
        self.inner.lock().unwrap().grpc_streams.remove(id)
    }

    /// Return a gRPC stream taken with
    /// [`take_grpc_stream`](Self::take_grpc_stream).
    pub(crate) fn put_back_grpc_stream(&self, id: &str, stream: GrpcStream) {
        self.inner
            .lock()
            .unwrap()
            .grpc_streams
            .insert(id.to_string(), stream);
    }

    /// Schema cached from an earlier reflection fetch to the same URL.
    pub(crate) fn reflection_pool(&self, url: &str) -> Option<DescriptorPool> {
        self.inner
            .lock()
            .unwrap()
            .reflection_pools
            .get(url)
            .cloned()
    }

    /// Cache a reflection-fetched schema under its URL.
    pub(crate) fn cache_reflection_pool(&self, url: &str, pool: DescriptorPool) {
        self.inner
            .lock()
            .unwrap()
            .reflection_pools
            .insert(url.to_string(), pool);
    }

    /// Drop every parked connection (iteration-end auto-close). Returns how
    /// many were dropped so the caller can decide whether to log. gRPC
    /// channels close with their last handle; open streams are cancelled by
    /// dropping their request sender and receiver.
    pub(crate) fn drain(&self) -> usize {
        let mut inner = self.inner.lock().unwrap();
        let n = inner.conns.len() + inner.grpc_conns.len() + inner.grpc_streams.len();
        inner.conns.clear();
        inner.grpc_conns.clear();
        inner.grpc_streams.clear();
        n
    }
}

impl std::fmt::Debug for Resources {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.inner.lock().unwrap();
        write!(
            f,
            "Resources({} ws + {} grpc + {} streams live)",
            inner.conns.len(),
            inner.grpc_conns.len(),
            inner.grpc_streams.len()
        )
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

    // -----------------------------------------------------------------
    // gRPC registry
    // -----------------------------------------------------------------

    fn lazy_grpc_conn() -> GrpcConn {
        use prost_reflect::DescriptorPool;
        use tonic::transport::Endpoint;
        let channel = Endpoint::from_static("http://127.0.0.1:1").connect_lazy();
        let pool = DescriptorPool::decode(crate::testsupport::ECHO_DESCRIPTOR_SET).unwrap();
        GrpcConn {
            channel,
            url: "grpc://127.0.0.1:1".into(),
            pool,
            generator: Gen::new(1),
            metadata: Vec::new(),
            max_recv_size: 1 << 20,
        }
    }

    fn dummy_grpc_stream() -> GrpcStream {
        use prost_reflect::DescriptorPool;
        let pool = DescriptorPool::decode(crate::testsupport::ECHO_DESCRIPTOR_SET).unwrap();
        let method = pool
            .get_service_by_name("perfscale.test.v1.Echo")
            .unwrap()
            .methods()
            .find(|m| m.name() == "Bidi")
            .unwrap();
        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        GrpcStream {
            sender: None,
            receiver: rx,
            method,
            generator: Gen::new(1),
            url: "grpc://127.0.0.1:1".into(),
            last_send: None,
        }
    }

    #[tokio::test]
    async fn grpc_insert_take_put_back_roundtrip() {
        let res = Resources::default();

        let id = res.insert_grpc(lazy_grpc_conn());
        assert_eq!(id, "grpc-1");

        let conn = res.take_grpc(&id).expect("present");
        assert!(res.take_grpc(&id).is_none(), "take removes");
        res.put_back_grpc(&id, conn);
        assert!(res.take_grpc(&id).is_some(), "put_back restores");
    }

    #[tokio::test]
    async fn grpc_stream_ids_and_drain_covers_everything() {
        let res = Resources::default();

        assert_eq!(res.insert_grpc_stream(dummy_grpc_stream()), "grpcs-1");
        assert_eq!(res.insert_grpc_stream(dummy_grpc_stream()), "grpcs-2");
        res.insert_grpc(lazy_grpc_conn());

        assert_eq!(res.drain(), 3, "two streams + one channel dropped");
        assert_eq!(res.drain(), 0);
        assert!(res.take_grpc_stream("grpcs-1").is_none());
        assert!(res.take_grpc("grpc-1").is_none());
    }

    #[tokio::test]
    async fn reflection_pool_cache_survives_drain() {
        use prost_reflect::DescriptorPool;
        let res = Resources::default();
        let pool = DescriptorPool::decode(crate::testsupport::ECHO_DESCRIPTOR_SET).unwrap();

        assert!(res.reflection_pool("grpc://x").is_none());
        res.cache_reflection_pool("grpc://x", pool);
        assert!(res.reflection_pool("grpc://x").is_some());
        // The schema cache is not connection state — it survives the
        // iteration-end drain so the next iteration skips the round trip.
        res.drain();
        assert!(res.reflection_pool("grpc://x").is_some());
    }
}
