use std::net::SocketAddr;

use axum::{routing::get, routing::post, Json, Router};
use serde::Deserialize;
use tracing::info;

use crate::cli::ServeArgs;
use crate::error::CliError;

#[derive(Deserialize)]
struct MetricsPayload {
    lines: Vec<String>,
}

fn app() -> Router {
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/api/v1/metrics", post(ingest))
}

/// Minimal local dev server: receives the aggregated summary that
/// `perfscale run --report <url>` posts after a run and prints it.
///
/// This is a stand-in for a real control-plane — there is no persistence,
/// auth, or multi-run aggregation. It exists so `perfscale run` from several
/// machines/terminals can report to one place during local development.
pub async fn serve(args: ServeArgs) -> Result<(), CliError> {
    let addr = SocketAddr::from(([0, 0, 0, 0], args.port));
    let listener = tokio::net::TcpListener::bind(addr).await.map_err(|e| {
        CliError::new(format!("failed to bind {addr}"))
            .cause(e.to_string())
            .hint(format!(
                "port {} is likely taken — pick another with `--port <PORT>`, or use `--port 0` \
                 to let the OS choose a free one (printed at startup)",
                args.port
            ))
            .docs("cli/commands.md#perfscale-serve")
    })?;
    // Re-read the bound address: if `args.port == 0` the OS picks a free port,
    // and `addr` above still holds the placeholder `0`.
    let bound_addr = listener
        .local_addr()
        .map_err(|e| CliError::new("failed to read bound address").cause(e.to_string()))?;

    info!(addr = %bound_addr, "perfscale serve listening");
    println!("perfscale serve listening on http://{bound_addr}");

    axum::serve(listener, app()).await.map_err(|e| {
        CliError::new("server error")
            .cause(e.to_string())
            .docs("cli/commands.md#perfscale-serve")
    })
}

async fn ingest(Json(payload): Json<MetricsPayload>) -> &'static str {
    println!("--- metrics batch ({} lines) ---", payload.lines.len());
    for line in &payload.lines {
        println!("  {line}");
    }
    "ok"
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
        let response = app()
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
        let response = app()
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
        let response = app().oneshot(request).await.unwrap();
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
        let response = app().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn metrics_route_rejects_syntactically_invalid_json() {
        let request = Request::builder()
            .method("POST")
            .uri("/api/v1/metrics")
            .header("content-type", "application/json")
            .body(Body::from("not json"))
            .unwrap();
        let response = app().oneshot(request).await.unwrap();
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
        let response = app().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn unknown_route_is_404() {
        let response = app()
            .oneshot(Request::builder().uri("/nope").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
