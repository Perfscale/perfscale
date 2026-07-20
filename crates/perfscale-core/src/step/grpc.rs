//! gRPC actions.
//!
//! | Action ID                  | What it does                                      |
//! |----------------------------|---------------------------------------------------|
//! | `std/grpc@v1`              | One-shot unary call (connect → call → close)      |
//! | `std/grpc-connect@v1`      | Open a channel + load schema, return its id       |
//! | `std/grpc-call@v1`         | Unary call on a Live Channel                      |
//! | `std/grpc-stream-open@v1`  | Start a streaming call on a Live Channel          |
//! | `std/grpc-stream-send@v1`  | Send message(s) on an open stream                 |
//! | `std/grpc-stream-recv@v1`  | Read from an open stream until a stopping rule    |
//! | `std/grpc-stream-close@v1` | Half-close + drain an open stream                 |
//!
//! # Channel Profile vs Live Channel
//!
//! A **Channel Profile** is plain data — `url`, `metadata`, `skipTLSVerify`,
//! schema source — passed as the `connection` parameter (an object, or the
//! JSON string a `${{ config.x }}` interpolation yields). Profile fields can
//! also be given inline; inline fields override the profile.
//!
//! A **Live Channel** is an HTTP/2 connection held across steps within one VU
//! iteration. `std/grpc-connect@v1` opens it, loads the message schema, and
//! returns `{ "id": "grpc-1", ... }`; `grpc-call` and `grpc-stream-open`
//! address it via `id: "${{ conn.id }}"`. Whatever a scenario leaves open is
//! dropped at iteration end. Streams opened on a channel get their own ids
//! (`grpcs-1`, …) from `std/grpc-stream-open@v1`.
//!
//! # Schema
//!
//! Dynamic calls need the protobuf schema at run time. Every connect-capable
//! action takes exactly one source (mutually exclusive):
//!
//! - `descriptor_set` — base64 of a serialized `FileDescriptorSet` (e.g.
//!   `protoc --descriptor_set_out`, or a previous step's
//!   `${{ fetch.body_base64 }}` output).
//! - `reflection: true` — fetch the schema from the server's reflection
//!   service (v1 protocol). The pool is cached per URL for the rest of the
//!   run, so repeated connects to one server pay one reflection round trip.
//!
//! Methods are named `"package.Service/Method"`; a typo fails with a
//! did-you-mean suggestion listing the closest known method.
//!
//! # Payload
//!
//! Requests are built from `payload` (JSON, decoded per protobuf-JSON rules
//! into a dynamic message — field names accept both the proto name and its
//! camelCase `json_name`) or `payload_base64` (base64 of the serialized
//! protobuf bytes). String leaves of `payload` may embed single-brace `${…}`
//! tokens ([`crate::generate`]), expanded per call/send.
//!
//! Responses are serialized back to JSON (`body`) with the same rules.
//!
//! # Status assertions
//!
//! `expect_status` (default `0`) names the expected gRPC status code. A call
//! whose status differs fails the step — including status `0` when another
//! status was expected, so error-path tests read naturally:
//! `expect_status: 5` passes when the server returns NOT_FOUND.
//!
//! # Metrics
//!
//! Unary calls emit `grpc_req_duration` (histogram), `grpc_msgs_sent` /
//! `grpc_msgs_received` (counters), `grpc_msg_rtt` (histogram) and
//! `grpc_req_failed` (counter) via the reserved `metrics` output key. Streams
//! emit `grpc_msgs_sent` / `grpc_msgs_received` per step and `grpc_msg_rtt`
//! when a recv's until-rule matches after a send. Stream lifetimes span user
//! steps, so they deliberately do not feed `grpc_req_duration`.

use std::sync::Arc;
use std::time::Instant;

use base64::Engine as _;
use prost_reflect::{DescriptorPool, DynamicMessage, MessageDescriptor, MethodDescriptor};
use serde_json::{json, Value};
use tokio::time::Duration;
use tonic::client::Grpc;
use tonic::codec::{Codec, Decoder, Encoder};
use tonic::metadata::{MetadataKey, MetadataMap, MetadataValue};
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};
use tonic::{Code, Request, Status};

use super::actions::{err, error_chain, ActionOutput, LogTag};
use super::context::Context;
use super::resources::{GrpcConn, GrpcStream};
use super::ws::{bool_param, json_subset_match, u64_param, Until};
use crate::generate::Gen;
use crate::lint::edit_distance;

/// Inbound message cap when `max_recv_size` is not set (tonic's own default
/// is 4 MiB — too tight for payload-heavy APIs).
const DEFAULT_MAX_RECV_SIZE: usize = 16 * 1024 * 1024;

/// Local slack added on top of the `grpc-timeout` header value, so a server
/// honouring the deadline wins the race and the step sees DEADLINE_EXCEEDED
/// (status 4) instead of an unlabelled local cut.
const LOCAL_TIMEOUT_SLACK_MS: u64 = 250;

// ---------------------------------------------------------------------------
// Channel Profile
// ---------------------------------------------------------------------------

/// Resolved connection parameters — the profile merged with inline fields.
#[derive(Debug)]
struct Profile {
    /// Target as given (`grpc://` / `grpcs://` / bare host), for log lines.
    url: String,
    /// `http(s)://` form tonic's `Endpoint` understands.
    endpoint_uri: String,
    /// True for `grpcs://` (the default scheme).
    tls: bool,
    skip_tls_verify: bool,
    metadata: Vec<(String, String)>,
    /// Base64 FileDescriptorSet.
    descriptor_set: Option<String>,
    reflection: bool,
    max_recv_size: usize,
}

/// Merge the `connection` profile (object or JSON string) with inline
/// parameters; inline wins. `url` is required; the scheme defaults to
/// `grpcs://`. Exactly one schema source (`descriptor_set` or `reflection`)
/// is required.
fn resolve_profile(params: &Value) -> Result<Profile, String> {
    // The profile arrives as an object, or as the JSON string that
    // `connection: "${{ config.x }}"` interpolation yields.
    let profile: Value = match params.get("connection") {
        Some(Value::Object(m)) => Value::Object(m.clone()),
        Some(Value::String(s)) => {
            serde_json::from_str(s).map_err(|_| "'connection' string is not valid JSON")?
        }
        Some(_) => return Err("'connection' must be an object or a JSON string".into()),
        None => Value::Object(Default::default()),
    };

    let field = |key: &str| -> Option<Value> {
        params
            .get(key)
            .filter(|v| !v.is_null())
            .or_else(|| profile.get(key).filter(|v| !v.is_null()))
            .cloned()
    };

    let raw_url = field("url")
        .as_ref()
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or("'url' is required (inline or via 'connection')")?;
    let (endpoint_uri, tls) = normalize_url(&raw_url)?;

    let mut metadata = Vec::new();
    if let Some(Value::Object(m)) = field("metadata") {
        for (k, v) in m {
            let v = match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            validate_metadata_entry(&k, &v)?;
            metadata.push((k, v));
        }
    }

    let descriptor_set = match field("descriptor_set") {
        Some(v) => Some(
            v.as_str()
                .ok_or("'descriptor_set' must be a base64 string")?
                .to_owned(),
        ),
        None => None,
    };
    let reflection = field("reflection").is_some_and(|v| bool_param(&v));
    match (&descriptor_set, reflection) {
        (Some(_), true) => {
            return Err("'descriptor_set' and 'reflection' are mutually exclusive".into())
        }
        (None, false) => {
            return Err(
                "schema required: pass 'descriptor_set' (base64 FileDescriptorSet) \
                 or 'reflection': true"
                    .into(),
            )
        }
        _ => {}
    }

    Ok(Profile {
        url: raw_url,
        endpoint_uri,
        tls,
        skip_tls_verify: field("skipTLSVerify").is_some_and(|v| bool_param(&v)),
        metadata,
        descriptor_set,
        reflection,
        max_recv_size: field("max_recv_size")
            .map(|v| u64_param(&v, DEFAULT_MAX_RECV_SIZE as u64))
            .unwrap_or(DEFAULT_MAX_RECV_SIZE as u64) as usize,
    })
}

/// `grpc://` → plaintext, `grpcs://` → TLS (default when no scheme is given).
/// tonic's `Endpoint` only understands `http`/`https`.
fn normalize_url(raw: &str) -> Result<(String, bool), String> {
    if let Some(rest) = raw.strip_prefix("grpc://") {
        Ok((format!("http://{rest}"), false))
    } else if let Some(rest) = raw.strip_prefix("grpcs://") {
        Ok((format!("https://{rest}"), true))
    } else if raw.contains("://") {
        Err(format!(
            "'url' scheme must be grpc:// or grpcs://, got '{raw}'"
        ))
    } else {
        Ok((format!("https://{raw}"), true))
    }
}

/// gRPC metadata keys must be lowercase ASCII; `-bin` keys carry raw bytes
/// and are not supported (values here are strings).
fn validate_metadata_entry(key: &str, value: &str) -> Result<(), String> {
    if key.ends_with("-bin") {
        return Err(format!(
            "metadata key '{key}': binary ('-bin') metadata is not supported"
        ));
    }
    MetadataKey::from_bytes(key.to_ascii_lowercase().as_bytes())
        .map(|_: MetadataKey<tonic::metadata::Ascii>| ())
        .map_err(|_| format!("invalid metadata key '{key}'"))?;
    MetadataValue::try_from(value).map_err(|_| format!("invalid metadata value for '{key}'"))?;
    Ok(())
}

/// Build the per-call metadata map: channel defaults first, per-call entries
/// override them on key conflict.
fn build_metadata(base: &[(String, String)], params: &Value) -> Result<MetadataMap, String> {
    let mut map = MetadataMap::new();
    let mut insert = |k: &str, v: &str| -> Result<(), String> {
        validate_metadata_entry(k, v)?;
        let key: MetadataKey<tonic::metadata::Ascii> =
            MetadataKey::from_bytes(k.to_ascii_lowercase().as_bytes())
                .map_err(|_| format!("invalid metadata key '{k}'"))?;
        let value =
            MetadataValue::try_from(v).map_err(|_| format!("invalid metadata value for '{k}'"))?;
        map.insert(key, value);
        Ok(())
    };
    for (k, v) in base {
        insert(k, v)?;
    }
    if let Some(Value::Object(m)) = params.get("metadata") {
        for (k, v) in m {
            let v = match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            insert(k, &v)?;
        }
    }
    Ok(map)
}

// ---------------------------------------------------------------------------
// Channel + TLS
// ---------------------------------------------------------------------------

/// Open the HTTP/2 channel. TLS (grpcs) verifies against the webpki roots
/// unless `skipTLSVerify` swaps in an accept-any verifier.
async fn connect_channel(profile: &Profile) -> Result<Channel, String> {
    let endpoint = Endpoint::from_shared(profile.endpoint_uri.clone())
        .map_err(|e| format!("invalid url: {e}"))?;
    let endpoint = if profile.tls {
        let tls = ClientTlsConfig::new();
        if profile.skip_tls_verify {
            // tonic's `ClientTlsConfig` gained a custom-verifier hook in
            // 0.14.6 (`tls_config_with_verifier`), so no hand-rolled hyper
            // connector is needed.
            endpoint
                .tls_config_with_verifier(tls, no_verify_verifier())
                .map_err(|e| error_chain(&e))?
        } else {
            endpoint
                .tls_config(tls.with_webpki_roots())
                .map_err(|e| error_chain(&e))?
        }
    } else {
        endpoint
    };
    endpoint.connect().await.map_err(|e| error_chain(&e))
}

/// A rustls `ServerCertVerifier` that accepts any server certificate — the
/// same model as ws.rs's `no_verify_tls_config`. Signature checks still run
/// (the handshake stays well-formed); the chain and hostname are not
/// validated. Opt-in via `skipTLSVerify: true` only.
fn no_verify_verifier() -> Arc<dyn rustls::client::danger::ServerCertVerifier> {
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::crypto::{ring, verify_tls12_signature, verify_tls13_signature, CryptoProvider};
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use rustls::{DigitallySignedStruct, SignatureScheme};

    #[derive(Debug)]
    struct NoVerify(Arc<CryptoProvider>);

    impl ServerCertVerifier for NoVerify {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp_response: &[u8],
            _now: UnixTime,
        ) -> Result<ServerCertVerified, rustls::Error> {
            Ok(ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            verify_tls12_signature(
                message,
                cert,
                dss,
                &self.0.signature_verification_algorithms,
            )
        }

        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            verify_tls13_signature(
                message,
                cert,
                dss,
                &self.0.signature_verification_algorithms,
            )
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            self.0.signature_verification_algorithms.supported_schemes()
        }
    }

    let provider = Arc::new(ring::default_provider());
    Arc::new(NoVerify(provider))
}

// ---------------------------------------------------------------------------
// Dynamic codec — DynamicMessage over the wire without codegen
// ---------------------------------------------------------------------------

/// A tonic [`Codec`] that encodes/decodes [`DynamicMessage`]s against the
/// method's descriptors. (This is why `prost_reflect::GrpcClient` is not
/// used: a plain codec over `tonic::client::Grpc` keeps timeout, metadata,
/// and max-message control explicit, with no extra API surface.)
struct DynamicCodec {
    output: MessageDescriptor,
}

impl Codec for DynamicCodec {
    type Encode = DynamicMessage;
    type Decode = DynamicMessage;
    type Encoder = DynamicEncoder;
    type Decoder = DynamicDecoder;

    fn encoder(&mut self) -> Self::Encoder {
        DynamicEncoder
    }

    fn decoder(&mut self) -> Self::Decoder {
        DynamicDecoder {
            desc: self.output.clone(),
        }
    }
}

struct DynamicEncoder;

impl Encoder for DynamicEncoder {
    type Item = DynamicMessage;
    type Error = Status;

    fn encode(
        &mut self,
        item: Self::Item,
        dst: &mut tonic::codec::EncodeBuf<'_>,
    ) -> Result<(), Status> {
        use prost::Message as _;
        item.encode(dst)
            .map_err(|e| Status::internal(format!("encode request: {e}")))
    }
}

struct DynamicDecoder {
    desc: MessageDescriptor,
}

impl Decoder for DynamicDecoder {
    type Item = DynamicMessage;
    type Error = Status;

    fn decode(
        &mut self,
        src: &mut tonic::codec::DecodeBuf<'_>,
    ) -> Result<Option<DynamicMessage>, Status> {
        DynamicMessage::decode(self.desc.clone(), src)
            .map(Some)
            .map_err(|e| Status::internal(format!("decode response: {e}")))
    }
}

/// Client wrapper with the channel's inbound message cap applied (tonic's
/// own default is 4 MiB).
fn grpc_client(channel: &Channel, max_recv_size: usize) -> Grpc<Channel> {
    Grpc::new(channel.clone()).max_decoding_message_size(max_recv_size)
}

/// `"/package.Service/Method"` — the HTTP/2 path every gRPC call uses.
fn method_path(method: &MethodDescriptor) -> Result<http::uri::PathAndQuery, String> {
    format!("/{}/{}", method.parent_service().full_name(), method.name())
        .parse()
        .map_err(|_| {
            format!(
                "method name '{}' is not a valid gRPC path",
                method.full_name()
            )
        })
}

// ---------------------------------------------------------------------------
// Schema: descriptor_set / reflection, method resolution
// ---------------------------------------------------------------------------

/// Load the message schema per the profile's source. Reflection pools are
/// cached per URL in the VU's resources (when a context is available), so
/// repeated connects to one server pay one reflection round trip per run.
///
/// `descriptor_set` is decoded by [`pool_from_descriptor_set`] *before*
/// connecting (fail fast, no network); only the reflection path remains here.
async fn load_pool(
    profile: &Profile,
    channel: &Channel,
    ctx: Option<&Context>,
) -> Result<DescriptorPool, String> {
    debug_assert!(profile.reflection, "descriptor_set is handled pre-connect");
    if let Some(ctx) = ctx {
        if let Some(pool) = ctx.resources.reflection_pool(&profile.url) {
            return Ok(pool);
        }
    }
    let pool = fetch_reflection_pool(channel).await?;
    if let Some(ctx) = ctx {
        ctx.resources
            .cache_reflection_pool(&profile.url, pool.clone());
    }
    Ok(pool)
}

/// Decode the profile's `descriptor_set` when that is the configured schema
/// source (`None` when reflection is used instead).
fn pool_from_descriptor_set(profile: &Profile) -> Option<Result<DescriptorPool, String>> {
    profile.descriptor_set.as_ref().map(|b64| {
        base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|e| format!("invalid base64 in 'descriptor_set': {e}"))
            .and_then(|bytes| {
                DescriptorPool::decode(&bytes[..])
                    .map_err(|e| format!("invalid 'descriptor_set': {e}"))
            })
    })
}

/// Fetch the server's schema via the v1 reflection protocol and assemble a
/// pool. `FileDescriptorResponse` carries the transitive closure of each
/// requested file, so one round trip per service suffices; files are
/// deduplicated by name.
async fn fetch_reflection_pool(channel: &Channel) -> Result<DescriptorPool, String> {
    use prost::Message as _;
    use tonic_reflection::pb::v1::server_reflection_client::ServerReflectionClient;
    use tonic_reflection::pb::v1::server_reflection_request::MessageRequest;
    use tonic_reflection::pb::v1::server_reflection_response::MessageResponse;
    use tonic_reflection::pb::v1::ServerReflectionRequest;

    let mut client = ServerReflectionClient::new(channel.clone());
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    let request_stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    let mut responses = client
        .server_reflection_info(request_stream)
        .await
        .map_err(|s| format!("reflection: {}", status_line(&s)))?
        .into_inner();

    let ask = |req: MessageRequest| {
        let tx = tx.clone();
        async move {
            tx.send(ServerReflectionRequest {
                host: String::new(),
                message_request: Some(req),
            })
            .await
            .map_err(|_| "reflection: server closed the stream".to_string())
        }
    };

    ask(MessageRequest::ListServices(String::new())).await?;
    let services = match responses.message().await {
        Ok(Some(resp)) => match resp.message_response {
            Some(MessageResponse::ListServicesResponse(list)) => list
                .service
                .into_iter()
                .map(|s| s.name)
                // The reflection service itself is of no interest to callers.
                .filter(|n| !n.starts_with("grpc.reflection."))
                .collect::<Vec<_>>(),
            _ => return Err("reflection: unexpected response to ListServices".into()),
        },
        Ok(None) => return Err("reflection: stream closed before ListServices reply".into()),
        Err(s) => return Err(format!("reflection: {}", status_line(&s))),
    };
    if services.is_empty() {
        return Err("reflection: server exposes no services".into());
    }

    let mut protos: std::collections::BTreeMap<String, prost_types::FileDescriptorProto> =
        Default::default();
    for service in &services {
        ask(MessageRequest::FileContainingSymbol(service.clone())).await?;
        match responses.message().await {
            Ok(Some(resp)) => match resp.message_response {
                Some(MessageResponse::FileDescriptorResponse(fds)) => {
                    for bytes in fds.file_descriptor_proto {
                        let proto = prost_types::FileDescriptorProto::decode(&bytes[..])
                            .map_err(|e| format!("reflection: bad FileDescriptorProto: {e}"))?;
                        protos.entry(proto.name().to_string()).or_insert(proto);
                    }
                }
                Some(MessageResponse::ErrorResponse(e)) => {
                    return Err(format!(
                        "reflection: {service}: {}: {}",
                        status_name(Code::from(e.error_code)),
                        e.error_message
                    ))
                }
                _ => return Err(format!("reflection: unexpected response for '{service}'")),
            },
            Ok(None) => {
                return Err(format!(
                    "reflection: stream closed before '{service}' was resolved"
                ))
            }
            Err(s) => return Err(format!("reflection: {}", status_line(&s))),
        }
    }

    DescriptorPool::from_file_descriptor_set(prost_types::FileDescriptorSet {
        file: protos.into_values().collect(),
    })
    .map_err(|e| format!("reflection: cannot assemble schema: {e}"))
}

/// `"package.Service/Method"` → MethodDescriptor. Unknown names fail with a
/// did-you-mean suggestion when a known method is within edit distance 2.
fn resolve_method(pool: &DescriptorPool, name: &str) -> Result<MethodDescriptor, String> {
    let Some((service_name, method_name)) = name.split_once('/') else {
        return Err(format!(
            "'method' must be \"package.Service/Method\", got '{name}'"
        ));
    };
    if let Some(service) = pool.get_service_by_name(service_name) {
        if let Some(m) = service.methods().find(|m| m.name() == method_name) {
            return Ok(m);
        }
    }

    let candidates: Vec<String> = pool
        .services()
        .flat_map(|s| {
            let full = s.full_name().to_string();
            s.methods()
                .map(move |m| format!("{full}/{}", m.name()))
                .collect::<Vec<_>>()
        })
        .collect();
    let suggestion = candidates
        .iter()
        .map(|c| (c, edit_distance(name, c)))
        .filter(|(_, d)| *d <= 2)
        .min_by_key(|(_, d)| *d);
    match suggestion {
        Some((c, _)) => Err(format!("unknown method '{name}' — did you mean '{c}'?")),
        None => Err(format!(
            "unknown method '{name}' — schema has: {}",
            candidates.join(", ")
        )),
    }
}

/// The `"package.Service/Method"` display form used in log lines.
fn method_display(method: &MethodDescriptor) -> String {
    format!("{}/{}", method.parent_service().full_name(), method.name())
}

/// Streaming shape of a method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamKind {
    Unary,
    Server,
    Client,
    Bidi,
}

impl StreamKind {
    fn of(method: &MethodDescriptor) -> StreamKind {
        match (method.is_client_streaming(), method.is_server_streaming()) {
            (false, false) => StreamKind::Unary,
            (false, true) => StreamKind::Server,
            (true, false) => StreamKind::Client,
            (true, true) => StreamKind::Bidi,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            StreamKind::Unary => "unary",
            StreamKind::Server => "server",
            StreamKind::Client => "client",
            StreamKind::Bidi => "bidi",
        }
    }
}

// ---------------------------------------------------------------------------
// Payload
// ---------------------------------------------------------------------------

/// Build the request message: `payload` (JSON, with `${…}` expansion in
/// string leaves) or `payload_base64` (serialized protobuf bytes).
fn build_message(
    desc: &MessageDescriptor,
    params: &Value,
    generator: &mut Gen,
) -> Result<DynamicMessage, String> {
    match (params.get("payload"), params.get("payload_base64")) {
        (Some(_), Some(_)) => Err("'payload' and 'payload_base64' are mutually exclusive".into()),
        (Some(v), None) => {
            let expanded = expand_tokens(v, generator);
            DynamicMessage::deserialize(desc.clone(), expanded)
                .map_err(|e| format!("'payload' does not match {}: {e}", desc.full_name()))
        }
        (None, Some(v)) => {
            let s = v.as_str().ok_or("'payload_base64' must be a string")?;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(s)
                .map_err(|e| format!("invalid base64 in 'payload_base64': {e}"))?;
            DynamicMessage::decode(desc.clone(), &bytes[..]).map_err(|e| {
                format!(
                    "'payload_base64' does not decode as {}: {e}",
                    desc.full_name()
                )
            })
        }
        (None, None) => Err("'payload' (or 'payload_base64') is required".into()),
    }
}

/// Expand `${…}` tokens in every string leaf (keys are never expanded).
fn expand_tokens(v: &Value, generator: &mut Gen) -> Value {
    match v {
        Value::String(s) => Value::String(generator.expand(s)),
        Value::Array(a) => Value::Array(a.iter().map(|x| expand_tokens(x, generator)).collect()),
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, x)| (k.clone(), expand_tokens(x, generator)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// `expect_status` — the gRPC status code the step asserts (default `0`).
fn expect_status(params: &Value) -> i32 {
    params
        .get("expect_status")
        .map(|v| u64_param(v, 0))
        .unwrap_or(0) as i32
}

/// Canonical gRPC status name for log lines and errors.
fn status_name(code: Code) -> &'static str {
    match code {
        Code::Ok => "OK",
        Code::Cancelled => "CANCELLED",
        Code::Unknown => "UNKNOWN",
        Code::InvalidArgument => "INVALID_ARGUMENT",
        Code::DeadlineExceeded => "DEADLINE_EXCEEDED",
        Code::NotFound => "NOT_FOUND",
        Code::AlreadyExists => "ALREADY_EXISTS",
        Code::PermissionDenied => "PERMISSION_DENIED",
        Code::ResourceExhausted => "RESOURCE_EXHAUSTED",
        Code::FailedPrecondition => "FAILED_PRECONDITION",
        Code::Aborted => "ABORTED",
        Code::OutOfRange => "OUT_OF_RANGE",
        Code::Unimplemented => "UNIMPLEMENTED",
        Code::Internal => "INTERNAL",
        Code::Unavailable => "UNAVAILABLE",
        Code::DataLoss => "DATA_LOSS",
        Code::Unauthenticated => "UNAUTHENTICATED",
    }
}

/// `"UNAVAILABLE: connection refused"` — one line for error messages.
fn status_line(status: &Status) -> String {
    if status.message().is_empty() {
        status_name(status.code()).to_string()
    } else {
        format!("{}: {}", status_name(status.code()), status.message())
    }
}

// ---------------------------------------------------------------------------
// Unary plumbing shared by std/grpc@v1 and std/grpc-call@v1
// ---------------------------------------------------------------------------

struct CallOutcome {
    status: Code,
    /// Response message as JSON (present only on status OK).
    body: Option<Value>,
    /// Status/transport error detail.
    error: Option<String>,
    duration_ms: f64,
}

/// One unary RPC: build the request, set `grpc-timeout`, enforce the same
/// deadline locally (plus slack, so a server-side DEADLINE_EXCEEDED arrives
/// first), decode the response.
async fn unary_call(
    channel: &Channel,
    max_recv_size: usize,
    method: &MethodDescriptor,
    message: DynamicMessage,
    metadata: MetadataMap,
    timeout: Duration,
) -> CallOutcome {
    let t0 = Instant::now();
    let path = match method_path(method) {
        Ok(p) => p,
        Err(e) => {
            return CallOutcome {
                status: Code::Internal,
                body: None,
                error: Some(e),
                duration_ms: 0.0,
            }
        }
    };
    let codec = DynamicCodec {
        output: method.output(),
    };

    let work = {
        let channel = channel.clone();
        async move {
            let mut client = grpc_client(&channel, max_recv_size);
            client
                .ready()
                .await
                .map_err(|e| Status::unavailable(error_chain(&e)))?;
            let mut request = Request::new(message);
            *request.metadata_mut() = metadata;
            request.set_timeout(timeout);
            client.unary(request, path, codec).await
        }
    };
    let result = tokio::time::timeout(
        timeout + Duration::from_millis(LOCAL_TIMEOUT_SLACK_MS),
        work,
    )
    .await;
    let duration_ms = t0.elapsed().as_secs_f64() * 1000.0;

    match result {
        Err(_) => CallOutcome {
            status: Code::DeadlineExceeded,
            body: None,
            error: Some(format!("timeout after {duration_ms:.2}ms")),
            duration_ms,
        },
        Ok(Ok(response)) => {
            let msg = response.into_inner();
            CallOutcome {
                status: Code::Ok,
                body: Some(serde_json::to_value(&msg).unwrap_or(Value::Null)),
                error: None,
                duration_ms,
            }
        }
        Ok(Err(status)) => CallOutcome {
            status: status.code(),
            body: None,
            error: Some(status.message().to_string()),
            duration_ms,
        },
    }
}

/// Fold a [`CallOutcome`] into the step output: status assertion against
/// `expect`, the reserved `metrics` key, and ws-style log lines.
fn unary_output(
    step_name: &str,
    url: &str,
    method: &MethodDescriptor,
    outcome: CallOutcome,
    expect: i32,
) -> ActionOutput {
    let display = method_display(method);
    let code = outcome.status as i32;
    let ok = code == expect;
    let received = u64::from(outcome.body.is_some());

    let mut metrics = json!({
        "grpc_req_duration": [outcome.duration_ms],
        "grpc_msgs_sent": 1,
        "grpc_msgs_received": received,
        "grpc_req_failed": if ok { 0 } else { 1 },
    });
    if outcome.status == Code::Ok {
        // Unary RTT == request duration; the array form marks a histogram.
        metrics["grpc_msg_rtt"] = json!([outcome.duration_ms]);
    }

    let mut value = json!({
        "status": code,
        "duration_ms": outcome.duration_ms,
        "metrics": metrics,
    });
    if let Some(body) = outcome.body {
        value["body"] = body;
    }
    let mut logs = vec![(
        if ok { LogTag::Out } else { LogTag::Err },
        format!(
            "gRPC {url} {display} → {} ({:.2}ms)",
            status_name(outcome.status),
            outcome.duration_ms
        ),
    )];
    if let Some(e) = &outcome.error {
        value["error"] = json!(e);
        logs.push((
            LogTag::Err,
            format!(
                "{step_name}: gRPC {display}: {}: {e}",
                status_name(outcome.status)
            ),
        ));
    } else if !ok {
        logs.push((
            LogTag::Err,
            format!(
                "{step_name}: gRPC {display}: expected status {expect}, got {code} ({})",
                status_name(outcome.status)
            ),
        ));
    }

    ActionOutput {
        value,
        logs,
        success: ok,
        http_sample: None,
    }
}

/// Look up the Live Channel for `params.id`, or explain what went wrong.
fn take_conn(params: &Value, ctx: &Context) -> Result<(String, GrpcConn), String> {
    let id = params
        .get("id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or("'id' is required (the output of std/grpc-connect@v1)")?;
    let conn = ctx.resources.take_grpc(id).ok_or(format!(
        "unknown connection id '{id}' — not connected in this iteration, already closed, \
         or opened in `before:` setup (connections do not cross into VU iterations)"
    ))?;
    Ok((id.to_string(), conn))
}

/// Look up the open stream for `params.id`, or explain what went wrong.
fn take_stream(params: &Value, ctx: &Context) -> Result<(String, GrpcStream), String> {
    let id = params
        .get("id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or("'id' is required (the output of std/grpc-stream-open@v1)")?;
    let stream = ctx.resources.take_grpc_stream(id).ok_or(format!(
        "unknown stream id '{id}' — not opened in this iteration, already closed, \
         or opened in `before:` setup (streams do not cross into VU iterations)"
    ))?;
    Ok((id.to_string(), stream))
}

// ---------------------------------------------------------------------------
// std/grpc-connect@v1
// ---------------------------------------------------------------------------
//
// Parameters:
//   connection     – Channel Profile (object, or `${{ config.x }}` JSON string)
//   url            – grpc:// or grpcs:// target (inline; overrides the profile)
//   metadata       – map of default call metadata (auth tokens etc.)
//   skipTLSVerify  – accept any server certificate (self-signed staging only)
//   descriptor_set – base64 FileDescriptorSet (mutually exclusive with reflection)
//   reflection     – true: load the schema via the server reflection service
//   max_recv_size  – inbound message cap in bytes, default 16 MiB
//   timeout        – ms for connect + schema load, default 10000
//
// Output:
//   { "id": "grpc-1", "connected": true, "duration_ms": <f64> }
//
// The channel and its schema live in the VU's resource registry until closed
// implicitly at iteration end.

pub(crate) async fn grpc_connect_action(
    params: &Value,
    ctx: &Context,
    step_name: &str,
) -> ActionOutput {
    let profile = match resolve_profile(params) {
        Ok(p) => p,
        Err(msg) => return err(step_name, &msg),
    };
    let timeout_ms = params
        .get("timeout")
        .map(|v| u64_param(v, 10_000))
        .unwrap_or(10_000);

    let t0 = Instant::now();
    // Fail fast on a bad descriptor_set before touching the network.
    let early_pool = match pool_from_descriptor_set(&profile) {
        Some(Ok(p)) => Some(p),
        Some(Err(msg)) => {
            return connect_err(
                step_name,
                &profile.url,
                &msg,
                t0.elapsed().as_secs_f64() * 1000.0,
            )
        }
        None => None,
    };
    let setup = tokio::time::timeout(Duration::from_millis(timeout_ms), async {
        let channel = connect_channel(&profile).await?;
        let pool = match early_pool {
            Some(p) => p,
            None => load_pool(&profile, &channel, Some(ctx)).await?,
        };
        Ok::<_, String>((channel, pool))
    })
    .await;
    let duration_ms = t0.elapsed().as_secs_f64() * 1000.0;

    let (channel, pool) = match setup {
        Ok(Ok(pair)) => pair,
        Ok(Err(msg)) => return connect_err(step_name, &profile.url, &msg, duration_ms),
        Err(_) => {
            return connect_err(
                step_name,
                &profile.url,
                &format!("connect/schema TIMEOUT after {duration_ms:.2}ms"),
                duration_ms,
            )
        }
    };

    let services = pool.services().len();
    let url = profile.url;
    let id = ctx.resources.insert_grpc(GrpcConn {
        channel,
        url: url.clone(),
        pool,
        generator: Gen::new(uuid::Uuid::new_v4().as_u128() as u64),
        metadata: profile.metadata,
        max_recv_size: profile.max_recv_size,
    });

    ActionOutput {
        value: json!({
            "id": id,
            "connected": true,
            "duration_ms": duration_ms,
        }),
        logs: vec![(
            LogTag::Out,
            format!("gRPC connect {url} → {id} ({services} services) ({duration_ms:.2}ms)"),
        )],
        success: true,
        http_sample: None,
    }
}

/// Connect failure output — no RPC was made, so no gRPC metrics.
fn connect_err(step_name: &str, url: &str, detail: &str, duration_ms: f64) -> ActionOutput {
    ActionOutput {
        value: json!({ "connected": false, "error": detail, "duration_ms": duration_ms }),
        logs: vec![(LogTag::Err, format!("{step_name}: gRPC {url}: {detail}"))],
        success: false,
        http_sample: None,
    }
}

// ---------------------------------------------------------------------------
// std/grpc-call@v1
// ---------------------------------------------------------------------------
//
// Parameters:
//   id             – Connection ID from std/grpc-connect@v1 (required)
//   method         – "package.Service/Method" (required; unary methods only)
//   payload        – JSON request message; `${…}` tokens expand per call
//   payload_base64 – serialized protobuf bytes (mutually exclusive)
//   metadata       – per-call metadata (overrides channel defaults per key)
//   expect_status  – expected gRPC status code, default 0
//   timeout        – ms (→ grpc-timeout), default 10000
//
// Output:
//   { "status": <i32>, "body": <object>?, "error": <string>?,
//     "duration_ms": <f64>,
//     "metrics": { "grpc_req_duration": [<f64>], "grpc_msgs_sent": 1,
//                  "grpc_msgs_received": <u64>, "grpc_msg_rtt": [<f64>]?,
//                  "grpc_req_failed": 0|1 } }
//
// A failed RPC fails the step but leaves the channel parked — HTTP/2
// channels recover (tonic reconnects), unlike a dead WebSocket.

pub(crate) async fn grpc_call_action(
    params: &Value,
    ctx: &Context,
    step_name: &str,
) -> ActionOutput {
    let (id, mut conn) = match take_conn(params, ctx) {
        Ok(x) => x,
        Err(msg) => return err(step_name, &msg),
    };
    let call = prepare_call(params, &mut conn);
    let (method, message, metadata) = match call {
        Ok(x) => x,
        Err(msg) => {
            ctx.resources.put_back_grpc(&id, conn);
            return err(step_name, &msg);
        }
    };
    let timeout_ms = params
        .get("timeout")
        .map(|v| u64_param(v, 10_000))
        .unwrap_or(10_000);
    let expect = expect_status(params);

    let outcome = unary_call(
        &conn.channel,
        conn.max_recv_size,
        &method,
        message,
        metadata,
        Duration::from_millis(timeout_ms),
    )
    .await;

    let url = conn.url.clone();
    ctx.resources.put_back_grpc(&id, conn);
    unary_output(step_name, &url, &method, outcome, expect)
}

/// Resolve + validate the method (unary only) and build the request message
/// and metadata for a unary call on `conn`.
fn prepare_call(
    params: &Value,
    conn: &mut GrpcConn,
) -> Result<(MethodDescriptor, DynamicMessage, MetadataMap), String> {
    let method_name = params
        .get("method")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or("'method' is required (\"package.Service/Method\")")?;
    let method = resolve_method(&conn.pool, method_name)?;
    if StreamKind::of(&method) != StreamKind::Unary {
        return Err(format!(
            "'{method_name}' is a streaming method — use std/grpc-stream-open@v1"
        ));
    }
    conn.generator.begin_message();
    let message = build_message(&method.input(), params, &mut conn.generator)?;
    let metadata = build_metadata(&conn.metadata, params)?;
    Ok((method, message, metadata))
}

// ---------------------------------------------------------------------------
// std/grpc@v1 — one-shot unary call
// ---------------------------------------------------------------------------
//
// Profile and payload parameters are the union of std/grpc-connect@v1 and
// std/grpc-call@v1; `timeout` (default 10000) covers connect + schema load +
// the call (the call's grpc-timeout is the remaining budget).
//
// Output: same shape as std/grpc-call@v1.

pub(crate) async fn grpc_unary_action(params: &Value, step_name: &str) -> ActionOutput {
    let profile = match resolve_profile(params) {
        Ok(p) => p,
        Err(msg) => return err(step_name, &msg),
    };
    let timeout_ms = params
        .get("timeout")
        .map(|v| u64_param(v, 10_000))
        .unwrap_or(10_000);
    let expect = expect_status(params);

    let t0 = Instant::now();
    let deadline = t0 + Duration::from_millis(timeout_ms);

    // Fail fast on a bad descriptor_set before touching the network.
    let early_pool = match pool_from_descriptor_set(&profile) {
        Some(Ok(p)) => Some(p),
        Some(Err(msg)) => {
            return connect_err(
                step_name,
                &profile.url,
                &msg,
                t0.elapsed().as_secs_f64() * 1000.0,
            )
        }
        None => None,
    };
    let remaining = deadline.saturating_duration_since(Instant::now());
    let setup = tokio::time::timeout(remaining, async {
        let channel = connect_channel(&profile).await?;
        let pool = match early_pool {
            Some(p) => p,
            None => load_pool(&profile, &channel, None).await?,
        };
        Ok::<_, String>((channel, pool))
    })
    .await;
    let (channel, pool) = match setup {
        Ok(Ok(pair)) => pair,
        Ok(Err(msg)) => {
            return connect_err(
                step_name,
                &profile.url,
                &msg,
                t0.elapsed().as_secs_f64() * 1000.0,
            )
        }
        Err(_) => {
            return connect_err(
                step_name,
                &profile.url,
                &format!(
                    "connect/schema TIMEOUT after {:.2}ms",
                    t0.elapsed().as_secs_f64() * 1000.0
                ),
                t0.elapsed().as_secs_f64() * 1000.0,
            )
        }
    };

    let method_name = match params.get("method").and_then(Value::as_str) {
        Some(m) if !m.is_empty() => m,
        _ => {
            return err(
                step_name,
                "'method' is required (\"package.Service/Method\")",
            )
        }
    };
    let method = match resolve_method(&pool, method_name) {
        Ok(m) => m,
        Err(msg) => return err(step_name, &msg),
    };
    if StreamKind::of(&method) != StreamKind::Unary {
        return err(
            step_name,
            &format!("'{method_name}' is a streaming method — use std/grpc-stream-open@v1"),
        );
    }

    let mut generator = Gen::new(uuid::Uuid::new_v4().as_u128() as u64);
    generator.begin_message();
    let message = match build_message(&method.input(), params, &mut generator) {
        Ok(m) => m,
        Err(msg) => return err(step_name, &msg),
    };
    let metadata = match build_metadata(&profile.metadata, params) {
        Ok(m) => m,
        Err(msg) => return err(step_name, &msg),
    };

    let remaining = deadline.saturating_duration_since(Instant::now());
    let outcome = unary_call(
        &channel,
        profile.max_recv_size,
        &method,
        message,
        metadata,
        remaining.max(Duration::from_millis(1)),
    )
    .await;
    unary_output(step_name, &profile.url, &method, outcome, expect)
}

// ---------------------------------------------------------------------------
// std/grpc-stream-open@v1
// ---------------------------------------------------------------------------
//
// Parameters:
//   id             – Connection ID from std/grpc-connect@v1 (required)
//   method         – "package.Service/Method" (required; streaming methods)
//   payload        – single request message — required for server-streaming,
//                    rejected for client-streaming/bidi (use grpc-stream-send)
//   payload_base64 – serialized protobuf bytes (mutually exclusive)
//   metadata       – per-call metadata (overrides channel defaults per key)
//
// Output:
//   { "id": "grpcs-1", "kind": "server"|"client"|"bidi", "open": true,
//     "duration_ms": <f64> }
//
// The RPC runs in a relay task that forwards decoded messages (and the final
// status) to the parked stream. Open therefore returns immediately — a
// client-streaming server sends its initial metadata only after the client
// half-closes, so waiting for it here would deadlock. Server-side errors
// (UNIMPLEMENTED etc.) surface at the first recv/close, not at open.

pub(crate) async fn grpc_stream_open_action(
    params: &Value,
    ctx: &Context,
    step_name: &str,
) -> ActionOutput {
    let (id, mut conn) = match take_conn(params, ctx) {
        Ok(x) => x,
        Err(msg) => return err(step_name, &msg),
    };

    let result = (|| -> Result<(MethodDescriptor, Option<DynamicMessage>, MetadataMap), String> {
        let method_name = params
            .get("method")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or("'method' is required (\"package.Service/Method\")")?;
        let method = resolve_method(&conn.pool, method_name)?;
        let kind = StreamKind::of(&method);
        if kind == StreamKind::Unary {
            return Err(format!(
                "'{method_name}' is a unary method — use std/grpc-call@v1"
            ));
        }
        let message = match kind {
            StreamKind::Server => {
                conn.generator.begin_message();
                Some(build_message(&method.input(), params, &mut conn.generator)?)
            }
            _ => {
                if params.get("payload").is_some() || params.get("payload_base64").is_some() {
                    return Err(format!(
                        "'{method_name}' is {}-streaming — send messages with \
                         std/grpc-stream-send@v1, 'payload' is only for server-streaming",
                        kind.as_str(),
                    ));
                }
                None
            }
        };
        let metadata = build_metadata(&conn.metadata, params)?;
        Ok((method, message, metadata))
    })();

    let (method, message, metadata) = match result {
        Ok(x) => x,
        Err(msg) => {
            ctx.resources.put_back_grpc(&id, conn);
            return err(step_name, &msg);
        }
    };

    let t0 = Instant::now();
    let kind = StreamKind::of(&method);
    let path = match method_path(&method) {
        Ok(p) => p,
        Err(msg) => {
            ctx.resources.put_back_grpc(&id, conn);
            return err(step_name, &msg);
        }
    };
    let codec = DynamicCodec {
        output: method.output(),
    };
    let channel = conn.channel.clone();
    let max_recv_size = conn.max_recv_size;
    let url = conn.url.clone();
    ctx.resources.put_back_grpc(&id, conn);

    // Request side: client/bidi streams take messages from the parked sender;
    // server-streaming sends its one request up front.
    let (req_tx, req_rx) = tokio::sync::mpsc::channel::<DynamicMessage>(64);
    // Response side: the relay task forwards decoded messages; dropping the
    // sender signals a clean (status-OK) end of stream.
    let (resp_tx, resp_rx) = tokio::sync::mpsc::channel::<Result<DynamicMessage, Status>>(64);

    tokio::spawn(async move {
        let result = async {
            let mut client = grpc_client(&channel, max_recv_size);
            client
                .ready()
                .await
                .map_err(|e| Status::unavailable(error_chain(&e)))?;
            match message {
                Some(msg) => {
                    let mut request = Request::new(msg);
                    *request.metadata_mut() = metadata;
                    client.server_streaming(request, path, codec).await
                }
                None => {
                    let stream = tokio_stream::wrappers::ReceiverStream::new(req_rx);
                    let mut request = Request::new(stream);
                    *request.metadata_mut() = metadata;
                    client.streaming(request, path, codec).await
                }
            }
        }
        .await;
        match result {
            Err(status) => {
                let _ = resp_tx.send(Err(status)).await;
            }
            Ok(response) => {
                let mut stream = response.into_inner();
                loop {
                    match stream.message().await {
                        Ok(Some(msg)) => {
                            if resp_tx.send(Ok(msg)).await.is_err() {
                                return; // scenario dropped the stream
                            }
                        }
                        Ok(None) => return, // clean end
                        Err(status) => {
                            let _ = resp_tx.send(Err(status)).await;
                            return;
                        }
                    }
                }
            }
        }
    });

    let stream = GrpcStream {
        sender: if kind == StreamKind::Server {
            None
        } else {
            Some(req_tx)
        },
        receiver: resp_rx,
        method: method.clone(),
        generator: Gen::new(uuid::Uuid::new_v4().as_u128() as u64),
        url: url.clone(),
        last_send: None,
    };
    let stream_id = ctx.resources.insert_grpc_stream(stream);
    let duration_ms = t0.elapsed().as_secs_f64() * 1000.0;

    ActionOutput {
        value: json!({
            "id": stream_id,
            "kind": kind.as_str(),
            "open": true,
            "duration_ms": duration_ms,
        }),
        logs: vec![(
            LogTag::Out,
            format!(
                "gRPC {url} {} stream → {stream_id} ({}) ({duration_ms:.2}ms)",
                method_display(&method),
                kind.as_str()
            ),
        )],
        success: true,
        http_sample: None,
    }
}

// ---------------------------------------------------------------------------
// std/grpc-stream-send@v1
// ---------------------------------------------------------------------------
//
// Parameters:
//   id             – Stream ID from std/grpc-stream-open@v1 (required)
//   payload        – JSON request message; `${…}` tokens expand per send
//   payload_base64 – serialized protobuf bytes (mutually exclusive)
//   repeat         – how many messages to emit, default 1
//   interval_ms    – gap between repeated sends, default 0
//   timeout        – ms for the whole send loop, default 10000
//
// Output:
//   { "sent": <u64>, "duration_ms": <f64>,
//     "metrics": { "grpc_msgs_sent": <u64> } }
//
// A send error means the peer (or the relay task) is gone: the step fails
// and the stream is dropped. A parameter error leaves the stream usable.

pub(crate) async fn grpc_stream_send_action(
    params: &Value,
    ctx: &Context,
    step_name: &str,
) -> ActionOutput {
    let (id, mut stream) = match take_stream(params, ctx) {
        Ok(x) => x,
        Err(msg) => return err(step_name, &msg),
    };
    if stream.sender.is_none() {
        let url = stream.url.clone();
        ctx.resources.put_back_grpc_stream(&id, stream);
        return err(
            step_name,
            &format!("gRPC send [{id}] ({url}): server-streaming — the server produces all messages; nothing to send"),
        );
    }

    let repeat = params.get("repeat").map(|v| u64_param(v, 1)).unwrap_or(1);
    let interval_ms = params
        .get("interval_ms")
        .map(|v| u64_param(v, 0))
        .unwrap_or(0);
    let timeout_ms = params
        .get("timeout")
        .map(|v| u64_param(v, 10_000))
        .unwrap_or(10_000);

    let t0 = Instant::now();
    let deadline = t0 + Duration::from_millis(timeout_ms);

    let mut sent = 0u64;
    let mut send_err: Option<String> = None;
    for i in 0..repeat.max(1) {
        if i > 0 && interval_ms > 0 {
            tokio::time::sleep(Duration::from_millis(interval_ms)).await;
        }
        if Instant::now() >= deadline {
            send_err = Some(format!("timeout after {sent} of {repeat} sends"));
            break;
        }
        stream.generator.begin_message();
        let msg = match build_message(&stream.method.input(), params, &mut stream.generator) {
            Ok(m) => m,
            Err(msg) => {
                // Parameter error — the stream stays usable.
                ctx.resources.put_back_grpc_stream(&id, stream);
                return err(step_name, &msg);
            }
        };
        let sender = stream.sender.as_ref().expect("checked above");
        let remaining = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(remaining, sender.send(msg)).await {
            Ok(Ok(())) => sent += 1,
            Ok(Err(_)) => {
                send_err = Some(peer_error(&mut stream.receiver).unwrap_or_else(|| {
                    "stream closed by the peer (or the call failed)".to_string()
                }));
                break;
            }
            Err(_) => {
                send_err = Some(format!("send TIMEOUT after {sent} of {repeat} sends"));
                break;
            }
        }
    }
    let duration_ms = t0.elapsed().as_secs_f64() * 1000.0;

    match send_err {
        None => {
            stream.last_send = Some(Instant::now());
            let url = stream.url.clone();
            ctx.resources.put_back_grpc_stream(&id, stream);
            ActionOutput {
                value: json!({
                    "sent": sent,
                    "duration_ms": duration_ms,
                    "metrics": { "grpc_msgs_sent": sent },
                }),
                logs: vec![(
                    LogTag::Out,
                    format!("gRPC {url} [{id}] → sent {sent} msg(s) ({duration_ms:.2}ms)"),
                )],
                success: true,
                http_sample: None,
            }
        }
        // The relay task is gone or the channel is full because the call
        // failed — a parked broken stream would just fail the next step with
        // a less honest message, so drop the id.
        Some(msg) => err(step_name, &format!("gRPC send [{id}]: {msg}")),
    }
}

/// Pull a pending terminal status out of the response channel without
/// blocking — used to explain a failed send with the real call status.
fn peer_error(
    receiver: &mut tokio::sync::mpsc::Receiver<Result<DynamicMessage, Status>>,
) -> Option<String> {
    match receiver.try_recv() {
        Ok(Err(status)) => Some(status_line(&status)),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// std/grpc-stream-recv@v1
// ---------------------------------------------------------------------------
//
// Parameters:
//   id             – Stream ID (required)
//   count          – stop after N messages (default 1; ignored when an
//                    until rule is given)
//   until_contains – stop when a message contains this substring (string
//                    messages: direct; object messages: compact-JSON form)
//   until_json     – stop when a message JSON-subset-matches this object
//   timeout        – ms, default 10000. Not reaching the stopping rule in
//                    time FAILS the step.
//
// Output:
//   { "messages": [...], "count": <u64>, "matched": <bool>,
//     "duration_ms": <f64>,
//     "metrics": { "grpc_msgs_received": <u64>, "grpc_msg_rtt": [<f64>]? } }
//
// `grpc_msg_rtt` appears only when an until rule matched and a
// grpc-stream-send preceded it on this stream — the send→match application
// RTT. A plain timeout fails the step but leaves the stream usable; the
// stream ending (cleanly or with a status) before the rule is reached fails
// the step and drops the stream.

pub(crate) async fn grpc_stream_recv_action(
    params: &Value,
    ctx: &Context,
    step_name: &str,
) -> ActionOutput {
    let (id, mut stream) = match take_stream(params, ctx) {
        Ok(x) => x,
        Err(msg) => return err(step_name, &msg),
    };
    let until = match Until::from_params(params) {
        Ok(u) => u,
        Err(msg) => {
            ctx.resources.put_back_grpc_stream(&id, stream);
            return err(step_name, &msg);
        }
    };
    let timeout_ms = params
        .get("timeout")
        .map(|v| u64_param(v, 10_000))
        .unwrap_or(10_000);

    let t0 = Instant::now();
    let deadline = t0 + Duration::from_millis(timeout_ms);
    let sent_at = stream.last_send.take();

    let target = match until {
        Until::Count(n) => n,
        _ => u64::MAX,
    };
    let mut messages: Vec<Value> = Vec::new();
    let mut satisfied = false;
    let mut rtt_ms: Option<f64> = None;
    let mut closed = false;
    let mut error: Option<String> = None;

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, stream.receiver.recv()).await {
            Err(_) => break, // timeout
            Ok(None) => {
                closed = true; // clean end of stream
                break;
            }
            Ok(Some(Err(status))) => {
                closed = true;
                error = Some(status_line(&status));
                break;
            }
            Ok(Some(Ok(msg))) => {
                let v = serde_json::to_value(&msg).unwrap_or(Value::Null);
                let matched = until_matches(&until, &v);
                messages.push(v);
                if matched {
                    satisfied = true;
                    rtt_ms = sent_at.map(|t| t.elapsed().as_secs_f64() * 1000.0);
                    break;
                }
                if messages.len() as u64 >= target {
                    satisfied = true;
                    break;
                }
            }
        }
    }
    let duration_ms = t0.elapsed().as_secs_f64() * 1000.0;

    let url = stream.url.clone();
    if closed {
        // The stream is exhausted (or failed) — a parked dead stream would
        // just fail the next step with a less honest message.
        drop(stream);
    } else {
        ctx.resources.put_back_grpc_stream(&id, stream);
    }

    let received = messages.len() as u64;
    let mut metrics = json!({ "grpc_msgs_received": received });
    if let Some(rtt) = rtt_ms {
        metrics["grpc_msg_rtt"] = json!([rtt]);
    }

    let mut value = json!({
        "messages": messages,
        "count": received,
        "matched": satisfied,
        "duration_ms": duration_ms,
        "metrics": metrics,
    });

    let mut logs = vec![(
        if satisfied { LogTag::Out } else { LogTag::Err },
        format!("gRPC {url} [{id}] ← {received} msg(s) ({duration_ms:.2}ms)"),
    )];
    if let Some(e) = &error {
        value["error"] = json!(e);
        logs.push((LogTag::Err, format!("{step_name}: gRPC recv [{id}]: {e}")));
    } else if !satisfied {
        let why = if closed {
            "stream ended before the stopping rule was reached"
        } else {
            "timeout before the stopping rule was reached"
        };
        logs.push((LogTag::Err, format!("{step_name}: gRPC recv [{id}]: {why}")));
    }

    ActionOutput {
        value,
        logs,
        success: satisfied,
        http_sample: None,
    }
}

/// Does this received message satisfy the until-rule? gRPC messages are
/// already JSON values (unlike ws frames): `until_json` subset-matches
/// directly; `until_contains` searches string payloads, or the compact-JSON
/// form of object payloads.
fn until_matches(until: &Until, received: &Value) -> bool {
    match until {
        Until::Count(_) => false,
        Until::Contains(needle) => match received.as_str() {
            Some(s) => s.contains(needle),
            None => serde_json::to_string(received).is_ok_and(|s| s.contains(needle)),
        },
        Until::Json(pattern) => json_subset_match(pattern, received),
    }
}

// ---------------------------------------------------------------------------
// std/grpc-stream-close@v1
// ---------------------------------------------------------------------------
//
// Parameters:
//   id             – Stream ID (required)
//   expect_status  – expected final gRPC status code, default 0
//   timeout        – ms to drain after half-close, default 10000
//
// Output:
//   { "closed": true, "status": <i32>, "received": <u64>, "messages": [...],
//     "duration_ms": <f64>, "error": <string>?,
//     "metrics": { "grpc_msgs_received": <u64>, "grpc_req_failed": 0|1 } }
//
// For client-streaming/bidi the sender is dropped first (half-close) so the
// server sees end-of-input; remaining server messages are then drained until
// the final status. For a client-streaming method the drained single message
// is the call's response. The id is released either way.

pub(crate) async fn grpc_stream_close_action(
    params: &Value,
    ctx: &Context,
    step_name: &str,
) -> ActionOutput {
    let (id, mut stream) = match take_stream(params, ctx) {
        Ok(x) => x,
        Err(msg) => return err(step_name, &msg),
    };
    let expect = expect_status(params);
    let timeout_ms = params
        .get("timeout")
        .map(|v| u64_param(v, 10_000))
        .unwrap_or(10_000);

    let t0 = Instant::now();
    let deadline = t0 + Duration::from_millis(timeout_ms);

    // Half-close: end of client input (no-op for server-streaming).
    drop(stream.sender.take());

    let mut messages: Vec<Value> = Vec::new();
    let mut final_status = Code::Ok;
    let mut error: Option<String> = None;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            final_status = Code::DeadlineExceeded;
            error = Some(format!(
                "timeout after {timeout_ms}ms waiting for the stream to end"
            ));
            break;
        }
        match tokio::time::timeout(remaining, stream.receiver.recv()).await {
            Err(_) => {
                final_status = Code::DeadlineExceeded;
                error = Some(format!(
                    "timeout after {timeout_ms}ms waiting for the stream to end"
                ));
                break;
            }
            Ok(None) => break, // clean end, status OK
            Ok(Some(Err(status))) => {
                final_status = status.code();
                error = Some(status_line(&status));
                break;
            }
            Ok(Some(Ok(msg))) => {
                messages.push(serde_json::to_value(&msg).unwrap_or(Value::Null));
            }
        }
    }
    let duration_ms = t0.elapsed().as_secs_f64() * 1000.0;

    let url = stream.url.clone();
    drop(stream);

    let received = messages.len() as u64;
    let code = final_status as i32;
    let ok = code == expect;

    let mut value = json!({
        "closed": true,
        "status": code,
        "received": received,
        "messages": messages,
        "duration_ms": duration_ms,
        "metrics": {
            "grpc_msgs_received": received,
            "grpc_req_failed": if ok { 0 } else { 1 },
        },
    });
    let mut logs = vec![(
        if ok { LogTag::Out } else { LogTag::Err },
        format!(
            "gRPC {url} [{id}] closed → {} ({received} msg(s) drained) ({duration_ms:.2}ms)",
            status_name(final_status)
        ),
    )];
    if let Some(e) = &error {
        value["error"] = json!(e);
        logs.push((LogTag::Err, format!("{step_name}: gRPC close [{id}]: {e}")));
    } else if !ok {
        logs.push((
            LogTag::Err,
            format!(
                "{step_name}: gRPC close [{id}]: expected status {expect}, got {code} ({})",
                status_name(final_status)
            ),
        ));
    }

    ActionOutput {
        value,
        logs,
        success: ok,
        http_sample: None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::step::actions::execute_action;
    use crate::testsupport::{
        descriptor_set_base64, start_echo_server, start_echo_server_tls, ECHO_DESCRIPTOR_SET,
    };

    fn plain_url(port: u16) -> String {
        format!("grpc://127.0.0.1:{port}")
    }

    /// Pool decoded from the test descriptor set (no server needed).
    fn test_pool() -> DescriptorPool {
        DescriptorPool::decode(ECHO_DESCRIPTOR_SET).unwrap()
    }

    // -----------------------------------------------------------------
    // Schema + method resolution (pure unit tests)
    // -----------------------------------------------------------------

    #[test]
    fn resolve_method_happy_path() {
        let pool = test_pool();
        let m = resolve_method(&pool, "perfscale.test.v1.Echo/Unary").unwrap();
        assert_eq!(m.name(), "Unary");
        assert!(!m.is_client_streaming());
        assert!(!m.is_server_streaming());
        assert_eq!(m.input().full_name(), "perfscale.test.v1.EchoRequest");
    }

    #[test]
    fn resolve_method_requires_slash_form() {
        let pool = test_pool();
        let err = resolve_method(&pool, "perfscale.test.v1.Echo.Unary").unwrap_err();
        assert!(err.contains("package.Service/Method"), "got: {err}");
    }

    #[test]
    fn resolve_method_typo_suggests_candidate() {
        let pool = test_pool();
        let err = resolve_method(&pool, "perfscale.test.v1.Echo/Unar").unwrap_err();
        assert!(
            err.contains("did you mean 'perfscale.test.v1.Echo/Unary'?"),
            "got: {err}"
        );
    }

    #[test]
    fn resolve_method_unknown_lists_candidates() {
        let pool = test_pool();
        let err = resolve_method(&pool, "completely.Off/Nope").unwrap_err();
        assert!(err.contains("schema has:"), "got: {err}");
        assert!(err.contains("perfscale.test.v1.Echo/Unary"), "got: {err}");
    }

    #[test]
    fn profile_requires_exactly_one_schema_source() {
        // Neither source.
        let err = resolve_profile(&json!({ "url": "grpc://x:1" })).unwrap_err();
        assert!(err.contains("schema required"), "got: {err}");
        // Both sources.
        let err = resolve_profile(&json!({
            "url": "grpc://x:1",
            "descriptor_set": descriptor_set_base64(),
            "reflection": true,
        }))
        .unwrap_err();
        assert!(err.contains("mutually exclusive"), "got: {err}");
    }

    #[test]
    fn profile_url_schemes() {
        // grpc:// plaintext, grpcs:// TLS, bare host defaults to TLS.
        let p = resolve_profile(&json!({
            "url": "grpc://h:1", "descriptor_set": descriptor_set_base64() }))
        .unwrap();
        assert!(!p.tls && p.endpoint_uri == "http://h:1");
        let p = resolve_profile(&json!({
            "url": "grpcs://h:2", "descriptor_set": descriptor_set_base64() }))
        .unwrap();
        assert!(p.tls && p.endpoint_uri == "https://h:2");
        let p = resolve_profile(&json!({
            "url": "h:3", "descriptor_set": descriptor_set_base64() }))
        .unwrap();
        assert!(p.tls && p.endpoint_uri == "https://h:3");
        let err = resolve_profile(&json!({
            "url": "http://h:4", "descriptor_set": descriptor_set_base64() }))
        .unwrap_err();
        assert!(err.contains("grpc:// or grpcs://"), "got: {err}");
    }

    #[test]
    fn profile_rejects_bin_metadata() {
        let err = resolve_profile(&json!({
            "url": "grpc://h:1",
            "descriptor_set": descriptor_set_base64(),
            "metadata": { "trace-bin": "AAAA" },
        }))
        .unwrap_err();
        assert!(err.contains("-bin"), "got: {err}");
    }

    #[test]
    fn connection_profile_string_merges_and_inline_wins() {
        let profile = json!({
            "url": "grpc://profile:1",
            "descriptor_set": descriptor_set_base64(),
            "metadata": { "a": "1" },
        })
        .to_string();
        let p = resolve_profile(&json!({
            "connection": profile,
            "url": "grpc://inline:2",
        }))
        .unwrap();
        assert_eq!(p.url, "grpc://inline:2");
        assert_eq!(p.metadata, vec![("a".to_string(), "1".to_string())]);
        assert!(p.descriptor_set.is_some());
    }

    // -----------------------------------------------------------------
    // Parameter validation through dispatch (no server)
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn descriptor_set_bad_base64_fails_connect() {
        let ctx = Context::new();
        let out = execute_action(
            "std/grpc-connect@v1",
            &json!({ "url": "grpc://127.0.0.1:1", "descriptor_set": "!!!not-base64!!!" }),
            &ctx,
            "c",
        )
        .await;
        assert!(!out.success);
        let line = &out.logs[0].1;
        assert!(line.contains("invalid base64"), "got: {line}");
    }

    #[tokio::test]
    async fn descriptor_set_bad_bytes_fail_connect() {
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD.encode(b"garbage-not-a-fds");
        let ctx = Context::new();
        let out = execute_action(
            "std/grpc-connect@v1",
            &json!({ "url": "grpc://127.0.0.1:1", "descriptor_set": b64 }),
            &ctx,
            "c",
        )
        .await;
        assert!(!out.success);
        assert!(
            out.logs[0].1.contains("invalid 'descriptor_set'"),
            "got: {}",
            out.logs[0].1
        );
    }

    #[tokio::test]
    async fn unknown_connection_and_stream_ids() {
        let ctx = Context::new();
        let out = execute_action(
            "std/grpc-call@v1",
            &json!({ "id": "grpc-99", "method": "a.B/C", "payload": {} }),
            &ctx,
            "call",
        )
        .await;
        assert!(!out.success && out.logs[0].1.contains("unknown connection id 'grpc-99'"));

        let out = execute_action(
            "std/grpc-stream-recv@v1",
            &json!({ "id": "grpcs-99" }),
            &ctx,
            "recv",
        )
        .await;
        assert!(!out.success && out.logs[0].1.contains("unknown stream id 'grpcs-99'"));
    }

    #[tokio::test]
    async fn unknown_action_id_is_not_claimed_by_grpc_family() {
        let ctx = Context::new();
        let out = execute_action("std/grp@v1", &json!({}), &ctx, "x").await;
        assert!(!out.success);
        assert!(out.logs[0].1.contains("unknown action 'std/grp@v1'"));
    }

    // -----------------------------------------------------------------
    // Live server fixtures
    // -----------------------------------------------------------------

    /// Connect a live channel via descriptor_set and return (ctx, id).
    async fn connect(port: u16) -> (Context, String) {
        let ctx = Context::new();
        let out = execute_action(
            "std/grpc-connect@v1",
            &json!({
                "url": plain_url(port),
                "descriptor_set": descriptor_set_base64(),
            }),
            &ctx,
            "connect",
        )
        .await;
        assert!(out.success, "connect failed: {:?}", out.logs);
        let id = out.value["id"].as_str().unwrap().to_string();
        assert_eq!(id, "grpc-1");
        (ctx, id)
    }

    // -----------------------------------------------------------------
    // std/grpc@v1 — one-shot
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn oneshot_unary_echo_with_descriptor_set() {
        let port = start_echo_server().await;
        let ctx = Context::new();
        let out = execute_action(
            "std/grpc@v1",
            &json!({
                "url": plain_url(port),
                "descriptor_set": descriptor_set_base64(),
                "method": "perfscale.test.v1.Echo/Unary",
                "payload": { "message": "hello" },
            }),
            &ctx,
            "call",
        )
        .await;
        assert!(out.success, "logs: {:?}", out.logs);
        assert_eq!(out.value["status"], 0);
        assert_eq!(out.value["body"]["message"], "hello");
        let metrics = &out.value["metrics"];
        assert_eq!(metrics["grpc_msgs_sent"], 1);
        assert_eq!(metrics["grpc_msgs_received"], 1);
        assert_eq!(metrics["grpc_req_failed"], 0);
        assert!(metrics["grpc_req_duration"].is_array());
        assert!(metrics["grpc_msg_rtt"].is_array());
    }

    #[tokio::test]
    async fn oneshot_unary_echo_with_reflection() {
        let port = start_echo_server().await;
        let ctx = Context::new();
        let out = execute_action(
            "grpc", // short alias
            &json!({
                "url": plain_url(port),
                "reflection": true,
                "method": "perfscale.test.v1.Echo/Unary",
                "payload": { "message": "via reflection" },
            }),
            &ctx,
            "call",
        )
        .await;
        assert!(out.success, "logs: {:?}", out.logs);
        assert_eq!(out.value["body"]["message"], "via reflection");
    }

    #[tokio::test]
    async fn oneshot_payload_base64_roundtrip() {
        use prost::Message as _;
        let port = start_echo_server().await;
        let req = crate::testsupport::echo::EchoRequest {
            message: "raw-bytes".into(),
            count: 0,
            size: 0,
        };
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD.encode(req.encode_to_vec());
        let ctx = Context::new();
        let out = execute_action(
            "std/grpc@v1",
            &json!({
                "url": plain_url(port),
                "descriptor_set": descriptor_set_base64(),
                "method": "perfscale.test.v1.Echo/Unary",
                "payload_base64": b64,
            }),
            &ctx,
            "call",
        )
        .await;
        assert!(out.success, "logs: {:?}", out.logs);
        assert_eq!(out.value["body"]["message"], "raw-bytes");
    }

    #[tokio::test]
    async fn payload_and_payload_base64_are_mutually_exclusive() {
        let port = start_echo_server().await;
        let (ctx, id) = connect(port).await;
        let out = execute_action(
            "std/grpc-call@v1",
            &json!({
                "id": id,
                "method": "perfscale.test.v1.Echo/Unary",
                "payload": { "message": "x" },
                "payload_base64": "AAE=",
            }),
            &ctx,
            "call",
        )
        .await;
        assert!(!out.success && out.logs[0].1.contains("mutually exclusive"));
        // Parameter error — the channel survives.
        assert!(ctx.resources.take_grpc(&id).is_some());
    }

    #[tokio::test]
    async fn payload_type_mismatch_is_a_clean_error() {
        let port = start_echo_server().await;
        let (ctx, id) = connect(port).await;
        let out = execute_action(
            "std/grpc-call@v1",
            &json!({
                "id": id,
                "method": "perfscale.test.v1.Echo/Unary",
                "payload": { "message": 42 },
            }),
            &ctx,
            "call",
        )
        .await;
        assert!(!out.success);
        assert!(
            out.logs[0].1.contains("does not match"),
            "got: {}",
            out.logs[0].1
        );
    }

    #[tokio::test]
    async fn payload_token_expansion_per_call() {
        let port = start_echo_server().await;
        let (ctx, id) = connect(port).await;
        for want in ["ping-1", "ping-2"] {
            let out = execute_action(
                "std/grpc-call@v1",
                &json!({
                    "id": id,
                    "method": "perfscale.test.v1.Echo/Unary",
                    "payload": { "message": "ping-${seq}" },
                }),
                &ctx,
                "call",
            )
            .await;
            assert!(out.success, "logs: {:?}", out.logs);
            assert_eq!(
                out.value["body"]["message"], want,
                "${{seq}} keeps counting per channel"
            );
        }
    }

    // -----------------------------------------------------------------
    // expect_status
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn non_ok_status_fails_by_default_and_counts_failed() {
        let port = start_echo_server().await;
        let (ctx, id) = connect(port).await;
        let out = execute_action(
            "std/grpc-call@v1",
            &json!({
                "id": id,
                "method": "perfscale.test.v1.Echo/Fail",
                "payload": {},
            }),
            &ctx,
            "call",
        )
        .await;
        assert!(!out.success);
        assert_eq!(out.value["status"], 3); // INVALID_ARGUMENT
        assert_eq!(out.value["error"], "deliberate failure");
        assert_eq!(out.value["metrics"]["grpc_req_failed"], 1);
        assert_eq!(out.value["metrics"]["grpc_msgs_received"], 0);
        // A failed RPC does not kill the channel.
        assert!(ctx.resources.take_grpc(&id).is_some());
    }

    #[tokio::test]
    async fn expect_status_makes_error_paths_pass() {
        let port = start_echo_server().await;
        let (ctx, id) = connect(port).await;
        let out = execute_action(
            "grpc-call", // alias
            &json!({
                "id": id,
                "method": "perfscale.test.v1.Echo/Fail",
                "payload": {},
                "expect_status": 3,
            }),
            &ctx,
            "call",
        )
        .await;
        assert!(out.success, "logs: {:?}", out.logs);
        assert_eq!(out.value["metrics"]["grpc_req_failed"], 0);

        // And OK is a failure when an error status was expected.
        let out = execute_action(
            "std/grpc-call@v1",
            &json!({
                "id": id,
                "method": "perfscale.test.v1.Echo/Unary",
                "payload": { "message": "x" },
                "expect_status": 3,
            }),
            &ctx,
            "call",
        )
        .await;
        assert!(!out.success);
        assert_eq!(out.value["metrics"]["grpc_req_failed"], 1);
    }

    #[tokio::test]
    async fn streaming_method_rejected_by_unary_actions() {
        let port = start_echo_server().await;
        let (ctx, id) = connect(port).await;
        let out = execute_action(
            "std/grpc-call@v1",
            &json!({
                "id": id,
                "method": "perfscale.test.v1.Echo/Bidi",
                "payload": { "message": "x" },
            }),
            &ctx,
            "call",
        )
        .await;
        assert!(!out.success && out.logs[0].1.contains("grpc-stream-open"));

        let ctx2 = Context::new();
        let out = execute_action(
            "std/grpc@v1",
            &json!({
                "url": plain_url(port),
                "descriptor_set": descriptor_set_base64(),
                "method": "perfscale.test.v1.Echo/Bidi",
                "payload": { "message": "x" },
            }),
            &ctx2,
            "call",
        )
        .await;
        assert!(!out.success && out.logs[0].1.contains("grpc-stream-open"));
    }

    // -----------------------------------------------------------------
    // max_recv_size
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn max_recv_size_bounds_inbound_messages() {
        let port = start_echo_server().await;

        // 1 MiB message with a 64 KiB cap → the call fails.
        let ctx = Context::new();
        let out = execute_action(
            "std/grpc@v1",
            &json!({
                "url": plain_url(port),
                "descriptor_set": descriptor_set_base64(),
                "max_recv_size": 65536,
                "method": "perfscale.test.v1.Echo/Large",
                "payload": { "size": 1048576 },
            }),
            &ctx,
            "call",
        )
        .await;
        assert!(!out.success, "oversize message must fail: {:?}", out.value);
        assert_eq!(out.value["metrics"]["grpc_req_failed"], 1);

        // Default cap (16 MiB) takes it fine.
        let ctx = Context::new();
        let out = execute_action(
            "std/grpc@v1",
            &json!({
                "url": plain_url(port),
                "descriptor_set": descriptor_set_base64(),
                "method": "perfscale.test.v1.Echo/Large",
                "payload": { "size": 1048576 },
            }),
            &ctx,
            "call",
        )
        .await;
        assert!(out.success, "logs: {:?}", out.logs);
        assert_eq!(out.value["body"]["message"], "large");
        // bytes fields surface as base64 in protobuf-JSON.
        assert_eq!(
            out.value["body"]["padding"].as_str().unwrap().len(),
            1398104, // base64 of 1 MiB
        );
    }

    // -----------------------------------------------------------------
    // TLS
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn tls_self_signed_rejected_by_default() {
        let port = start_echo_server_tls().await;
        let ctx = Context::new();
        let out = execute_action(
            "std/grpc-connect@v1",
            &json!({
                "url": format!("grpcs://localhost:{port}"),
                "descriptor_set": descriptor_set_base64(),
            }),
            &ctx,
            "connect",
        )
        .await;
        assert!(!out.success, "self-signed must be rejected by default");
        assert_eq!(out.value["connected"], false);
    }

    #[tokio::test]
    async fn tls_self_signed_accepted_with_skip_tls_verify() {
        let port = start_echo_server_tls().await;
        let ctx = Context::new();
        let out = execute_action(
            "std/grpc-connect@v1",
            &json!({
                "url": format!("grpcs://localhost:{port}"),
                "descriptor_set": descriptor_set_base64(),
                "skipTLSVerify": true,
            }),
            &ctx,
            "connect",
        )
        .await;
        assert!(out.success, "logs: {:?}", out.logs);
        let id = out.value["id"].as_str().unwrap();
        let out = execute_action(
            "std/grpc-call@v1",
            &json!({
                "id": id,
                "method": "perfscale.test.v1.Echo/Unary",
                "payload": { "message": "over-tls" },
            }),
            &ctx,
            "call",
        )
        .await;
        assert!(out.success, "logs: {:?}", out.logs);
        assert_eq!(out.value["body"]["message"], "over-tls");
    }

    // -----------------------------------------------------------------
    // Reflection (through connect, incl. the per-URL pool cache)
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn connect_via_reflection_and_cache_reuse() {
        let port = start_echo_server().await;
        let ctx = Context::new();
        for expected_id in ["grpc-1", "grpc-2"] {
            let out = execute_action(
                "std/grpc-connect@v1",
                &json!({ "url": plain_url(port), "reflection": true }),
                &ctx,
                "connect",
            )
            .await;
            assert!(out.success, "logs: {:?}", out.logs);
            assert_eq!(out.value["id"], expected_id);
        }
        // The second connect must have hit the per-URL cache.
        assert!(ctx.resources.reflection_pool(&plain_url(port)).is_some());

        let out = execute_action(
            "std/grpc-call@v1",
            &json!({
                "id": "grpc-2",
                "method": "perfscale.test.v1.Echo/Unary",
                "payload": { "message": "cached-schema" },
            }),
            &ctx,
            "call",
        )
        .await;
        assert!(out.success, "logs: {:?}", out.logs);
        assert_eq!(out.value["body"]["message"], "cached-schema");
    }

    #[tokio::test]
    async fn reflection_failure_is_a_clean_connect_error() {
        // No server listening here.
        let ctx = Context::new();
        let out = execute_action(
            "std/grpc-connect@v1",
            &json!({ "url": "grpc://127.0.0.1:1", "reflection": true, "timeout": 2000 }),
            &ctx,
            "connect",
        )
        .await;
        assert!(!out.success);
        assert_eq!(out.value["connected"], false);
        assert!(out.value["error"].as_str().is_some());
    }

    // -----------------------------------------------------------------
    // Streams
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn server_streaming_open_recv_close() {
        let port = start_echo_server().await;
        let (ctx, id) = connect(port).await;

        let out = execute_action(
            "std/grpc-stream-open@v1",
            &json!({
                "id": id,
                "method": "perfscale.test.v1.Echo/ServerStream",
                "payload": { "message": "stream", "count": 3 },
            }),
            &ctx,
            "open",
        )
        .await;
        assert!(out.success, "logs: {:?}", out.logs);
        assert_eq!(out.value["kind"], "server");
        let sid = out.value["id"].as_str().unwrap().to_string();
        assert_eq!(sid, "grpcs-1");

        // Sending on a server-streaming stream is a parameter error (stream survives).
        let out = execute_action(
            "std/grpc-stream-send@v1",
            &json!({ "id": sid, "payload": { "message": "x" } }),
            &ctx,
            "send",
        )
        .await;
        assert!(!out.success && out.logs[0].1.contains("server-streaming"));

        // The stream survived the parameter error — recv still works.
        let out = execute_action(
            "std/grpc-stream-recv@v1",
            &json!({ "id": sid, "count": 3 }),
            &ctx,
            "recv",
        )
        .await;
        assert!(out.success, "logs: {:?}", out.logs);
        assert_eq!(out.value["count"], 3);
        assert_eq!(out.value["messages"][2]["seq"], 3);
        assert_eq!(out.value["metrics"]["grpc_msgs_received"], 3);

        // The server already sent everything; close drains the clean end.
        let out = execute_action(
            "std/grpc-stream-close@v1",
            &json!({ "id": sid }),
            &ctx,
            "close",
        )
        .await;
        assert!(out.success, "logs: {:?}", out.logs);
        assert_eq!(out.value["status"], 0);
        assert_eq!(out.value["closed"], true);
        assert!(
            ctx.resources.take_grpc_stream(&sid).is_none(),
            "close releases the id"
        );
    }

    #[tokio::test]
    async fn client_streaming_send_close_gets_response() {
        let port = start_echo_server().await;
        let (ctx, id) = connect(port).await;

        let out = execute_action(
            "std/grpc-stream-open@v1",
            &json!({ "id": id, "method": "perfscale.test.v1.Echo/ClientStream" }),
            &ctx,
            "open",
        )
        .await;
        assert!(out.success, "logs: {:?}", out.logs);
        assert_eq!(out.value["kind"], "client");
        let sid = out.value["id"].as_str().unwrap().to_string();

        // payload at open is rejected for client-streaming.
        let out = execute_action(
            "std/grpc-stream-open@v1",
            &json!({
                "id": id,
                "method": "perfscale.test.v1.Echo/ClientStream",
                "payload": { "message": "x" },
            }),
            &ctx,
            "open2",
        )
        .await;
        assert!(!out.success && out.logs[0].1.contains("grpc-stream-send"));

        let out = execute_action(
            "std/grpc-stream-send@v1",
            &json!({ "id": sid, "payload": { "message": "evt-${seq}" }, "repeat": 3 }),
            &ctx,
            "send",
        )
        .await;
        assert!(out.success, "logs: {:?}", out.logs);
        assert_eq!(out.value["sent"], 3);
        assert_eq!(out.value["metrics"]["grpc_msgs_sent"], 3);

        // Half-close → server responds once → drained at close.
        let out = execute_action(
            "std/grpc-stream-close@v1",
            &json!({ "id": sid }),
            &ctx,
            "close",
        )
        .await;
        assert!(out.success, "logs: {:?}", out.logs);
        assert_eq!(out.value["status"], 0);
        assert_eq!(out.value["received"], 1);
        assert_eq!(
            out.value["messages"][0]["message"],
            "3 messages, last: evt-3"
        );
        assert_eq!(out.value["metrics"]["grpc_msgs_received"], 1);
    }

    #[tokio::test]
    async fn bidi_send_recv_until_close() {
        let port = start_echo_server().await;
        let (ctx, id) = connect(port).await;

        let out = execute_action(
            "std/grpc-stream-open@v1",
            &json!({ "id": id, "method": "perfscale.test.v1.Echo/Bidi" }),
            &ctx,
            "open",
        )
        .await;
        assert!(out.success, "logs: {:?}", out.logs);
        assert_eq!(out.value["kind"], "bidi");
        let sid = out.value["id"].as_str().unwrap().to_string();

        let out = execute_action(
            "std/grpc-stream-send@v1",
            &json!({ "id": sid, "payload": { "message": "m-${seq}" }, "repeat": 5, "interval_ms": 5 }),
            &ctx,
            "send",
        )
        .await;
        assert!(out.success, "logs: {:?}", out.logs);
        assert_eq!(out.value["sent"], 5);

        // until_contains matches the compact-JSON form of the message.
        let out = execute_action(
            "std/grpc-stream-recv@v1",
            &json!({ "id": sid, "until_contains": "m-5", "timeout": 5000 }),
            &ctx,
            "recv",
        )
        .await;
        assert!(out.success, "logs: {:?}", out.logs);
        assert_eq!(out.value["matched"], true);
        assert_eq!(out.value["count"], 5);
        assert_eq!(out.value["messages"][4]["message"], "m-5");
        assert!(
            out.value["metrics"]["grpc_msg_rtt"].is_array(),
            "send→match RTT recorded"
        );

        // until_json subset-matches objects directly.
        let out = execute_action(
            "std/grpc-stream-send@v1",
            &json!({ "id": sid, "payload": { "message": "again" } }),
            &ctx,
            "send2",
        )
        .await;
        assert!(out.success);
        let out = execute_action(
            "std/grpc-stream-recv@v1",
            &json!({ "id": sid, "until_json": { "message": "again" }, "timeout": 5000 }),
            &ctx,
            "recv2",
        )
        .await;
        assert!(
            out.success && out.value["matched"] == true,
            "logs: {:?}",
            out.logs
        );

        let out = execute_action(
            "std/grpc-stream-close@v1",
            &json!({ "id": sid }),
            &ctx,
            "close",
        )
        .await;
        assert!(out.success, "logs: {:?}", out.logs);
        assert_eq!(out.value["status"], 0);
    }

    #[tokio::test]
    async fn recv_on_ended_stream_fails_and_drops_id() {
        let port = start_echo_server().await;
        let (ctx, id) = connect(port).await;

        let out = execute_action(
            "std/grpc-stream-open@v1",
            &json!({
                "id": id,
                "method": "perfscale.test.v1.Echo/ServerStream",
                "payload": { "message": "s", "count": 1 },
            }),
            &ctx,
            "open",
        )
        .await;
        let sid = out.value["id"].as_str().unwrap().to_string();

        // Read the one message, then ask for more than the server will send.
        let out = execute_action(
            "std/grpc-stream-recv@v1",
            &json!({ "id": sid, "count": 5, "timeout": 5000 }),
            &ctx,
            "recv",
        )
        .await;
        assert!(!out.success, "stream ends before count is reached");
        assert_eq!(out.value["count"], 1); // messages read along the way are kept
        assert!(out.logs.iter().any(|(_, l)| l.contains("stream ended")));
        assert!(
            ctx.resources.take_grpc_stream(&sid).is_none(),
            "ended stream is dropped"
        );
    }

    #[tokio::test]
    async fn recv_timeout_fails_but_stream_survives() {
        let port = start_echo_server().await;
        let (ctx, id) = connect(port).await;

        let out = execute_action(
            "std/grpc-stream-open@v1",
            &json!({ "id": id, "method": "perfscale.test.v1.Echo/Bidi" }),
            &ctx,
            "open",
        )
        .await;
        let sid = out.value["id"].as_str().unwrap().to_string();

        let out = execute_action(
            "std/grpc-stream-recv@v1",
            &json!({ "id": sid, "timeout": 100 }),
            &ctx,
            "recv",
        )
        .await;
        assert!(!out.success);
        assert!(out.logs.iter().any(|(_, l)| l.contains("timeout")));
        assert!(
            ctx.resources.take_grpc_stream(&sid).is_some(),
            "timeout keeps the stream"
        );
    }

    #[tokio::test]
    async fn close_honors_expect_status() {
        let port = start_echo_server().await;
        let (ctx, id) = connect(port).await;

        // Fail is unary; emulate a status-bearing stream via Bidi then close
        // with a wrong expectation: final status is OK(0).
        let out = execute_action(
            "std/grpc-stream-open@v1",
            &json!({ "id": id, "method": "perfscale.test.v1.Echo/Bidi" }),
            &ctx,
            "open",
        )
        .await;
        let sid = out.value["id"].as_str().unwrap().to_string();
        let out = execute_action(
            "std/grpc-stream-close@v1",
            &json!({ "id": sid, "expect_status": 5 }),
            &ctx,
            "close",
        )
        .await;
        assert!(!out.success, "status 0 != expected 5");
        assert_eq!(out.value["metrics"]["grpc_req_failed"], 1);
    }

    #[tokio::test]
    async fn unary_method_rejected_by_stream_open() {
        let port = start_echo_server().await;
        let (ctx, id) = connect(port).await;
        let out = execute_action(
            "std/grpc-stream-open@v1",
            &json!({ "id": id, "method": "perfscale.test.v1.Echo/Unary" }),
            &ctx,
            "open",
        )
        .await;
        assert!(!out.success && out.logs[0].1.contains("grpc-call"));
    }

    // -----------------------------------------------------------------
    // Concurrency sanity: 10 VUs × 100 unary calls
    // -----------------------------------------------------------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn ten_vus_hundred_unary_calls_each() {
        let port = start_echo_server().await;
        let mut handles = Vec::new();
        for _ in 0..10 {
            handles.push(tokio::spawn(async move {
                let (ctx, id) = connect(port).await;
                for i in 0..100 {
                    let out = execute_action(
                        "std/grpc-call@v1",
                        &json!({
                            "id": id,
                            "method": "perfscale.test.v1.Echo/Unary",
                            "payload": { "message": format!("vu-call-{i}") },
                        }),
                        &ctx,
                        "call",
                    )
                    .await;
                    assert!(out.success, "call {i} failed: {:?}", out.logs);
                }
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
    }

    // -----------------------------------------------------------------
    // http → body_base64 → descriptor_set pipeline (wiremock + echo server)
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn fetched_descriptor_set_flows_into_grpc_step() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/schema.pb"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/octet-stream")
                    .set_body_bytes(ECHO_DESCRIPTOR_SET),
            )
            .mount(&server)
            .await;

        let port = start_echo_server().await;
        let mut ctx = Context::new();

        // Step 1: fetch the schema over HTTP.
        let fetch = execute_action(
            "std/http@v1",
            &json!({ "url": format!("{}/schema.pb", server.uri()) }),
            &ctx,
            "fetch",
        )
        .await;
        assert!(fetch.success, "logs: {:?}", fetch.logs);
        assert_eq!(fetch.value["body"], "");
        assert!(fetch.value["body_base64"].as_str().is_some());
        ctx.set("fetch", fetch.value);

        // Step 2: the interpolated base64 is the schema source.
        let out = execute_action(
            "std/grpc@v1",
            &json!({
                "url": plain_url(port),
                "descriptor_set": "${{ fetch.body_base64 }}",
                "method": "perfscale.test.v1.Echo/Unary",
                "payload": { "message": "schema-over-http" },
            }),
            &ctx,
            "call",
        )
        .await;
        assert!(out.success, "logs: {:?}", out.logs);
        assert_eq!(out.value["body"]["message"], "schema-over-http");
    }
}
