use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::{Html, Response};
use axum::{routing::get, routing::post, Json, Router};
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::cli::ServeArgs;
use crate::error::CliError;

/// Bodies accepted on `POST /api/v1/metrics`: the end-of-run summary
/// (`{"lines": [...]}`) or a during-run snapshot batch from
/// `report.during_run` (`{"samples": [...], "seq": N}`).
#[derive(Deserialize)]
#[serde(untagged)]
enum MetricsPayload {
    Summary {
        lines: Vec<String>,
    },
    Snapshot {
        samples: Vec<LiveSample>,
        #[serde(default)]
        seq: Option<u64>,
    },
}

#[derive(Clone, Deserialize, Serialize)]
struct LiveSample {
    metric: String,
    #[serde(default)]
    labels: serde_json::Map<String, serde_json::Value>,
    value: f64,
}

/// One received during-run batch, kept for the dashboard's time series.
#[derive(Serialize)]
struct SnapshotBatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    seq: Option<u64>,
    samples: Vec<LiveSample>,
}

/// Retention for the in-memory dashboard feed: at the default 2s snapshot
/// interval 2000 batches cover ~66 minutes of a run; older batches drop.
const MAX_SNAPSHOT_BATCHES: usize = 2000;
/// End-of-run summaries are rare (one per finished run) but each is a full
/// k6-style block — keep the last few runs only.
const MAX_SUMMARIES: usize = 20;

#[derive(Default)]
struct StateInner {
    snapshots: VecDeque<SnapshotBatch>,
    summaries: VecDeque<Vec<String>>,
}

/// What the dashboard polls on `GET /api/v1/state`.
#[derive(Serialize)]
struct StateView<'a> {
    snapshots: &'a VecDeque<SnapshotBatch>,
    summaries: &'a VecDeque<Vec<String>>,
}

#[derive(Clone, Default)]
struct AppState {
    inner: Arc<Mutex<StateInner>>,
}

impl AppState {
    fn push_snapshot(&self, seq: Option<u64>, samples: Vec<LiveSample>) {
        let mut inner = self.inner.lock().unwrap();
        inner.snapshots.push_back(SnapshotBatch { seq, samples });
        while inner.snapshots.len() > MAX_SNAPSHOT_BATCHES {
            inner.snapshots.pop_front();
        }
    }

    fn push_summary(&self, lines: Vec<String>) {
        let mut inner = self.inner.lock().unwrap();
        inner.summaries.push_back(lines);
        while inner.summaries.len() > MAX_SUMMARIES {
            inner.summaries.pop_front();
        }
    }
}

fn app(ui: bool) -> Router {
    let mut router = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/api/v1/metrics", post(ingest))
        .route("/api/v1/state", get(state_view))
        .route("/ws", get(ws_upgrade));
    if ui {
        router = router.route("/", get(dashboard));
    }
    router.with_state(AppState::default())
}

/// `GET /` — the built-in dashboard (only with the UI enabled, see
/// [`ServeArgs::no_ui`]). A single self-contained page: no CDN, no build
/// step; it polls `/api/v1/state` and draws the time series itself.
async fn dashboard() -> Html<&'static str> {
    Html(include_str!("serve_ui.html"))
}

/// `GET /api/v1/state` — everything the dashboard needs: recent snapshot
/// batches and the last run summaries, oldest first.
async fn state_view(State(state): State<AppState>) -> Json<serde_json::Value> {
    let inner = state.inner.lock().unwrap();
    Json(
        serde_json::to_value(StateView {
            snapshots: &inner.snapshots,
            summaries: &inner.summaries,
        })
        .unwrap_or_else(|_| serde_json::json!({ "snapshots": [], "summaries": [] })),
    )
}

/// Minimal local dev server: receives the aggregated summary that
/// `perfscale run --report <url>` posts after a run and prints it.
///
/// This is a stand-in for a real control-plane — there is no persistence,
/// auth, or multi-run aggregation. It exists so `perfscale run` from several
/// machines/terminals can report to one place during local development.
///
/// Unless `--no-ui` is passed, `/` serves a built-in dashboard (live
/// time-series of during-run snapshots plus the last run summaries) backed
/// by `GET /api/v1/state`.
///
/// With `--tls` the same endpoints are served over HTTPS using a self-signed
/// certificate generated at startup — a local TLS target for load tests
/// (clients must skip certificate verification).
pub async fn serve(args: ServeArgs) -> Result<(), CliError> {
    let addr = SocketAddr::from(([0, 0, 0, 0], args.port));
    let listener = std::net::TcpListener::bind(addr).map_err(|e| {
        CliError::new(format!("failed to bind {addr}"))
            .cause(e.to_string())
            .hint(format!(
                "port {} is likely taken — pick another with `--port <PORT>`, or use `--port 0` \
                 to let the OS choose a free one (printed at startup)",
                args.port
            ))
            .docs("cli/commands.md#perfscale-serve")
    })?;
    listener
        .set_nonblocking(true)
        .map_err(|e| CliError::new("failed to configure listener").cause(e.to_string()))?;
    // Re-read the bound address: if `args.port == 0` the OS picks a free port,
    // and `addr` above still holds the placeholder `0`.
    let bound_addr = listener
        .local_addr()
        .map_err(|e| CliError::new("failed to read bound address").cause(e.to_string()))?;

    let server_error = |e: std::io::Error| {
        CliError::new("server error")
            .cause(e.to_string())
            .docs("cli/commands.md#perfscale-serve")
    };

    let router = app(!args.no_ui);

    if args.tls {
        // reqwest's rustls-tls (ring) is also in this process, so rustls sees
        // more than one provider and needs an explicit process default.
        let _ = rustls::crypto::ring::default_provider().install_default();

        let config = tls_config().await?;
        info!(addr = %bound_addr, "perfscale serve listening (tls)");
        println!("perfscale serve listening on https://{bound_addr} (self-signed certificate)");
        if !args.no_ui {
            println!("perfscale serve dashboard on https://{bound_addr}/ (--no-ui to disable)");
        }

        axum_server::from_tcp_rustls(listener, config)
            .serve(router.into_make_service())
            .await
            .map_err(server_error)
    } else {
        let listener = tokio::net::TcpListener::from_std(listener)
            .map_err(|e| CliError::new("failed to configure listener").cause(e.to_string()))?;

        info!(addr = %bound_addr, "perfscale serve listening");
        println!("perfscale serve listening on http://{bound_addr}");
        if !args.no_ui {
            println!("perfscale serve dashboard on http://{bound_addr}/ (--no-ui to disable)");
        }

        axum::serve(listener, router).await.map_err(server_error)
    }
}

/// Build a rustls config around a fresh self-signed certificate for
/// `localhost`/`127.0.0.1`. Generated per process start — nothing touches
/// the filesystem, and the throwaway key never needs rotation or storage.
async fn tls_config() -> Result<axum_server::tls_rustls::RustlsConfig, CliError> {
    let certified =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()])
            .map_err(|e| {
                CliError::new("failed to generate self-signed certificate").cause(e.to_string())
            })?;

    axum_server::tls_rustls::RustlsConfig::from_pem(
        certified.cert.pem().into_bytes(),
        certified.key_pair.serialize_pem().into_bytes(),
    )
    .await
    .map_err(|e| CliError::new("failed to build TLS config").cause(e.to_string()))
}

async fn ingest(State(state): State<AppState>, Json(payload): Json<MetricsPayload>) -> &'static str {
    match payload {
        MetricsPayload::Summary { lines } => {
            println!("--- metrics batch ({} lines) ---", lines.len());
            for line in &lines {
                println!("  {line}");
            }
            state.push_summary(lines);
        }
        MetricsPayload::Snapshot { samples, seq } => {
            let seq_label = seq.map_or_else(|| "?".to_string(), |s| s.to_string());
            println!("--- live snapshot #{seq_label} ({} samples) ---", samples.len());
            for s in &samples {
                println!("  {}{} {}", s.metric, format_labels(&s.labels), s.value);
            }
            state.push_snapshot(seq, samples);
        }
    }
    "ok"
}

/// Render labels Prometheus-style (`{quantile="0.95",gpu="0"}`); empty → "".
fn format_labels(labels: &serde_json::Map<String, serde_json::Value>) -> String {
    if labels.is_empty() {
        return String::new();
    }
    let parts: Vec<String> = labels
        .iter()
        .map(|(k, v)| match v {
            serde_json::Value::String(s) => format!("{k}=\"{s}\""),
            other => format!("{k}=\"{other}\""),
        })
        .collect();
    format!("{{{}}}", parts.join(","))
}

/// `GET /ws` — WebSocket echo endpoint: a loopback target for WebSocket load
/// tests and the `ws` benchmark suite. Every text (and binary) message is
/// echoed back verbatim.
async fn ws_upgrade(ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(ws_echo)
}

async fn ws_echo(mut socket: WebSocket) {
    while let Some(Ok(msg)) = socket.recv().await {
        let reply = match msg {
            Message::Text(t) => Message::Text(t),
            Message::Binary(b) => Message::Binary(b),
            Message::Ping(p) => Message::Pong(p),
            Message::Close(_) => break,
            Message::Pong(_) => continue,
        };
        if socket.send(reply).await.is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use super::*;

    #[tokio::test]
    async fn health_route_returns_ok() {
        let response = app(false)
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"ok");
    }

    #[tokio::test]
    async fn health_route_rejects_post() {
        let response = app(false)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn metrics_route_accepts_json_batch() {
        let body = serde_json::json!({ "lines": ["a", "b"] }).to_string();
        let request = Request::builder()
            .method("POST")
            .uri("/api/v1/metrics")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();
        let response = app(false).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"ok");
    }

    #[tokio::test]
    async fn metrics_route_accepts_empty_lines() {
        let request = Request::builder()
            .method("POST")
            .uri("/api/v1/metrics")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::json!({ "lines": [] }).to_string()))
            .unwrap();
        let response = app(false).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn metrics_route_accepts_during_run_snapshot() {
        let body = serde_json::json!({
            "seq": 3,
            "samples": [
                {"metric": "llm_ttft_ms", "labels": {"quantile": "0.95"}, "value": 1.2, "ts": "1970-01-01T00:00:00.007Z"},
                {"metric": "gpu_utilization_pct", "labels": {"gpu": "0"}, "value": 97.0, "ts": 7}
            ]
        })
        .to_string();
        let request = Request::builder()
            .method("POST")
            .uri("/api/v1/metrics")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();
        let response = app(false).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn format_labels_renders_prometheus_style() {
        let labels = serde_json::json!({"gpu": "0", "quantile": "0.95"});
        assert_eq!(
            super::format_labels(labels.as_object().unwrap()),
            r#"{gpu="0",quantile="0.95"}"#
        );
        assert_eq!(super::format_labels(&serde_json::Map::new()), "");
    }

    #[tokio::test]
    async fn metrics_route_rejects_syntactically_invalid_json() {
        let request = Request::builder()
            .method("POST")
            .uri("/api/v1/metrics")
            .header("content-type", "application/json")
            .body(Body::from("not json"))
            .unwrap();
        let response = app(false).oneshot(request).await.unwrap();
        // Syntax errors are a 400 (Bad Request); a well-formed-but-wrong-shape
        // body (see below) is a 422 — axum distinguishes the two.
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn metrics_route_rejects_missing_lines_field() {
        let request = Request::builder()
            .method("POST")
            .uri("/api/v1/metrics")
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .unwrap();
        let response = app(false).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn ws_route_echoes_text_messages() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite;

        // WebSocket needs a live server — tower's oneshot can't do the
        // upgrade handshake.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app(false)).await.unwrap();
        });

        let url = format!("ws://127.0.0.1:{}/ws", addr.port());
        let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
        socket
            .send(tungstenite::Message::Text("hello echo".into()))
            .await
            .unwrap();
        let reply = socket.next().await.unwrap().unwrap();
        assert_eq!(
            reply,
            tungstenite::Message::Text("hello echo".into()),
            "echoed message must match the sent one"
        );

        server.abort();
    }

    #[tokio::test]
    async fn tls_config_builds_from_generated_cert() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        assert!(tls_config().await.is_ok());
    }

    #[tokio::test]
    async fn tls_serve_responds_to_insecure_https_client() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let config = tls_config().await.unwrap();

        let server = tokio::spawn(async move {
            axum_server::from_tcp_rustls(listener, config)
                .serve(app(false).into_make_service())
                .await
                .unwrap();
        });

        // Self-signed certificate → verification must be skipped, exactly like
        // load-test clients pointed at `serve --tls` do.
        let client = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .build()
            .unwrap();
        let body = client
            .get(format!("https://127.0.0.1:{}/health", addr.port()))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(body, "ok");
        server.abort();
    }

    #[tokio::test]
    async fn unknown_route_is_404() {
        let response = app(false)
            .oneshot(Request::builder().uri("/nope").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn dashboard_is_served_when_ui_enabled() {
        let response = app(true)
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("<title>perfscale serve</title>"), "{html}");
        // Self-contained: the dashboard must not depend on any CDN.
        assert!(!html.contains("https://cdn"), "{html}");
    }

    #[tokio::test]
    async fn dashboard_is_404_when_ui_disabled() {
        let response = app(false)
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn state_route_starts_empty() {
        let response = app(false)
            .oneshot(
                Request::builder()
                    .uri("/api/v1/state")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json, serde_json::json!({ "snapshots": [], "summaries": [] }));
    }

    /// POST a snapshot and a summary to a live server, then read them back
    /// through `/api/v1/state` exactly like the dashboard does.
    #[tokio::test]
    async fn state_route_reflects_ingested_metrics() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app(true)).await.unwrap();
        });
        let base = format!("http://127.0.0.1:{}", addr.port());
        let client = reqwest::Client::new();

        client
            .post(format!("{base}/api/v1/metrics"))
            .json(&serde_json::json!({
                "seq": 1,
                "samples": [{"metric": "llm_tokens_per_sec", "labels": {}, "value": 42.0}]
            }))
            .send()
            .await
            .unwrap();
        client
            .post(format!("{base}/api/v1/metrics"))
            .json(&serde_json::json!({ "lines": ["http_reqs: 10 2.00/s"] }))
            .send()
            .await
            .unwrap();

        let state: serde_json::Value = client
            .get(format!("{base}/api/v1/state"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(state["snapshots"][0]["seq"], 1);
        assert_eq!(
            state["snapshots"][0]["samples"][0]["metric"],
            "llm_tokens_per_sec"
        );
        assert_eq!(
            state["summaries"][0][0],
            serde_json::json!("http_reqs: 10 2.00/s")
        );

        server.abort();
    }
}
