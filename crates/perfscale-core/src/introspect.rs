//! Public introspection API — schema reflection and connectivity probes for
//! external tooling (e.g. the control-plane editor) that embeds the engine.
//!
//! These functions are **not** step actions: they are plain library calls
//! sharing the engine's transport code paths (`step::grpc` for the gRPC
//! reflection protocol, `step::ws` for the WebSocket handshake, `step::db`
//! for database connections), so what a probe reports is what a run would do.
//!
//! - [`reflect_schema`] — connect to a gRPC server, fetch its schema via the
//!   v1 reflection protocol, and render every service/method with JSON
//!   skeletons for the request and response messages.
//! - [`probe_ws`] — one WebSocket handshake with timing: reachability,
//!   negotiated subprotocol, or the failure reason.
//! - [`probe_db`] — one database connection + `SELECT 1` round-trip with
//!   timing; errors are DSN-sanitized.

use std::time::Instant;

use prost_reflect::{DescriptorPool, FieldDescriptor, Kind, MessageDescriptor};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tokio::time::Duration;
use tonic::transport::Endpoint;

use crate::step::actions::error_chain;
use crate::step::grpc::fetch_reflection_pool;
use crate::step::ws::{ws_handshake, Profile};

pub use crate::step::db::probe_db;

// ---------------------------------------------------------------------------
// gRPC schema reflection
// ---------------------------------------------------------------------------

/// One RPC method with JSON skeletons of its request and response messages.
/// `name` is the engine's call form: `"package.Service/Method"`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MethodSchema {
    pub name: String,
    pub input_json: Value,
    pub output_json: Value,
}

/// One gRPC service with all its methods.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServiceSchema {
    pub name: String,
    pub methods: Vec<MethodSchema>,
}

/// Connect through `endpoint` (the caller owns TLS/plaintext configuration),
/// fetch the server's schema via the v1 reflection protocol, and enumerate
/// every service with JSON skeletons for each method's input/output.
///
/// The skeletons follow protobuf-JSON shape: field keys are `jsonName`s,
/// scalars carry their zero value (`""`, `0`, `false`), enums their first
/// variant name, messages a nested object, repeated fields a single-element
/// array, maps a single-entry object. Nesting is capped at
/// [`MAX_SKELETON_DEPTH`] levels (cycles included); `google.protobuf.Any`
/// renders `null` — its payload is opaque until a concrete type is known.
pub async fn reflect_schema(endpoint: Endpoint) -> Result<Vec<ServiceSchema>, String> {
    let channel = endpoint.connect().await.map_err(|e| error_chain(&e))?;
    let pool = fetch_reflection_pool(&channel).await?;
    Ok(schema_from_pool(&pool))
}

/// Depth limit for nested message expansion — the root message is depth 0,
/// so skeletons show this many object levels before rendering `null`.
pub const MAX_SKELETON_DEPTH: usize = 5;

/// Render every service in the pool (reflection services are already
/// filtered out by [`fetch_reflection_pool`]).
fn schema_from_pool(pool: &DescriptorPool) -> Vec<ServiceSchema> {
    pool.services()
        .map(|service| ServiceSchema {
            name: service.full_name().to_string(),
            methods: service
                .methods()
                .map(|method| MethodSchema {
                    name: format!("{}/{}", service.full_name(), method.name()),
                    input_json: message_skeleton(&method.input()),
                    output_json: message_skeleton(&method.output()),
                })
                .collect(),
        })
        .collect()
}

/// JSON skeleton of one message, starting at depth 0.
fn message_skeleton(desc: &MessageDescriptor) -> Value {
    message_at(desc, 0, &mut Vec::new())
}

/// Object form of `desc` — or `null` when the depth cap is reached, a cycle
/// would close (`path` holds the message names on the way down), or the type
/// is opaque (`google.protobuf.Any`).
fn message_at(desc: &MessageDescriptor, depth: usize, path: &mut Vec<String>) -> Value {
    if desc.full_name() == "google.protobuf.Any"
        || depth >= MAX_SKELETON_DEPTH
        || path.iter().any(|p| p == desc.full_name())
    {
        return Value::Null;
    }
    path.push(desc.full_name().to_string());
    let mut obj = Map::new();
    for field in desc.fields() {
        obj.insert(
            field.json_name().to_string(),
            field_skeleton(&field, depth, path),
        );
    }
    path.pop();
    Value::Object(obj)
}

/// Skeleton of one field: repeated → single-element array, map →
/// single-entry object, everything else per its [`Kind`].
fn field_skeleton(field: &FieldDescriptor, depth: usize, path: &mut Vec<String>) -> Value {
    if field.is_map() {
        // `is_map` implies a message kind holding the synthetic entry type.
        let Kind::Message(entry) = field.kind() else {
            return Value::Null;
        };
        let key = map_key_placeholder(&entry.map_entry_key_field().kind());
        let value = kind_skeleton(&entry.map_entry_value_field().kind(), depth, path);
        let mut obj = Map::new();
        obj.insert(key.to_string(), value);
        return Value::Object(obj);
    }
    let element = kind_skeleton(&field.kind(), depth, path);
    if field.is_list() {
        return json!([element]);
    }
    element
}

/// Protobuf-JSON zero value of a scalar kind; enums render their first
/// variant name, messages recurse (one level deeper).
fn kind_skeleton(kind: &Kind, depth: usize, path: &mut Vec<String>) -> Value {
    match kind {
        Kind::Double
        | Kind::Float
        | Kind::Int32
        | Kind::Int64
        | Kind::Uint32
        | Kind::Uint64
        | Kind::Sint32
        | Kind::Sint64
        | Kind::Fixed32
        | Kind::Fixed64
        | Kind::Sfixed32
        | Kind::Sfixed64 => json!(0),
        Kind::Bool => json!(false),
        // Bytes are base64 strings in protobuf-JSON; empty is the zero value.
        Kind::String | Kind::Bytes => json!(""),
        Kind::Enum(e) => e
            .values()
            .next()
            .map(|v| json!(v.name()))
            .unwrap_or(Value::Null),
        Kind::Message(m) => message_at(m, depth + 1, path),
    }
}

/// The single key a map skeleton shows, shaped to the key type so the entry
/// stays valid protobuf-JSON (map keys serialize as JSON strings).
fn map_key_placeholder(kind: &Kind) -> &'static str {
    match kind {
        Kind::Bool => "false",
        Kind::String => "key",
        _ => "0", // integral kinds
    }
}

// ---------------------------------------------------------------------------
// WebSocket probe
// ---------------------------------------------------------------------------

/// Outcome of one WebSocket handshake attempt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WsProbeResult {
    /// The handshake completed (TCP + TLS + HTTP upgrade).
    pub ok: bool,
    /// Wall time of the attempt, success or failure.
    pub latency_ms: u64,
    /// The subprotocol the server negotiated, if any.
    pub subprotocol: Option<String>,
    /// Failure reason when `ok` is false (transport error or timeout).
    pub error: Option<String>,
}

/// Perform a single WebSocket handshake against `url` and report how it
/// went. Uses the same handshake path as the `std/ws*@v1` actions, so
/// `skip_tls_verify`, `headers`, and `subprotocols` behave exactly as in a
/// run. Never panics and never hangs past `timeout_ms`: the timeout wraps
/// the whole handshake, and failures return `ok: false` with the error.
pub async fn probe_ws(
    url: &str,
    headers: Vec<(String, String)>,
    subprotocols: Vec<String>,
    skip_tls_verify: bool,
    timeout_ms: u64,
) -> WsProbeResult {
    let profile = Profile::new(url, headers, subprotocols, skip_tls_verify);
    let t0 = Instant::now();
    let result =
        tokio::time::timeout(Duration::from_millis(timeout_ms), ws_handshake(&profile)).await;
    let latency_ms = t0.elapsed().as_millis() as u64;

    match result {
        Ok(Ok((stream, subprotocol))) => {
            // A probe only proves reachability — no close handshake.
            drop(stream);
            WsProbeResult {
                ok: true,
                latency_ms,
                subprotocol,
                error: None,
            }
        }
        Ok(Err(msg)) => WsProbeResult {
            ok: false,
            latency_ms,
            subprotocol: None,
            error: Some(msg),
        },
        Err(_) => WsProbeResult {
            ok: false,
            latency_ms,
            subprotocol: None,
            error: Some(format!("handshake timeout after {latency_ms}ms")),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------
    // Skeleton rendering — hand-built pool (no server)
    // -----------------------------------------------------------------

    /// A pool covering every scalar kind, a nested message, an enum,
    /// repeated + map fields, a self-recursive message, a depth chain, and
    /// google.protobuf.Any (as a second file).
    fn test_pool() -> DescriptorPool {
        use prost_types::field_descriptor_proto::{Label, Type};
        use prost_types::{
            DescriptorProto, EnumDescriptorProto, EnumValueDescriptorProto, FieldDescriptorProto,
            FileDescriptorProto, FileDescriptorSet, MessageOptions,
        };

        fn field(
            name: &str,
            number: i32,
            label: Label,
            ty: Type,
            type_name: Option<&str>,
        ) -> FieldDescriptorProto {
            FieldDescriptorProto {
                name: Some(name.into()),
                number: Some(number),
                label: Some(label as i32),
                r#type: Some(ty as i32),
                type_name: type_name.map(Into::into),
                ..Default::default()
            }
        }
        let opt = Label::Optional;
        let rep = Label::Repeated;
        let msg = |name: &str, fields: Vec<FieldDescriptorProto>| DescriptorProto {
            name: Some(name.into()),
            field: fields,
            ..Default::default()
        };
        let map_entry =
            |name: &str, value_ty: Type, value_type_name: Option<&str>| DescriptorProto {
                name: Some(name.into()),
                field: vec![
                    field("key", 1, opt, Type::String, None),
                    field("value", 2, opt, value_ty, value_type_name),
                ],
                options: Some(MessageOptions {
                    map_entry: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            };

        let color = EnumDescriptorProto {
            name: Some("Color".into()),
            value: vec![
                EnumValueDescriptorProto {
                    name: Some("COLOR_UNSPECIFIED".into()),
                    number: Some(0),
                    ..Default::default()
                },
                EnumValueDescriptorProto {
                    name: Some("COLOR_RED".into()),
                    number: Some(1),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        let nested = msg(
            "Nested",
            vec![
                field("text_value", 1, opt, Type::String, None),
                field("nums", 2, rep, Type::Int32, None),
            ],
        );

        let mut scalars = msg(
            "Scalars",
            vec![
                field("f_double", 1, opt, Type::Double, None),
                field("f_float", 2, opt, Type::Float, None),
                field("f_int32", 3, opt, Type::Int32, None),
                field("f_int64", 4, opt, Type::Int64, None),
                field("f_uint32", 5, opt, Type::Uint32, None),
                field("f_uint64", 6, opt, Type::Uint64, None),
                field("f_sint32", 7, opt, Type::Sint32, None),
                field("f_sint64", 8, opt, Type::Sint64, None),
                field("f_fixed32", 9, opt, Type::Fixed32, None),
                field("f_fixed64", 10, opt, Type::Fixed64, None),
                field("f_sfixed32", 11, opt, Type::Sfixed32, None),
                field("f_sfixed64", 12, opt, Type::Sfixed64, None),
                field("f_bool", 13, opt, Type::Bool, None),
                field("f_string", 14, opt, Type::String, None),
                field("f_bytes", 15, opt, Type::Bytes, None),
                field("f_color", 16, opt, Type::Enum, Some(".t.Color")),
                field("f_nested", 17, opt, Type::Message, Some(".t.Nested")),
                field("f_tags", 18, rep, Type::String, None),
                field(
                    "f_counts",
                    19,
                    rep,
                    Type::Message,
                    Some(".t.Scalars.CountsEntry"),
                ),
                field(
                    "f_lookup",
                    20,
                    rep,
                    Type::Message,
                    Some(".t.Scalars.LookupEntry"),
                ),
            ],
        );
        scalars.nested_type = vec![
            map_entry("CountsEntry", Type::Int32, None),
            map_entry("LookupEntry", Type::Message, Some(".t.Nested")),
        ];

        let loop_msg = msg(
            "Loop",
            vec![
                field("child", 1, opt, Type::Message, Some(".t.Loop")),
                field("kids", 2, rep, Type::Message, Some(".t.Loop")),
            ],
        );

        let holds_any = msg(
            "HoldsAny",
            vec![field(
                "payload",
                1,
                opt,
                Type::Message,
                Some(".google.protobuf.Any"),
            )],
        );

        // L1 → L2 → … → L6, one `next` field each; L6 holds a scalar.
        let mut chain = Vec::new();
        for i in (1..=6).rev() {
            let fields = if i == 6 {
                vec![field("n", 1, opt, Type::Int32, None)]
            } else {
                vec![field(
                    "next",
                    1,
                    opt,
                    Type::Message,
                    Some(&format!(".t.L{}", i + 1)),
                )]
            };
            chain.push(msg(&format!("L{i}"), fields));
        }

        let any_file = FileDescriptorProto {
            name: Some("google/protobuf/any.proto".into()),
            package: Some("google.protobuf".into()),
            syntax: Some("proto3".into()),
            message_type: vec![msg(
                "Any",
                vec![
                    field("type_url", 1, opt, Type::String, None),
                    field("value", 2, opt, Type::Bytes, None),
                ],
            )],
            ..Default::default()
        };
        let main = FileDescriptorProto {
            name: Some("t.proto".into()),
            package: Some("t".into()),
            syntax: Some("proto3".into()),
            dependency: vec!["google/protobuf/any.proto".into()],
            message_type: {
                let mut v = vec![nested, scalars, loop_msg, holds_any];
                v.extend(chain);
                v
            },
            enum_type: vec![color],
            ..Default::default()
        };

        DescriptorPool::from_file_descriptor_set(FileDescriptorSet {
            file: vec![any_file, main],
        })
        .unwrap()
    }

    fn skeleton(pool: &DescriptorPool, message: &str) -> Value {
        message_skeleton(&pool.get_message_by_name(message).unwrap())
    }

    #[test]
    fn skeleton_scalars_use_protobuf_json_defaults() {
        let pool = test_pool();
        let skel = skeleton(&pool, "t.Scalars");
        // json_name (camelCase) is the object key, as in protobuf-JSON.
        for key in [
            "fDouble",
            "fFloat",
            "fInt32",
            "fInt64",
            "fUint32",
            "fUint64",
            "fSint32",
            "fSint64",
            "fFixed32",
            "fFixed64",
            "fSfixed32",
            "fSfixed64",
        ] {
            assert_eq!(skel[key], json!(0), "key {key}");
        }
        assert_eq!(skel["fBool"], json!(false));
        assert_eq!(skel["fString"], json!(""));
        assert_eq!(skel["fBytes"], json!(""), "bytes are base64 strings");
    }

    #[test]
    fn skeleton_nested_message_recurses() {
        let pool = test_pool();
        let skel = skeleton(&pool, "t.Scalars");
        assert_eq!(skel["fNested"], json!({ "textValue": "", "nums": [0] }));
    }

    #[test]
    fn skeleton_enum_is_first_variant_name() {
        let pool = test_pool();
        let skel = skeleton(&pool, "t.Scalars");
        assert_eq!(skel["fColor"], json!("COLOR_UNSPECIFIED"));
    }

    #[test]
    fn skeleton_repeated_is_single_element_array() {
        let pool = test_pool();
        let skel = skeleton(&pool, "t.Scalars");
        assert_eq!(skel["fTags"], json!([""]));
    }

    #[test]
    fn skeleton_map_is_single_entry_object() {
        let pool = test_pool();
        let skel = skeleton(&pool, "t.Scalars");
        assert_eq!(skel["fCounts"], json!({ "key": 0 }));
        assert_eq!(
            skel["fLookup"],
            json!({ "key": { "textValue": "", "nums": [0] } })
        );
    }

    #[test]
    fn skeleton_caps_depth_and_breaks_cycles() {
        let pool = test_pool();
        // Depth 0..=4 render objects; the message at depth 5 renders null.
        let skel = skeleton(&pool, "t.L1");
        assert!(skel["next"]["next"]["next"]["next"].is_object());
        assert_eq!(
            skel["next"]["next"]["next"]["next"]["next"],
            Value::Null,
            "L6 sits at depth {MAX_SKELETON_DEPTH}"
        );

        // The cycle guard stops a self-referencing message immediately.
        let skel = skeleton(&pool, "t.Loop");
        assert_eq!(skel, json!({ "child": null, "kids": [null] }));
    }

    #[test]
    fn skeleton_any_is_null() {
        let pool = test_pool();
        let skel = skeleton(&pool, "t.HoldsAny");
        assert_eq!(skel, json!({ "payload": null }));
    }

    // -----------------------------------------------------------------
    // reflect_schema against the live echo server (reflection enabled)
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn reflect_schema_lists_services_with_skeletons() {
        let port = crate::testsupport::start_echo_server().await;
        let endpoint =
            Endpoint::from_shared(format!("http://127.0.0.1:{port}")).expect("valid uri");
        let services = reflect_schema(endpoint).await.expect("reflection works");

        assert_eq!(services.len(), 1);
        let echo = &services[0];
        assert_eq!(echo.name, "perfscale.test.v1.Echo");
        assert_eq!(echo.methods.len(), 6);

        let unary = echo
            .methods
            .iter()
            .find(|m| m.name == "perfscale.test.v1.Echo/Unary")
            .expect("Unary present");
        assert_eq!(
            unary.input_json,
            json!({ "message": "", "count": 0, "size": 0 })
        );
        assert_eq!(
            unary.output_json,
            json!({ "message": "", "seq": 0, "padding": "" })
        );
        // Streaming shapes keep the same message skeletons.
        let bidi = echo
            .methods
            .iter()
            .find(|m| m.name == "perfscale.test.v1.Echo/Bidi")
            .expect("Bidi present");
        assert_eq!(bidi.input_json, unary.input_json);
    }

    #[tokio::test]
    async fn reflect_schema_connect_failure_returns_err() {
        let endpoint = Endpoint::from_shared("http://127.0.0.1:1".to_string()).expect("valid uri");
        let err = reflect_schema(endpoint)
            .await
            .expect_err("nothing listens on port 1");
        assert!(!err.is_empty());
    }

    // -----------------------------------------------------------------
    // probe_ws — same server patterns as step/ws.rs tests
    // -----------------------------------------------------------------

    /// Echo server that accepts the first offered subprotocol.
    async fn subprotocol_echo_server() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                tokio::spawn(async move {
                    use futures_util::StreamExt as _;
                    use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
                    use tokio_tungstenite::tungstenite::Message;
                    // The callback's Result type (large ErrorResponse Err) is
                    // fixed by tungstenite's accept_hdr_async signature.
                    #[allow(clippy::result_large_err)]
                    let cb = |req: &Request, mut resp: Response| {
                        let offered = req
                            .headers()
                            .get("sec-websocket-protocol")
                            .and_then(|v| v.to_str().ok())
                            .and_then(|s| s.split(',').next())
                            .unwrap_or("")
                            .trim()
                            .to_string();
                        resp.headers_mut()
                            .insert("sec-websocket-protocol", offered.parse().unwrap());
                        Ok(resp)
                    };
                    let Ok(mut ws) = tokio_tungstenite::accept_hdr_async(tcp, cb).await else {
                        return;
                    };
                    while let Some(Ok(m)) = ws.next().await {
                        if matches!(m, Message::Close(_)) {
                            break;
                        }
                    }
                });
            }
        });
        format!("ws://{addr}")
    }

    #[tokio::test]
    async fn probe_ws_ok_reports_latency_and_subprotocol() {
        let url = subprotocol_echo_server().await;
        let result = probe_ws(
            &url,
            vec![("X-Test".into(), "1".into())],
            vec!["graphql-ws".into(), "other".into()],
            false,
            5_000,
        )
        .await;
        assert!(result.ok, "error: {:?}", result.error);
        assert_eq!(result.error, None);
        assert_eq!(result.subprotocol.as_deref(), Some("graphql-ws"));
        assert!(result.latency_ms < 5_000, "{}", result.latency_ms);
    }

    /// A one-shot `wss://` echo server with a fresh self-signed cert.
    async fn tls_echo_server() -> String {
        use futures_util::{SinkExt as _, StreamExt as _};
        use std::sync::Arc;
        use tokio_tungstenite::tungstenite::Message;

        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let key = rustls::pki_types::PrivateKeyDer::Pkcs8(cert.key_pair.serialize_der().into());
        let server_config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert.cert.der().clone()], key)
        .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    // A rejected handshake (the no-skip probe) just ends here.
                    let Ok(tls) = acceptor.accept(tcp).await else {
                        return;
                    };
                    let Ok(mut ws) = tokio_tungstenite::accept_async(tls).await else {
                        return;
                    };
                    while let Some(Ok(msg)) = ws.next().await {
                        match msg {
                            Message::Text(t) => {
                                if ws.send(Message::Text(t)).await.is_err() {
                                    break;
                                }
                            }
                            Message::Close(_) => break,
                            _ => {}
                        }
                    }
                });
            }
        });
        format!("wss://localhost:{}", addr.port())
    }

    #[tokio::test]
    async fn probe_ws_wss_default_rejects_skip_accepts() {
        let url = tls_echo_server().await;

        // Self-signed chain: rejected without the opt-in flag.
        let rejected = probe_ws(&url, vec![], vec![], false, 5_000).await;
        assert!(!rejected.ok);
        assert!(rejected.error.is_some());
        assert_eq!(rejected.subprotocol, None);

        let accepted = probe_ws(&url, vec![], vec![], true, 5_000).await;
        assert!(accepted.ok, "error: {:?}", accepted.error);
        assert_eq!(accepted.error, None);
    }

    #[tokio::test]
    async fn probe_ws_refused_and_invalid_urls_fail_cleanly() {
        let refused = probe_ws("ws://127.0.0.1:1/", vec![], vec![], false, 2_000).await;
        assert!(!refused.ok);
        assert!(refused.error.is_some());

        let invalid = probe_ws("not a url", vec![], vec![], false, 2_000).await;
        assert!(!invalid.ok);
        let err = invalid.error.expect("error is reported, not panicked");
        assert!(err.contains("invalid url"), "got: {err}");
    }

    /// Accepts TCP but never answers the HTTP upgrade — the client-side
    /// timeout must cut the probe, not hang.
    async fn blackhole_server() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let _hold = tcp; // keep the socket open and silent
                    std::future::pending::<()>().await;
                });
            }
        });
        format!("ws://{addr}")
    }

    #[tokio::test]
    async fn probe_ws_timeout_is_enforced() {
        let url = blackhole_server().await;
        let t0 = Instant::now();
        let result = probe_ws(&url, vec![], vec![], false, 200).await;
        let elapsed = t0.elapsed();
        assert!(!result.ok);
        let err = result.error.expect("timeout is reported");
        assert!(err.contains("timeout"), "got: {err}");
        assert!(
            elapsed < Duration::from_secs(5),
            "timeout must cut the probe, took {elapsed:?}"
        );
    }
}
