//! LLM action: `std/llm@v1` — drive chat-completion endpoints under load,
//! measuring time-to-first-token (TTFT), generation throughput (tokens/sec),
//! and token usage per request.
//!
//! One step = one completion request. Streaming is the default for the
//! `openai` / `anthropic` endpoints: the response is read as server-sent
//! events, text deltas are concatenated, and TTFT is the arrival time of the
//! first chunk that carries content.
//!
//! # Endpoints
//!
//! | Endpoint    | Wire format |
//! |-------------|-------------|
//! | `openai` (default) | OpenAI chat completions (`/v1/chat/completions`) — also covers OpenAI-compatible servers (Ollama, vLLM, LM Studio, …). SSE chunks: `choices[0].delta.content`, final `usage` chunk via `stream_options.include_usage` |
//! | `anthropic` | Anthropic messages API (`/v1/messages`). SSE events: `message_start` (input tokens), `content_block_delta` (text), `message_delta` (output tokens) |
//! | `generic`   | Body is the `params` object verbatim; the response is one JSON document (or an SSE stream glued back together), fields pulled out via `extract` |
//!
//! # Parameters
//!
//! | Parameter    | Type            | Default    | Description |
//! |--------------|-----------------|------------|-------------|
//! | `endpoint`   | string          | `"openai"` | `openai`, `anthropic`, or `generic` |
//! | `url`        | string          | required   | Completion endpoint URL |
//! | `model`      | string          | —          | Model name; required for `openai` / `anthropic` |
//! | `prompt`     | string          | —          | Sugar for a single `user` message (mutually exclusive with `messages`) |
//! | `messages`   | array           | —          | `[{ role, content }]` chat messages |
//! | `max_tokens` | integer         | `256`      | Completion token cap |
//! | `stream`     | bool            | `true` (`openai`/`anthropic`), `false` (`generic`) | Stream the response as SSE |
//! | `api_key`    | string          | —          | `Authorization: Bearer` (`openai`/`generic`), `x-api-key` (`anthropic`, which also sends `anthropic-version: 2023-06-01`) |
//! | `headers`    | object          | —          | Extra request headers, string values |
//! | `params`     | object          | —          | Passthrough into the request body (`temperature`, …); for `generic` it IS the body |
//! | `extract`    | object          | —          | `generic` only: `{ text, prompt_tokens, completion_tokens }` — each a dotted path (`$.usage.completion_tokens`, `$.choices[0].text`) or a regex with one capture group |
//! | `timeout_ms` | integer         | `120000`   | Whole-request timeout (connect → last chunk) |
//!
//! # Output
//!
//! ```json
//! { "endpoint": "openai", "model": "gpt-4o-mini", "status": 200,
//!   "ttft_ms": 120.31, "duration_ms": 850.02,
//!   "prompt_tokens": 12, "completion_tokens": 96,
//!   "tokens_per_sec": 131.5, "chunks": 34, "text": "…",
//!   "metrics": { "llm_ttft_ms": [120.31], "llm_tokens_per_sec": [131.5],
//!                "llm_prompt_tokens": 12, "llm_completion_tokens": 96,
//!                "llm_chunks": 34 } }
//! ```
//!
//! `ttft_ms` appears only for streamed responses; `text` is truncated to
//! ~4 KiB. A non-2xx status fails the step with the status and the first
//! ~500 characters of the error body.
//!
//! # Observer seam (downstream pro crates)
//!
//! [`register_llm_observer`] lets a downstream (proprietary) crate receive an
//! [`LlmSample`] for every completed request — including failed ones — with
//! the per-chunk arrival deltas needed for detailed ITL/TPOT and cost
//! metrics. Observer panics are contained: a misbehaving observer never
//! affects the step.
//!
//! [`register_llm_metrics_observer`] is the metrics-returning half of the
//! seam: a [`LlmMetricsObserver`] additionally hands back a map of extra
//! metrics per successful request, which the engine merges into the step
//! output's `metrics` object — that's how pro builds surface ITL/TPOT
//! percentiles and per-request cost under their own `pro_*` keys without
//! core changes. Engine keys win on collision, so an observer can never
//! clobber the built-in `llm_*` metrics.

use std::sync::{Arc, OnceLock, RwLock};
use std::time::Instant;

use futures_util::StreamExt as _;
use serde_json::{json, Map, Value};
use tokio::time::Duration;

use super::actions::{err, error_chain, ActionOutput, LogTag};
use super::context::Context;
use super::http::{client, ClientPool};
use super::ws::{bool_param, u64_param};

/// `text` is capped at this many characters in the step output.
const TEXT_CAP: usize = 4096;

/// Error bodies are quoted up to this many characters on non-2xx responses.
const ERROR_BODY_CAP: usize = 500;

// ---------------------------------------------------------------------------
// Parameters
// ---------------------------------------------------------------------------

/// The wire protocol of the target endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endpoint {
    /// OpenAI chat completions (and OpenAI-compatible servers).
    OpenAi,
    /// Anthropic messages API.
    Anthropic,
    /// Bring-your-own format: body = `params`, fields pulled via `extract`.
    Generic,
}

impl Endpoint {
    /// The name as used in `with: { endpoint: "…" }`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Endpoint::OpenAi => "openai",
            Endpoint::Anthropic => "anthropic",
            Endpoint::Generic => "generic",
        }
    }
}

/// One chat message from the `messages` array.
#[derive(Debug, Clone)]
pub struct ChatMessage {
    /// `system` / `user` / `assistant` / …
    pub role: String,
    /// Message text.
    pub content: String,
}

/// Resolved `with:` parameters of a `std/llm@v1` step. Public so downstream
/// observer crates can read them.
#[derive(Debug, Clone)]
pub struct LlmParams {
    /// Wire protocol of the target.
    pub endpoint: Endpoint,
    /// Completion endpoint URL.
    pub url: String,
    /// Model name — required for `openai` / `anthropic`, optional otherwise.
    pub model: Option<String>,
    /// Chat messages (`prompt` was already desugared into one user message).
    /// Empty for `generic` steps that carry their payload in `params`.
    pub messages: Vec<ChatMessage>,
    /// Completion token cap (default 256).
    pub max_tokens: u64,
    /// Stream the response as SSE (default true for `openai` / `anthropic`,
    /// false for `generic`).
    pub stream: bool,
    /// API key — sent as `Authorization: Bearer` (`openai` / `generic`) or
    /// `x-api-key` (`anthropic`).
    pub api_key: Option<String>,
    /// Extra request headers (string values only).
    pub headers: Map<String, Value>,
    /// Passthrough body fields (`openai` / `anthropic`), or the entire body
    /// (`generic`).
    pub params: Map<String, Value>,
    /// Field extraction rules — `generic` endpoint only.
    pub extract: Option<ExtractSpec>,
    /// Whole-request timeout in ms (default 120000).
    pub timeout_ms: u64,
}

impl LlmParams {
    fn from_params(params: &Value) -> Result<LlmParams, String> {
        let endpoint = match params["endpoint"].as_str().unwrap_or("openai") {
            "openai" => Endpoint::OpenAi,
            "anthropic" => Endpoint::Anthropic,
            "generic" => Endpoint::Generic,
            other => {
                return Err(format!(
                    "invalid endpoint '{other}' — use \"openai\", \"anthropic\" or \"generic\""
                ));
            }
        };
        let url = params["url"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or("'url' is required")?
            .to_string();
        let model = params["model"].as_str().map(str::to_owned);
        if endpoint != Endpoint::Generic && model.is_none() {
            return Err(format!(
                "'model' is required for endpoint: {}",
                endpoint.as_str()
            ));
        }

        let prompt = params["prompt"].as_str();
        let messages_v = params.get("messages").filter(|v| !v.is_null());
        let messages = match (prompt, messages_v) {
            (Some(_), Some(_)) => {
                return Err("'prompt' and 'messages' are mutually exclusive".into());
            }
            (Some(p), None) => vec![ChatMessage {
                role: "user".into(),
                content: p.to_string(),
            }],
            (None, Some(Value::Array(a))) => a
                .iter()
                .enumerate()
                .map(|(i, m)| {
                    let role = m["role"]
                        .as_str()
                        .ok_or_else(|| format!("'messages[{i}].role' must be a string"))?;
                    let content = m["content"]
                        .as_str()
                        .ok_or_else(|| format!("'messages[{i}].content' must be a string"))?;
                    Ok(ChatMessage {
                        role: role.to_string(),
                        content: content.to_string(),
                    })
                })
                .collect::<Result<Vec<_>, String>>()?,
            (None, Some(_)) => {
                return Err("'messages' must be an array of {role, content} objects".into());
            }
            (None, None) => {
                if endpoint == Endpoint::Generic {
                    Vec::new()
                } else {
                    return Err("one of 'prompt' or 'messages' is required".into());
                }
            }
        };

        let headers = match params.get("headers") {
            None | Some(Value::Null) => Map::new(),
            Some(Value::Object(m)) => m.clone(),
            Some(_) => return Err("'headers' must be an object".into()),
        };
        let extra = match params.get("params") {
            None | Some(Value::Null) => Map::new(),
            Some(Value::Object(m)) => m.clone(),
            Some(_) => return Err("'params' must be an object".into()),
        };
        let extract = match params.get("extract") {
            None | Some(Value::Null) => None,
            Some(Value::Object(m)) => {
                if endpoint != Endpoint::Generic {
                    return Err("'extract' is only supported for endpoint: generic".into());
                }
                Some(ExtractSpec::from_map(m)?)
            }
            Some(_) => return Err("'extract' must be an object".into()),
        };

        Ok(LlmParams {
            endpoint,
            url,
            model,
            messages,
            max_tokens: u64_param(&params["max_tokens"], 256),
            stream: params
                .get("stream")
                .map(bool_param)
                .unwrap_or(endpoint != Endpoint::Generic),
            api_key: params["api_key"].as_str().map(str::to_owned),
            headers,
            params: extra,
            extract,
            timeout_ms: u64_param(&params["timeout_ms"], 120_000),
        })
    }
}

// ---------------------------------------------------------------------------
// extract — dotted paths and single-group regexes (generic endpoint)
// ---------------------------------------------------------------------------

/// One selector from an `extract` value: either a dotted path
/// (`$.usage.completion_tokens`, `$.choices[0].text`) or a regex with exactly
/// one capture group. Public so downstream observer crates can reuse the
/// resolution rules.
#[derive(Debug, Clone)]
pub enum Extractor {
    /// Dotted path over the response JSON.
    Path(Vec<PathSegment>),
    /// Regex applied to the raw response text; group 1 is the value.
    Regex(regex::Regex),
}

/// One step of a dotted path.
#[derive(Debug, Clone)]
pub enum PathSegment {
    /// Object key.
    Key(String),
    /// Array index (`[N]`).
    Index(usize),
}

impl Extractor {
    fn parse(raw: &str) -> Result<Extractor, String> {
        if let Some(rest) = raw.strip_prefix("$.") {
            Ok(Extractor::Path(parse_dotted_path(rest)?))
        } else {
            let re = regex::Regex::new(raw)
                .map_err(|e| format!("invalid extract regex '{raw}': {e}"))?;
            if re.captures_len() != 2 {
                return Err(format!(
                    "extract regex '{raw}' must have exactly one capture group"
                ));
            }
            Ok(Extractor::Regex(re))
        }
    }

    /// Extract a text value: paths resolve against `json`, regexes against
    /// `raw`.
    fn text(&self, json: Option<&Value>, raw: &str) -> Option<String> {
        match self {
            Extractor::Path(segs) => json.and_then(|j| resolve_path(j, segs)).map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            }),
            Extractor::Regex(re) => re
                .captures(raw)
                .and_then(|c| c.get(1))
                .map(|m| m.as_str().to_string()),
        }
    }

    /// Extract a numeric value (token counts).
    fn number(&self, json: Option<&Value>, raw: &str) -> Option<u64> {
        match self {
            Extractor::Path(segs) => json
                .and_then(|j| resolve_path(j, segs))
                .and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f as u64))),
            Extractor::Regex(re) => re
                .captures(raw)
                .and_then(|c| c.get(1))
                .and_then(|m| m.as_str().parse().ok()),
        }
    }
}

/// Parse the part after `$.`: `usage.completion_tokens`, `choices[0].text`.
/// Public so the shared-variable steps reuse the exact same dotted-path
/// syntax for their `extract` parameter.
pub fn parse_dotted_path(s: &str) -> Result<Vec<PathSegment>, String> {
    let mut out = Vec::new();
    for part in s.split('.') {
        if part.is_empty() {
            return Err(format!("invalid extract path '$.{s}': empty segment"));
        }
        let key_len = part.find('[').unwrap_or(part.len());
        if key_len > 0 {
            out.push(PathSegment::Key(part[..key_len].to_string()));
        }
        let mut rest = &part[key_len..];
        while let Some(stripped) = rest.strip_prefix('[') {
            let end = stripped
                .find(']')
                .ok_or_else(|| format!("invalid extract path '$.{s}': unclosed '['"))?;
            let idx: usize = stripped[..end]
                .parse()
                .map_err(|_| format!("invalid extract path '$.{s}': bad index"))?;
            out.push(PathSegment::Index(idx));
            rest = &stripped[end + 1..];
        }
        if !rest.is_empty() {
            return Err(format!("invalid extract path '$.{s}'"));
        }
    }
    if out.is_empty() {
        return Err(format!("invalid extract path '$.{s}': nothing to resolve"));
    }
    Ok(out)
}

/// Walk a parsed dotted path down a JSON value. Public so the
/// shared-variable steps resolve `extract` paths against stored values.
pub fn resolve_path<'v>(mut v: &'v Value, segs: &[PathSegment]) -> Option<&'v Value> {
    for seg in segs {
        v = match seg {
            PathSegment::Key(k) => v.get(k)?,
            PathSegment::Index(i) => v.get(i)?,
        };
    }
    Some(v)
}

/// The `extract` object: where to pull `text`, `prompt_tokens`, and
/// `completion_tokens` from a `generic` endpoint's response. Public so
/// downstream observer crates can read the configured rules.
#[derive(Debug, Clone)]
pub struct ExtractSpec {
    /// Selector for the completion text.
    pub text: Option<Extractor>,
    /// Selector for the prompt token count.
    pub prompt_tokens: Option<Extractor>,
    /// Selector for the completion token count.
    pub completion_tokens: Option<Extractor>,
}

impl ExtractSpec {
    fn from_map(m: &Map<String, Value>) -> Result<ExtractSpec, String> {
        let parse = |key: &str| -> Result<Option<Extractor>, String> {
            match m.get(key) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(s)) => Ok(Some(Extractor::parse(s)?)),
                Some(_) => Err(format!("'extract.{key}' must be a string")),
            }
        };
        Ok(ExtractSpec {
            text: parse("text")?,
            prompt_tokens: parse("prompt_tokens")?,
            completion_tokens: parse("completion_tokens")?,
        })
    }
}

// ---------------------------------------------------------------------------
// Observer seam — per-request samples for downstream (pro) metrics
// ---------------------------------------------------------------------------

/// One completed `std/llm@v1` request, handed to every registered
/// [`LlmObserver`]. Public for downstream pro crates that derive detailed
/// metrics (per-token ITL/TPOT percentiles, cost accounting) from it.
#[derive(Debug, Clone)]
pub struct LlmSample {
    /// Endpoint kind (`openai` / `anthropic` / `generic`).
    pub endpoint: String,
    /// Model name, when the step set one.
    pub model: Option<String>,
    /// Time to first content chunk, ms — `None` for non-streamed requests.
    pub ttft_ms: Option<f64>,
    /// Whole-request wall time, ms.
    pub total_ms: f64,
    /// Prompt tokens as reported by the server.
    pub prompt_tokens: Option<u64>,
    /// Completion tokens as reported by the server.
    pub completion_tokens: Option<u64>,
    /// Arrival deltas between consecutive stream chunks, ms — the first
    /// entry is request-start → first chunk. Empty for non-streamed
    /// requests. Pro crates turn this into ITL/TPOT distributions.
    pub chunk_intervals_ms: Vec<f64>,
    /// Set when the request failed (transport error, non-2xx, stream
    /// break); the token/ttft fields are then empty.
    pub error: Option<String>,
}

/// A consumer of per-request [`LlmSample`]s. Implemented by downstream
/// (proprietary) crates; panics inside [`LlmObserver::on_sample`] are
/// contained and never affect the step.
pub trait LlmObserver: Send + Sync {
    /// Called once per completed request, success or failure.
    fn on_sample(&self, sample: &LlmSample);
}

fn observer_registry() -> &'static RwLock<Vec<Arc<dyn LlmObserver>>> {
    static REGISTRY: OnceLock<RwLock<Vec<Arc<dyn LlmObserver>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| RwLock::new(Vec::new()))
}

/// Register an [`LlmObserver`]. Typically called once at startup by a
/// downstream (proprietary) crate; observers are invoked in registration
/// order for every completed `std/llm@v1` request.
pub fn register_llm_observer(observer: Arc<dyn LlmObserver>) {
    observer_registry().write().unwrap().push(observer);
}

/// Fan a sample out to all observers. A panicking observer is swallowed —
/// observers are instrumentation, they must never fail the step.
fn notify_observers(sample: &LlmSample) {
    let observers: Vec<Arc<dyn LlmObserver>> = observer_registry().read().unwrap().clone();
    for o in observers {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| o.on_sample(sample)));
    }
}

/// A consumer of per-request [`LlmSample`]s that contributes extra metrics
/// to the step output. Implemented by downstream (proprietary) crates —
/// unlike [`LlmObserver`], which only watches, a metrics observer hands back
/// a map that the engine merges into `value["metrics"]` of the successful
/// step (pro crates prefix their keys with `pro_`; engine keys win on
/// collision). Failed requests produce no `metrics` object, so metrics
/// observers are consulted on success only — use [`LlmObserver`] when error
/// samples matter. Panics are contained and never affect the step.
pub trait LlmMetricsObserver: Send + Sync {
    /// Called once per successful request. Return extra metrics to merge
    /// into the step's `metrics` object, or `None` to add nothing.
    fn on_sample_metrics(&self, sample: &LlmSample) -> Option<Map<String, Value>>;
}

fn metrics_observer_registry() -> &'static RwLock<Vec<Arc<dyn LlmMetricsObserver>>> {
    static REGISTRY: OnceLock<RwLock<Vec<Arc<dyn LlmMetricsObserver>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| RwLock::new(Vec::new()))
}

/// Register a [`LlmMetricsObserver`]. Typically called once at startup by a
/// downstream (proprietary) crate; observers are invoked in registration
/// order for every successful `std/llm@v1` request.
pub fn register_llm_metrics_observer(observer: Arc<dyn LlmMetricsObserver>) {
    metrics_observer_registry().write().unwrap().push(observer);
}

/// Collect extra metrics from all metrics observers. A panicking observer is
/// swallowed; between observers the first registration wins a key collision.
fn collect_observer_metrics(sample: &LlmSample) -> Map<String, Value> {
    let observers: Vec<Arc<dyn LlmMetricsObserver>> =
        metrics_observer_registry().read().unwrap().clone();
    let mut out = Map::new();
    for o in observers {
        let caught =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| o.on_sample_metrics(sample)));
        if let Ok(Some(extra)) = caught {
            for (k, v) in extra {
                out.entry(k).or_insert(v);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// SSE decoding
// ---------------------------------------------------------------------------

/// Incremental decoder for server-sent-event `data:` lines. Feed arbitrary
/// byte splits; complete payloads come out, the partial tail stays buffered.
/// `data: [DONE]` is passed through — the caller recognizes it.
#[derive(Default)]
struct SseDecoder {
    buf: Vec<u8>,
}

impl SseDecoder {
    fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.buf.drain(..=pos).collect();
            line.pop(); // '\n'
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if let Some(payload) = Self::data_payload(&line) {
                out.push(payload);
            }
        }
        out
    }

    /// Flush a trailing partial line at end of stream.
    fn finish(&mut self) -> Vec<String> {
        let line = std::mem::take(&mut self.buf);
        Self::data_payload(&line).into_iter().collect()
    }

    fn data_payload(line: &[u8]) -> Option<String> {
        let line = line.strip_prefix(b"data:")?;
        let line = line.strip_prefix(b" ").unwrap_or(line);
        Some(String::from_utf8_lossy(line).into_owned())
    }
}

/// What one SSE chunk carried.
enum ChunkEvent {
    /// A text delta to append.
    Text(String),
    /// Token usage reported by the server.
    Usage {
        prompt: Option<u64>,
        completion: Option<u64>,
    },
}

/// Parse one OpenAI-style SSE chunk: `choices[0].delta.content` plus the
/// final `usage` object (arrives with empty `choices` when
/// `stream_options.include_usage` is set).
fn openai_chunk_events(payload: &str) -> Vec<ChunkEvent> {
    let Ok(v) = serde_json::from_str::<Value>(payload) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if let Some(content) = v["choices"][0]["delta"]["content"].as_str() {
        if !content.is_empty() {
            out.push(ChunkEvent::Text(content.to_string()));
        }
    }
    if let Some(usage) = v.get("usage").filter(|u| !u.is_null()) {
        out.push(ChunkEvent::Usage {
            prompt: usage["prompt_tokens"].as_u64(),
            completion: usage["completion_tokens"].as_u64(),
        });
    }
    out
}

/// Parse one Anthropic SSE event payload: `message_start` carries input
/// tokens, `content_block_delta` the text, `message_delta` the output token
/// count.
fn anthropic_chunk_events(payload: &str) -> Vec<ChunkEvent> {
    let Ok(v) = serde_json::from_str::<Value>(payload) else {
        return Vec::new();
    };
    match v["type"].as_str() {
        Some("content_block_delta") => v["delta"]["text"]
            .as_str()
            .filter(|t| !t.is_empty())
            .map(|t| vec![ChunkEvent::Text(t.to_string())])
            .unwrap_or_default(),
        Some("message_start") => vec![ChunkEvent::Usage {
            prompt: v["message"]["usage"]["input_tokens"].as_u64(),
            completion: None,
        }],
        Some("message_delta") => vec![ChunkEvent::Usage {
            prompt: None,
            completion: v["usage"]["output_tokens"].as_u64(),
        }],
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Exchange
// ---------------------------------------------------------------------------

/// The measured result of one request.
#[derive(Default)]
struct Exchange {
    status: u16,
    text: String,
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    ttft_ms: Option<f64>,
    chunks: u64,
    chunk_intervals_ms: Vec<f64>,
    last_chunk_at: Option<Instant>,
}

impl Exchange {
    /// Record the arrival of one SSE data payload (chunk count + inter-chunk
    /// delta for the observer seam).
    fn note_chunk(&mut self, arrival: Instant, t0: Instant) {
        let anchor = self.last_chunk_at.unwrap_or(t0);
        self.chunk_intervals_ms
            .push((arrival - anchor).as_secs_f64() * 1000.0);
        self.last_chunk_at = Some(arrival);
        self.chunks += 1;
    }

    fn apply(&mut self, event: ChunkEvent, arrival: Instant, t0: Instant) {
        match event {
            ChunkEvent::Text(t) => {
                if self.ttft_ms.is_none() {
                    self.ttft_ms = Some((arrival - t0).as_secs_f64() * 1000.0);
                }
                self.text.push_str(&t);
            }
            ChunkEvent::Usage { prompt, completion } => {
                if prompt.is_some() {
                    self.prompt_tokens = prompt;
                }
                if completion.is_some() {
                    self.completion_tokens = completion;
                }
            }
        }
    }
}

/// Build the request body for the endpoint.
fn build_body(p: &LlmParams) -> Value {
    if p.endpoint == Endpoint::Generic {
        return Value::Object(p.params.clone());
    }
    let messages: Vec<Value> = p
        .messages
        .iter()
        .map(|m| json!({ "role": m.role, "content": m.content }))
        .collect();
    let mut map = Map::new();
    map.insert("model".into(), json!(p.model));
    map.insert("messages".into(), Value::Array(messages));
    map.insert("max_tokens".into(), json!(p.max_tokens));
    map.insert("stream".into(), json!(p.stream));
    if p.endpoint == Endpoint::OpenAi && p.stream {
        map.insert("stream_options".into(), json!({ "include_usage": true }));
    }
    for (k, v) in &p.params {
        map.insert(k.clone(), v.clone());
    }
    Value::Object(map)
}

/// Run one completion request. Errors are hard failures: transport error,
/// non-2xx status, or a broken stream.
async fn run_exchange(p: &LlmParams, ctx: &Context, t0: Instant) -> Result<Exchange, String> {
    let client = client(ClientPool::PerVu, false, ctx.http_client_shard());
    let mut req = client
        .post(&p.url)
        .timeout(Duration::from_millis(p.timeout_ms))
        .json(&build_body(p));

    match (&p.endpoint, &p.api_key) {
        (Endpoint::Anthropic, key) => {
            req = req.header("anthropic-version", "2023-06-01");
            if let Some(key) = key {
                req = req.header("x-api-key", key);
            }
        }
        (_, Some(key)) => req = req.bearer_auth(key),
        _ => {}
    }
    for (k, v) in &p.headers {
        if let Some(s) = v.as_str() {
            req = req.header(k, s);
        }
    }

    let resp = req
        .send()
        .await
        .map_err(|e| format!("POST {}: {}", p.url, error_chain(&e)))?;
    let status = resp.status();
    let mut ex = Exchange {
        status: status.as_u16(),
        ..Default::default()
    };
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        let snippet: String = body.chars().take(ERROR_BODY_CAP).collect();
        return Err(format!("HTTP {}: {snippet}", status.as_u16()));
    }

    if p.stream {
        let mut decoder = SseDecoder::default();
        let mut byte_stream = resp.bytes_stream();
        // Raw text of every data payload — the generic endpoint extracts
        // from it after the stream ends.
        let mut raw = String::new();
        let mut last_json: Option<Value> = None;
        while let Some(item) = byte_stream.next().await {
            let bytes = item.map_err(|e| format!("stream {}: {}", p.url, error_chain(&e)))?;
            let arrival = Instant::now();
            for payload in decoder.feed(&bytes) {
                if payload.trim() == "[DONE]" {
                    continue;
                }
                ex.note_chunk(arrival, t0);
                match p.endpoint {
                    Endpoint::OpenAi => {
                        for ev in openai_chunk_events(&payload) {
                            ex.apply(ev, arrival, t0);
                        }
                    }
                    Endpoint::Anthropic => {
                        for ev in anthropic_chunk_events(&payload) {
                            ex.apply(ev, arrival, t0);
                        }
                    }
                    Endpoint::Generic => {
                        if !raw.is_empty() {
                            raw.push('\n');
                        }
                        raw.push_str(&payload);
                        if let Ok(v) = serde_json::from_str::<Value>(&payload) {
                            last_json = Some(v);
                        }
                    }
                }
            }
        }
        let arrival = Instant::now();
        for payload in decoder.finish() {
            if payload.trim() == "[DONE]" {
                continue;
            }
            ex.note_chunk(arrival, t0);
            if p.endpoint == Endpoint::Generic {
                if !raw.is_empty() {
                    raw.push('\n');
                }
                raw.push_str(&payload);
                if let Ok(v) = serde_json::from_str::<Value>(&payload) {
                    last_json = Some(v);
                }
            }
        }
        if p.endpoint == Endpoint::Generic {
            extract_generic(p, &mut ex, last_json.as_ref(), &raw);
        }
    } else {
        let body = resp
            .text()
            .await
            .map_err(|e| format!("read {}: {}", p.url, error_chain(&e)))?;
        match p.endpoint {
            Endpoint::OpenAi => {
                let v: Value = serde_json::from_str(&body)
                    .map_err(|e| format!("invalid JSON response: {e}"))?;
                if let Some(text) = v["choices"][0]["message"]["content"].as_str() {
                    ex.text = text.to_string();
                }
                ex.prompt_tokens = v["usage"]["prompt_tokens"].as_u64();
                ex.completion_tokens = v["usage"]["completion_tokens"].as_u64();
            }
            Endpoint::Anthropic => {
                let v: Value = serde_json::from_str(&body)
                    .map_err(|e| format!("invalid JSON response: {e}"))?;
                if let Some(text) = v["content"][0]["text"].as_str() {
                    ex.text = text.to_string();
                }
                ex.prompt_tokens = v["usage"]["input_tokens"].as_u64();
                ex.completion_tokens = v["usage"]["output_tokens"].as_u64();
            }
            Endpoint::Generic => {
                let json = serde_json::from_str::<Value>(&body).ok();
                extract_generic(p, &mut ex, json.as_ref(), &body);
            }
        }
    }
    Ok(ex)
}

/// Apply the `extract` rules of a generic step. Without `extract` the whole
/// raw body becomes `text`.
fn extract_generic(p: &LlmParams, ex: &mut Exchange, json: Option<&Value>, raw: &str) {
    match &p.extract {
        Some(spec) => {
            ex.text = spec
                .text
                .as_ref()
                .and_then(|e| e.text(json, raw))
                .unwrap_or_default();
            ex.prompt_tokens = spec
                .prompt_tokens
                .as_ref()
                .and_then(|e| e.number(json, raw));
            ex.completion_tokens = spec
                .completion_tokens
                .as_ref()
                .and_then(|e| e.number(json, raw));
        }
        None => ex.text = raw.to_string(),
    }
}

/// completion_tokens / generation time. With a TTFT the generation window is
/// "after the first token"; without one the whole request time is used.
fn tokens_per_sec(ex: &Exchange, duration_ms: f64) -> Option<f64> {
    let completion = ex.completion_tokens?;
    let gen_ms = match ex.ttft_ms {
        Some(ttft) if duration_ms - ttft > 0.0 => duration_ms - ttft,
        _ => duration_ms,
    };
    if gen_ms <= 0.0 {
        return None;
    }
    Some(completion as f64 / (gen_ms / 1000.0))
}

// ---------------------------------------------------------------------------
// std/llm@v1
// ---------------------------------------------------------------------------

pub(crate) async fn llm_action(params: &Value, ctx: &Context, step_name: &str) -> ActionOutput {
    let parsed = match LlmParams::from_params(params) {
        Ok(p) => p,
        Err(msg) => return err(step_name, msg.as_str()),
    };

    let t0 = Instant::now();
    let result = run_exchange(&parsed, ctx, t0).await;
    let duration_ms = t0.elapsed().as_secs_f64() * 1000.0;

    let ex = match result {
        Ok(ex) => ex,
        Err(msg) => {
            notify_observers(&LlmSample {
                endpoint: parsed.endpoint.as_str().to_string(),
                model: parsed.model.clone(),
                ttft_ms: None,
                total_ms: duration_ms,
                prompt_tokens: None,
                completion_tokens: None,
                chunk_intervals_ms: Vec::new(),
                error: Some(msg.clone()),
            });
            return ActionOutput {
                value: json!({
                    "endpoint": parsed.endpoint.as_str(),
                    "model": parsed.model,
                    "error": msg,
                    "duration_ms": duration_ms,
                }),
                logs: vec![(
                    LogTag::Err,
                    format!(
                        "{step_name}: LLM {} {}: {msg}",
                        parsed.endpoint.as_str(),
                        parsed.url
                    ),
                )],
                success: false,
                http_sample: None,
            };
        }
    };

    let sample = LlmSample {
        endpoint: parsed.endpoint.as_str().to_string(),
        model: parsed.model.clone(),
        ttft_ms: ex.ttft_ms,
        total_ms: duration_ms,
        prompt_tokens: ex.prompt_tokens,
        completion_tokens: ex.completion_tokens,
        chunk_intervals_ms: ex.chunk_intervals_ms.clone(),
        error: None,
    };
    notify_observers(&sample);
    // Metrics observers (pro seam) get the same sample and contribute extra
    // `pro_*` metrics; engine keys win collisions.
    let observer_metrics = collect_observer_metrics(&sample);

    let tps = tokens_per_sec(&ex, duration_ms);
    let text: String = ex.text.chars().take(TEXT_CAP).collect();
    let mut value = json!({
        "endpoint": parsed.endpoint.as_str(),
        "status": ex.status,
        "duration_ms": duration_ms,
        "chunks": ex.chunks,
        "text": text,
    });
    if let Some(model) = &parsed.model {
        value["model"] = json!(model);
    }
    if let Some(ttft) = ex.ttft_ms {
        value["ttft_ms"] = json!(ttft);
    }
    if let Some(p) = ex.prompt_tokens {
        value["prompt_tokens"] = json!(p);
    }
    if let Some(c) = ex.completion_tokens {
        value["completion_tokens"] = json!(c);
    }
    if let Some(t) = tps {
        value["tokens_per_sec"] = json!(t);
    }

    // Latency/throughput ride as single-sample arrays (histograms), token
    // counts and chunks as plain counters — the runner's `metrics` rules.
    let mut metrics = json!({ "llm_chunks": ex.chunks });
    if let Some(ttft) = ex.ttft_ms {
        metrics["llm_ttft_ms"] = json!([ttft]);
    }
    if let Some(t) = tps {
        metrics["llm_tokens_per_sec"] = json!([t]);
    }
    if let Some(p) = ex.prompt_tokens {
        metrics["llm_prompt_tokens"] = json!(p);
    }
    if let Some(c) = ex.completion_tokens {
        metrics["llm_completion_tokens"] = json!(c);
    }
    for (k, v) in observer_metrics {
        // Engine metrics win: an observer can add `pro_*` keys but never
        // clobber the built-in `llm_*` ones.
        metrics.as_object_mut().unwrap().entry(k).or_insert(v);
    }
    value["metrics"] = metrics;

    let model = parsed.model.as_deref().unwrap_or("-");
    let mut line = format!(
        "LLM {} {} → {}, {} chunks",
        parsed.endpoint.as_str(),
        model,
        ex.status,
        ex.chunks
    );
    if let Some(ttft) = ex.ttft_ms {
        line += &format!(", ttft {ttft:.2}ms");
    }
    if let Some(t) = tps {
        line += &format!(", {t:.1} tok/s");
    }
    line += &format!(" ({duration_ms:.2}ms)");

    ActionOutput {
        value,
        logs: vec![(LogTag::Out, line)],
        success: true,
        http_sample: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::step::actions::execute_action;
    use crate::step::context::Context;

    // -----------------------------------------------------------------
    // Parameter parsing
    // -----------------------------------------------------------------

    #[test]
    fn params_openai_defaults_and_prompt_sugar() {
        let p = LlmParams::from_params(&json!({
            "url": "http://localhost:11434/v1/chat/completions",
            "model": "llama3",
            "prompt": "say hi",
        }))
        .unwrap();
        assert_eq!(p.endpoint, Endpoint::OpenAi);
        assert_eq!(p.max_tokens, 256);
        assert!(p.stream);
        assert_eq!(p.timeout_ms, 120_000);
        assert!(p.api_key.is_none());
        assert_eq!(p.messages.len(), 1);
        assert_eq!(p.messages[0].role, "user");
        assert_eq!(p.messages[0].content, "say hi");
    }

    #[test]
    fn params_messages_array() {
        let p = LlmParams::from_params(&json!({
            "url": "http://x/",
            "model": "m",
            "messages": [
                { "role": "system", "content": "be brief" },
                { "role": "user", "content": "hi" },
            ],
            "max_tokens": 64,
            "stream": false,
            "timeout_ms": 5000,
            "params": { "temperature": 0.2 },
        }))
        .unwrap();
        assert_eq!(p.messages.len(), 2);
        assert_eq!(p.messages[0].role, "system");
        assert_eq!(p.max_tokens, 64);
        assert!(!p.stream);
        assert_eq!(p.timeout_ms, 5000);
        assert_eq!(p.params["temperature"], json!(0.2));
    }

    #[test]
    fn params_generic_defaults_unstreamed() {
        let p = LlmParams::from_params(&json!({
            "endpoint": "generic",
            "url": "http://x/predict",
            "params": { "inputs": "hi" },
        }))
        .unwrap();
        assert_eq!(p.endpoint, Endpoint::Generic);
        assert!(!p.stream);
        assert!(p.messages.is_empty());
        assert!(p.model.is_none());
    }

    #[tokio::test]
    async fn missing_url_is_rejected() {
        let out = execute_action(
            "std/llm@v1",
            &json!({ "model": "m", "prompt": "hi" }),
            &Context::new(),
            "step",
        )
        .await;
        assert!(!out.success);
        assert!(
            out.logs[0].1.contains("'url' is required"),
            "{:?}",
            out.logs
        );
    }

    #[tokio::test]
    async fn missing_model_is_rejected_for_openai_and_anthropic() {
        for endpoint in ["openai", "anthropic"] {
            let out = execute_action(
                "std/llm@v1",
                &json!({ "endpoint": endpoint, "url": "http://x/", "prompt": "hi" }),
                &Context::new(),
                "step",
            )
            .await;
            assert!(!out.success);
            assert!(
                out.logs[0].1.contains("'model' is required"),
                "{endpoint}: {:?}",
                out.logs
            );
        }
    }

    #[tokio::test]
    async fn neither_prompt_nor_messages_is_rejected() {
        let out = execute_action(
            "std/llm@v1",
            &json!({ "url": "http://x/", "model": "m" }),
            &Context::new(),
            "step",
        )
        .await;
        assert!(!out.success);
        assert!(
            out.logs[0].1.contains("one of 'prompt' or 'messages'"),
            "{:?}",
            out.logs
        );
    }

    #[test]
    fn prompt_and_messages_are_mutually_exclusive() {
        let e = LlmParams::from_params(&json!({
            "url": "http://x/",
            "model": "m",
            "prompt": "hi",
            "messages": [{ "role": "user", "content": "hi" }],
        }))
        .unwrap_err();
        assert!(e.contains("mutually exclusive"), "{e}");
    }

    #[test]
    fn invalid_endpoint_is_rejected() {
        let e = LlmParams::from_params(&json!({
            "endpoint": "gemini",
            "url": "http://x/",
        }))
        .unwrap_err();
        assert!(e.contains("invalid endpoint 'gemini'"), "{e}");
    }

    #[test]
    fn malformed_message_is_rejected() {
        let e = LlmParams::from_params(&json!({
            "url": "http://x/",
            "model": "m",
            "messages": [{ "role": "user" }],
        }))
        .unwrap_err();
        assert!(e.contains("'messages[0].content' must be a string"), "{e}");
    }

    #[test]
    fn extract_is_generic_only() {
        let e = LlmParams::from_params(&json!({
            "url": "http://x/",
            "model": "m",
            "prompt": "hi",
            "extract": { "text": "$.text" },
        }))
        .unwrap_err();
        assert!(e.contains("only supported for endpoint: generic"), "{e}");
    }

    #[test]
    fn invalid_extract_rules_are_rejected() {
        let cases = [
            json!({ "text": "$." }),            // empty path
            json!({ "text": "$.choices[x]" }),  // bad index
            json!({ "text": "$.a..b" }),        // empty segment
            json!({ "text": "no group here" }), // regex without a capture group
            json!({ "text": "(unclosed" }),     // invalid regex
            json!({ "text": 42 }),              // not a string
        ];
        for extract in cases {
            let r = LlmParams::from_params(&json!({
                "endpoint": "generic",
                "url": "http://x/",
                "extract": extract,
            }));
            assert!(r.is_err(), "extract {extract} must be rejected");
        }
    }

    // -----------------------------------------------------------------
    // Dotted paths and extractors
    // -----------------------------------------------------------------

    #[test]
    fn dotted_path_resolves_keys_and_indices() {
        let v = json!({
            "usage": { "completion_tokens": 42 },
            "choices": [{ "text": "hello" }],
        });
        let text = Extractor::parse("$.choices[0].text").unwrap();
        assert_eq!(text.text(Some(&v), ""), Some("hello".to_string()));
        let n = Extractor::parse("$.usage.completion_tokens").unwrap();
        assert_eq!(n.number(Some(&v), ""), Some(42));
        // Missing key / out-of-range index resolve to None, not a panic.
        let missing = Extractor::parse("$.choices[5].text").unwrap();
        assert_eq!(missing.text(Some(&v), ""), None);
    }

    #[test]
    fn regex_extractor_uses_capture_group_one() {
        let text = Extractor::parse(r"output: (.+)$").unwrap();
        assert_eq!(
            text.text(None, "noise\noutput: hello world"),
            Some("hello world".to_string())
        );
        let n = Extractor::parse(r"(\d+) tokens").unwrap();
        assert_eq!(n.number(None, "used 17 tokens"), Some(17));
    }

    // -----------------------------------------------------------------
    // SSE decoding and chunk parsers
    // -----------------------------------------------------------------

    #[test]
    fn sse_decoder_handles_buffer_boundaries() {
        let mut d = SseDecoder::default();
        assert!(d.feed(br#"data: {"a""#).is_empty());
        let got = d.feed(b": 1}\n\ndata: [DONE]\n");
        assert_eq!(got, vec![r#"{"a": 1}"#, "[DONE]"]);
        assert!(d.buf.is_empty());
    }

    #[test]
    fn sse_decoder_handles_crlf_and_trailing_partial_line() {
        let mut d = SseDecoder::default();
        let got = d.feed(b"data: one\r\ndata: two\r\n");
        assert_eq!(got, vec!["one", "two"]);
        assert!(d.feed(b"data: tail").is_empty());
        assert_eq!(d.finish(), vec!["tail"]);
    }

    #[test]
    fn sse_decoder_ignores_non_data_lines() {
        let mut d = SseDecoder::default();
        let got = d.feed(b": keepalive\n\nevent: message\ndata: real\n");
        assert_eq!(got, vec!["real"]);
    }

    #[test]
    fn openai_chunk_parses_delta_and_usage() {
        let evs = openai_chunk_events(r#"{"choices":[{"delta":{"content":"Hel"}}]}"#);
        assert!(matches!(&evs[0], ChunkEvent::Text(t) if t == "Hel"));

        // Final usage chunk: empty choices + usage object.
        let evs = openai_chunk_events(
            r#"{"choices":[],"usage":{"prompt_tokens":12,"completion_tokens":7}}"#,
        );
        assert!(matches!(
            &evs[0],
            ChunkEvent::Usage {
                prompt: Some(12),
                completion: Some(7)
            }
        ));

        // Mid-stream chunks carry "usage": null — ignored.
        assert!(openai_chunk_events(r#"{"choices":[{"delta":{}}],"usage":null}"#).is_empty());
        // Non-JSON payloads are ignored, never fatal.
        assert!(openai_chunk_events("not json").is_empty());
    }

    #[test]
    fn anthropic_chunk_parses_events() {
        let evs = anthropic_chunk_events(
            r#"{"type":"message_start","message":{"usage":{"input_tokens":25,"output_tokens":1}}}"#,
        );
        assert!(matches!(
            &evs[0],
            ChunkEvent::Usage {
                prompt: Some(25),
                completion: None
            }
        ));

        let evs = anthropic_chunk_events(
            r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"Hi"}}"#,
        );
        assert!(matches!(&evs[0], ChunkEvent::Text(t) if t == "Hi"));

        let evs =
            anthropic_chunk_events(r#"{"type":"message_delta","usage":{"output_tokens":15}}"#);
        assert!(matches!(
            &evs[0],
            ChunkEvent::Usage {
                prompt: None,
                completion: Some(15)
            }
        ));

        // ping / message_stop / unknown events carry nothing.
        assert!(anthropic_chunk_events(r#"{"type":"ping"}"#).is_empty());
        assert!(anthropic_chunk_events("not json").is_empty());
    }

    // -----------------------------------------------------------------
    // Integration — mock servers on axum
    // -----------------------------------------------------------------

    use std::sync::Mutex;

    /// Spin up a mock server on an ephemeral port; returns its base URL.
    async fn serve(app: axum::Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    /// An SSE response whose chunks go out with real delays between them.
    fn sse_response(chunks: Vec<String>, delay_ms: u64) -> axum::response::Response {
        use axum::response::IntoResponse as _;
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<String, std::convert::Infallible>>(
            chunks.len() + 2,
        );
        tokio::spawn(async move {
            for c in chunks {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                if tx.send(Ok(format!("data: {c}\n\n"))).await.is_err() {
                    return;
                }
            }
            let _ = tx.send(Ok("data: [DONE]\n\n".to_string())).await;
        });
        (
            [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
            axum::body::Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx)),
        )
            .into_response()
    }

    #[derive(Clone, Default)]
    struct Capture {
        headers: Arc<Mutex<Vec<(String, String)>>>,
        bodies: Arc<Mutex<Vec<Value>>>,
    }

    impl Capture {
        fn headers(&self) -> Vec<(String, String)> {
            self.headers.lock().unwrap().clone()
        }
        fn bodies(&self) -> Vec<Value> {
            self.bodies.lock().unwrap().clone()
        }
    }

    fn openai_chunks() -> Vec<String> {
        vec![
            r#"{"choices":[{"delta":{"content":"Hello"}}],"usage":null}"#.to_string(),
            r#"{"choices":[{"delta":{"content":", world"}}],"usage":null}"#.to_string(),
            r#"{"choices":[{"delta":{"content":"!"}}],"usage":null}"#.to_string(),
            r#"{"choices":[],"usage":{"prompt_tokens":12,"completion_tokens":3,"total_tokens":15}}"#
                .to_string(),
        ]
    }

    #[tokio::test]
    async fn openai_streaming_measures_ttft_and_usage() {
        let cap = Capture::default();
        let state = cap.clone();
        let app = axum::Router::new().route(
            "/v1/chat/completions",
            axum::routing::post(
                move |headers: axum::http::HeaderMap, body: axum::body::Bytes| {
                    let state = state.clone();
                    async move {
                        for (k, v) in headers.iter() {
                            state.headers.lock().unwrap().push((
                                k.as_str().to_string(),
                                v.to_str().unwrap_or("").to_string(),
                            ));
                        }
                        state
                            .bodies
                            .lock()
                            .unwrap()
                            .push(serde_json::from_slice::<Value>(&body).unwrap());
                        sse_response(openai_chunks(), 25)
                    }
                },
            ),
        );
        let base = serve(app).await;
        let out = execute_action(
            "std/llm@v1",
            &json!({
                "url": format!("{base}/v1/chat/completions"),
                "model": "gpt-test",
                "prompt": "hi",
                "api_key": "sk-test",
                "params": { "temperature": 0.0 },
            }),
            &Context::new(),
            "chat",
        )
        .await;
        assert!(out.success, "{:?}", out.logs);
        assert_eq!(out.value["status"], 200);
        assert_eq!(out.value["text"], "Hello, world!");
        assert_eq!(out.value["prompt_tokens"], 12);
        assert_eq!(out.value["completion_tokens"], 3);
        assert_eq!(out.value["chunks"], 4);
        // TTFT = first content chunk; must sit strictly inside the total.
        let ttft = out.value["ttft_ms"].as_f64().unwrap();
        let total = out.value["duration_ms"].as_f64().unwrap();
        assert!(ttft > 0.0 && ttft < total, "ttft {ttft} total {total}");
        assert!(out.value["tokens_per_sec"].as_f64().unwrap() > 0.0);
        // Metrics channel: histograms for latency/throughput, counters for tokens.
        assert!(out.value["metrics"]["llm_ttft_ms"].is_array());
        assert!(out.value["metrics"]["llm_tokens_per_sec"].is_array());
        assert_eq!(out.value["metrics"]["llm_prompt_tokens"], 12);
        assert_eq!(out.value["metrics"]["llm_completion_tokens"], 3);
        assert_eq!(out.value["metrics"]["llm_chunks"], 4);

        // Server side: bearer auth and the body shape (stream + include_usage
        // + params passthrough).
        let headers = cap.headers();
        let auth = headers
            .iter()
            .find(|(k, _)| k == "authorization")
            .map(|(_, v)| v.as_str());
        assert_eq!(auth, Some("Bearer sk-test"), "{headers:?}");
        let body = &cap.bodies()[0];
        assert_eq!(body["model"], "gpt-test");
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert_eq!(body["temperature"], 0.0);
        assert_eq!(body["messages"][0]["content"], "hi");
    }

    #[tokio::test]
    async fn api_key_from_env_placeholder_reaches_the_header_not_the_logs() {
        std::env::set_var("PERFSCALE_TEST_LLM_ENV_KEY", "sk-env-secret");
        let cap = Capture::default();
        let state = cap.clone();
        let app = axum::Router::new().route(
            "/v1/chat/completions",
            axum::routing::post(
                move |headers: axum::http::HeaderMap, body: axum::body::Bytes| {
                    let state = state.clone();
                    async move {
                        for (k, v) in headers.iter() {
                            state.headers.lock().unwrap().push((
                                k.as_str().to_string(),
                                v.to_str().unwrap_or("").to_string(),
                            ));
                        }
                        state
                            .bodies
                            .lock()
                            .unwrap()
                            .push(serde_json::from_slice::<Value>(&body).unwrap());
                        sse_response(openai_chunks(), 1)
                    }
                },
            ),
        );
        let base = serve(app).await;
        let out = execute_action(
            "std/llm@v1",
            &json!({
                "url": format!("{base}/v1/chat/completions"),
                "model": "gpt-test",
                "prompt": "hi",
                "api_key": "${{ env.PERFSCALE_TEST_LLM_ENV_KEY }}",
            }),
            &Context::new(),
            "chat",
        )
        .await;
        assert!(out.success, "{:?}", out.logs);

        // The env value reached the Authorization header…
        let headers = cap.headers();
        let auth = headers
            .iter()
            .find(|(k, _)| k == "authorization")
            .map(|(_, v)| v.as_str());
        assert_eq!(auth, Some("Bearer sk-env-secret"), "{headers:?}");
        // …and never leaked into a log line or the step output.
        for (_, line) in &out.logs {
            assert!(!line.contains("sk-env-secret"), "secret in log: {line}");
        }
        assert!(
            !out.value.to_string().contains("sk-env-secret"),
            "secret in output value"
        );
        std::env::remove_var("PERFSCALE_TEST_LLM_ENV_KEY");
    }

    #[tokio::test]
    async fn missing_env_var_fails_the_step_before_any_request() {
        std::env::remove_var("PERFSCALE_TEST_LLM_ENV_UNSET");
        let out = execute_action(
            "std/llm@v1",
            &json!({
                "url": "http://127.0.0.1:1/v1/chat/completions",
                "model": "gpt-test",
                "prompt": "hi",
                "api_key": "${{ env.PERFSCALE_TEST_LLM_ENV_UNSET }}",
            }),
            &Context::new(),
            "chat",
        )
        .await;
        assert!(!out.success);
        assert!(
            out.logs[0]
                .1
                .contains("env var 'PERFSCALE_TEST_LLM_ENV_UNSET' is not set"),
            "{:?}",
            out.logs
        );
        // Fail-fast: no HTTP sample was recorded (no request ever went out).
        assert!(out.http_sample.is_none(), "{:?}", out.logs);
    }

    #[tokio::test]
    async fn openai_non_streaming_has_no_ttft() {
        let app = axum::Router::new().route(
            "/v1/chat/completions",
            axum::routing::post(|| async {
                axum::Json(json!({
                    "choices": [{ "message": { "role": "assistant", "content": "one-shot answer" } }],
                    "usage": { "prompt_tokens": 5, "completion_tokens": 9, "total_tokens": 14 },
                }))
            }),
        );
        let base = serve(app).await;
        let out = execute_action(
            "std/llm@v1",
            &json!({
                "url": format!("{base}/v1/chat/completions"),
                "model": "gpt-test",
                "prompt": "hi",
                "stream": false,
            }),
            &Context::new(),
            "chat",
        )
        .await;
        assert!(out.success, "{:?}", out.logs);
        assert_eq!(out.value["text"], "one-shot answer");
        assert!(out.value.get("ttft_ms").is_none());
        assert_eq!(out.value["chunks"], 0);
        assert_eq!(out.value["completion_tokens"], 9);
        // No TTFT → throughput is computed over the whole request time.
        assert!(out.value["tokens_per_sec"].as_f64().unwrap() > 0.0);
        assert!(out.value["metrics"].get("llm_ttft_ms").is_none());
    }

    #[tokio::test]
    async fn anthropic_streaming_events_and_headers() {
        let cap = Capture::default();
        let state = cap.clone();
        let app = axum::Router::new().route(
            "/v1/messages",
            axum::routing::post(move |headers: axum::http::HeaderMap| {
                let state = state.clone();
                async move {
                    for (k, v) in headers.iter() {
                        state.headers.lock().unwrap().push((
                            k.as_str().to_string(),
                            v.to_str().unwrap_or("").to_string(),
                        ));
                    }
                    sse_response(
                        vec![
                            r#"{"type":"message_start","message":{"usage":{"input_tokens":25,"output_tokens":1}}}"#.to_string(),
                            r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"Hi"}}"#.to_string(),
                            r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":" there"}}"#.to_string(),
                            r#"{"type":"message_delta","usage":{"output_tokens":15}}"#.to_string(),
                            r#"{"type":"message_stop"}"#.to_string(),
                        ],
                        10,
                    )
                }
            }),
        );
        let base = serve(app).await;
        let out = execute_action(
            "std/llm@v1",
            &json!({
                "endpoint": "anthropic",
                "url": format!("{base}/v1/messages"),
                "model": "claude-test",
                "prompt": "hi",
                "api_key": "ant-key",
            }),
            &Context::new(),
            "claude",
        )
        .await;
        assert!(out.success, "{:?}", out.logs);
        assert_eq!(out.value["text"], "Hi there");
        assert_eq!(out.value["prompt_tokens"], 25);
        assert_eq!(out.value["completion_tokens"], 15);
        assert!(out.value["ttft_ms"].as_f64().is_some());

        let headers = cap.headers();
        let get = |name: &str| {
            headers
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("x-api-key"), Some("ant-key"), "{headers:?}");
        assert_eq!(get("anthropic-version"), Some("2023-06-01"), "{headers:?}");
        assert!(get("authorization").is_none(), "{headers:?}");
    }

    #[tokio::test]
    async fn generic_dotted_path_extract() {
        let cap = Capture::default();
        let state = cap.clone();
        let app = axum::Router::new().route(
            "/predict",
            axum::routing::post(move |body: axum::body::Bytes| {
                let state = state.clone();
                async move {
                    state
                        .bodies
                        .lock()
                        .unwrap()
                        .push(serde_json::from_slice::<Value>(&body).unwrap());
                    axum::Json(json!({
                        "choices": [{ "text": "generic answer" }],
                        "usage": { "prompt_tokens": 5, "completion_tokens": 7 },
                    }))
                }
            }),
        );
        let base = serve(app).await;
        let out = execute_action(
            "std/llm@v1",
            &json!({
                "endpoint": "generic",
                "url": format!("{base}/predict"),
                "params": { "inputs": "hi", "max_new_tokens": 16 },
                "extract": {
                    "text": "$.choices[0].text",
                    "prompt_tokens": "$.usage.prompt_tokens",
                    "completion_tokens": "$.usage.completion_tokens",
                },
            }),
            &Context::new(),
            "generic",
        )
        .await;
        assert!(out.success, "{:?}", out.logs);
        assert_eq!(out.value["text"], "generic answer");
        assert_eq!(out.value["prompt_tokens"], 5);
        assert_eq!(out.value["completion_tokens"], 7);
        assert!(out.value.get("model").is_none());
        // The generic body IS the params object, verbatim.
        assert_eq!(
            cap.bodies()[0],
            json!({ "inputs": "hi", "max_new_tokens": 16 })
        );
    }

    #[tokio::test]
    async fn generic_regex_extract() {
        let app = axum::Router::new().route(
            "/predict",
            axum::routing::post(|| async {
                "result: the answer is 42\nprompt_tokens: 11\ncompletion_tokens: 13"
            }),
        );
        let base = serve(app).await;
        let out = execute_action(
            "std/llm@v1",
            &json!({
                "endpoint": "generic",
                "url": format!("{base}/predict"),
                "extract": {
                    "text": "result: (.+)",
                    "prompt_tokens": "prompt_tokens: (\\d+)",
                    "completion_tokens": "completion_tokens: (\\d+)",
                },
            }),
            &Context::new(),
            "generic",
        )
        .await;
        assert!(out.success, "{:?}", out.logs);
        assert_eq!(out.value["text"], "the answer is 42");
        assert_eq!(out.value["prompt_tokens"], 11);
        assert_eq!(out.value["completion_tokens"], 13);
        assert!(out.value["tokens_per_sec"].as_f64().unwrap() > 0.0);
    }

    #[tokio::test]
    async fn non_2xx_fails_with_status_and_body_snippet() {
        let app = axum::Router::new().route(
            "/v1/chat/completions",
            axum::routing::post(|| async {
                (
                    axum::http::StatusCode::TOO_MANY_REQUESTS,
                    "rate limit exceeded: try again later",
                )
            }),
        );
        let base = serve(app).await;
        let out = execute_action(
            "std/llm@v1",
            &json!({
                "url": format!("{base}/v1/chat/completions"),
                "model": "gpt-test",
                "prompt": "hi",
            }),
            &Context::new(),
            "chat",
        )
        .await;
        assert!(!out.success);
        let err = out.value["error"].as_str().unwrap();
        assert!(err.contains("HTTP 429"), "{err}");
        assert!(err.contains("rate limit exceeded"), "{err}");
        assert!(out.logs.iter().any(|(tag, _)| *tag == LogTag::Err));
    }

    // -----------------------------------------------------------------
    // Observer seam
    // -----------------------------------------------------------------

    struct CollectObserver {
        samples: Mutex<Vec<LlmSample>>,
    }

    impl LlmObserver for CollectObserver {
        fn on_sample(&self, sample: &LlmSample) {
            self.samples.lock().unwrap().push(sample.clone());
        }
    }

    struct PanicObserver;

    impl LlmObserver for PanicObserver {
        fn on_sample(&self, _sample: &LlmSample) {
            panic!("observer must never break the step");
        }
    }

    #[tokio::test]
    async fn observers_get_success_and_error_samples() {
        let collected = Arc::new(CollectObserver {
            samples: Mutex::new(Vec::new()),
        });
        register_llm_observer(collected.clone());
        register_llm_observer(Arc::new(PanicObserver));

        // Success sample with chunk intervals.
        let ok_model = format!("ok-{}", uuid::Uuid::new_v4());
        let app = axum::Router::new().route(
            "/v1/chat/completions",
            axum::routing::post(move || {
                let chunks = openai_chunks();
                async move { sse_response(chunks, 5) }
            }),
        );
        let base = serve(app).await;
        let out = execute_action(
            "std/llm@v1",
            &json!({
                "url": format!("{base}/v1/chat/completions"),
                "model": ok_model,
                "prompt": "hi",
            }),
            &Context::new(),
            "chat",
        )
        .await;
        // The panicking observer did not affect the step.
        assert!(out.success, "{:?}", out.logs);

        // Error sample on a non-2xx.
        let err_model = format!("err-{}", uuid::Uuid::new_v4());
        let out = execute_action(
            "std/llm@v1",
            &json!({
                "url": format!("{base}/nowhere"),
                "model": err_model,
                "prompt": "hi",
            }),
            &Context::new(),
            "chat",
        )
        .await;
        assert!(!out.success);

        let samples = collected.samples.lock().unwrap();
        let ok = samples
            .iter()
            .find(|s| s.model.as_deref() == Some(ok_model.as_str()))
            .expect("success sample recorded");
        assert_eq!(ok.endpoint, "openai");
        assert!(ok.error.is_none());
        assert_eq!(ok.chunk_intervals_ms.len(), 4, "{ok:?}");
        assert!(ok.ttft_ms.is_some());
        assert_eq!(ok.completion_tokens, Some(3));

        let err = samples
            .iter()
            .find(|s| s.model.as_deref() == Some(err_model.as_str()))
            .expect("error sample recorded");
        assert!(
            err.error.as_deref().is_some_and(|e| e.contains("HTTP 404")),
            "{err:?}"
        );
        assert!(err.chunk_intervals_ms.is_empty());
    }

    // -----------------------------------------------------------------
    // Metrics observer seam (pro metrics merged into the step output)
    // -----------------------------------------------------------------

    struct ProMetricsObserver;

    impl LlmMetricsObserver for ProMetricsObserver {
        fn on_sample_metrics(&self, sample: &LlmSample) -> Option<Map<String, Value>> {
            let mut m = Map::new();
            m.insert("pro_test_samples".into(), json!(1));
            m.insert(
                "pro_test_itl_avg_ms".into(),
                json!([sample.chunk_intervals_ms.iter().sum::<f64>()
                    / sample.chunk_intervals_ms.len().max(1) as f64]),
            );
            // Collision attempt: the engine's own key must survive.
            m.insert("llm_chunks".into(), json!(999));
            Some(m)
        }
    }

    struct PanicMetricsObserver;

    impl LlmMetricsObserver for PanicMetricsObserver {
        fn on_sample_metrics(&self, _sample: &LlmSample) -> Option<Map<String, Value>> {
            panic!("metrics observer must never break the step");
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn metrics_observers_merge_into_step_metrics() {
        register_llm_metrics_observer(Arc::new(ProMetricsObserver));
        register_llm_metrics_observer(Arc::new(PanicMetricsObserver));

        let model = format!("pro-{}", uuid::Uuid::new_v4());
        let app = axum::Router::new().route(
            "/v1/chat/completions",
            axum::routing::post(move || {
                let chunks = openai_chunks();
                async move { sse_response(chunks, 5) }
            }),
        );
        let base = serve(app).await;
        let out = execute_action(
            "std/llm@v1",
            &json!({
                "url": format!("{base}/v1/chat/completions"),
                "model": model,
                "prompt": "hi",
            }),
            &Context::new(),
            "chat",
        )
        .await;
        // The panicking observer did not affect the step.
        assert!(out.success, "{:?}", out.logs);

        // Observer metrics landed under their own keys…
        assert_eq!(out.value["metrics"]["pro_test_samples"], 1);
        assert!(
            out.value["metrics"]["pro_test_itl_avg_ms"]
                .as_array()
                .is_some_and(|a| a.len() == 1 && a[0].as_f64().unwrap() > 0.0),
            "{:?}",
            out.value["metrics"]
        );
        // …and the engine's own metric survived the collision attempt.
        assert_eq!(out.value["metrics"]["llm_chunks"], 4);

        // Failed steps carry no metrics object, so metrics observers are not
        // consulted on errors (plain LlmObserver still gets the sample).
        let out = execute_action(
            "std/llm@v1",
            &json!({
                "url": format!("{base}/nowhere"),
                "model": model,
                "prompt": "hi",
            }),
            &Context::new(),
            "chat",
        )
        .await;
        assert!(!out.success);
        assert!(out.value.get("metrics").is_none(), "{:?}", out.value);
    }
}
