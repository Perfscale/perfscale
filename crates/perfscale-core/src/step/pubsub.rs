//! Pub/sub action: `std/pubsub@v1` — publish messages to a subject and/or
//! wait for messages on it, measuring publish and end-to-end latency.
//!
//! One step = one exchange: (optional) subscribe → (optional) publish →
//! (optional) wait for `count` matching messages, bounded by a timeout. When
//! both `publish` and `subscribe` are given the subscription is established
//! FIRST, so a same-subject roundtrip sees its own messages.
//!
//! # Drivers
//!
//! The transport is pluggable behind [`PubSubDriver`]; this crate ships:
//!
//! | Driver   | Transport                                             |
//! |----------|-------------------------------------------------------|
//! | `memory` | In-process broadcast bus (default) — no broker needed |
//! | `nats`   | A real NATS server via the `async-nats` crate         |
//!
//! Proprietary drivers (Kafka, Redis, …) live in closed crates and register
//! themselves via [`register_pubsub_driver`] at process start — the same
//! extension posture as [`super::actions::register_action`]. An unknown
//! `driver` value fails the step with the list of registered drivers, which
//! is how a user learns their build lacks the pro crate.
//!
//! The `memory` bus is **process-global**: one channel per subject, shared by
//! every VU in the process. A message one VU publishes is delivered to every
//! other VU subscribed to the same subject — cross-VU messaging is a feature
//! (it models fan-out without a broker), not leakage.
//!
//! # Parameters
//!
//! | Parameter    | Type            | Default    | Description |
//! |--------------|-----------------|------------|-------------|
//! | `driver`     | string          | `"memory"` | `memory` or `nats` (plus any registered downstream drivers) |
//! | `subject`    | string          | required   | NATS subject / in-memory topic name |
//! | `url`        | string          | —          | Broker URL; required by `nats`, ignored by `memory` |
//! | `publish`    | string \| array | —          | One message, or a list; non-strings are serialized to JSON text |
//! | `subscribe`  | object          | —          | `{ count, until_contains, timeout_ms }` — wait for `count` messages (default 1) that each contain `until_contains` (optional), within `timeout_ms` (default 5000) |
//!
//! At least one of `publish` / `subscribe` is required. Publish-only is a
//! pure producer step (success = all publishes accepted); subscribe-only is a
//! pure consumer step.
//!
//! # Output
//!
//! ```json
//! { "driver": "memory", "subject": "orders.created", "published": 2,
//!   "received": 1, "duration_ms": 3.21,
//!   "body": "<joined received payloads, for check: body_contains>",
//!   "metrics": { "pubsub_msgs_published": 2, "pubsub_msgs_received": 1,
//!                "pubsub_e2e_ms": [1.2] } }
//! ```
//!
//! `received` / `body` / the `pubsub_msgs_received` and `pubsub_e2e_ms`
//! metrics appear only when `subscribe` is given. `pubsub_e2e_ms` holds one
//! sample per matched message: start of the publish phase → consumed.
//!
//! The step fails (`success: false` + an `[err]` line) on connect failure,
//! publish error, or a subscribe timeout — the error reports how many of
//! `count` arrived and how many the `until_contains` matcher rejected.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, LazyLock, Mutex, Once, OnceLock, RwLock};
use std::time::Instant;

use futures_util::{Stream, StreamExt as _};
use serde_json::{json, Value};
use tokio::sync::broadcast;
use tokio::time::Duration;

use super::actions::{err, ActionOutput, LogTag};
use super::ws::u64_param;

// ---------------------------------------------------------------------------
// Parameters
// ---------------------------------------------------------------------------

/// Resolved `with:` parameters of a `std/pubsub@v1` step, handed to the
/// driver. Public so downstream driver crates can read them.
#[derive(Debug, Clone)]
pub struct PubSubParams {
    /// Requested driver name (`memory`, `nats`, or a downstream-registered one).
    pub driver: String,
    /// NATS subject / in-memory topic name.
    pub subject: String,
    /// Broker URL — required by drivers that talk to a server (`nats`),
    /// ignored by `memory`.
    pub url: Option<String>,
    /// Messages to publish, as wire bytes (non-string JSON values were
    /// serialized to their JSON text at parse time).
    pub publish: Vec<Vec<u8>>,
    /// Consumer side of the step, when `subscribe` was given.
    pub subscribe: Option<SubscribeSpec>,
}

/// The `subscribe` object: wait for `count` matching messages.
#[derive(Debug, Clone)]
pub struct SubscribeSpec {
    /// Messages to wait for (default 1).
    pub count: u64,
    /// Optional substring each counted message must contain; non-matching
    /// messages are passed over and counted as rejected (the ws `Until`
    /// semantics).
    pub until_contains: Option<String>,
    /// Deadline for the wait, ms (default 5000).
    pub timeout_ms: u64,
}

impl PubSubParams {
    fn from_params(params: &Value) -> Result<PubSubParams, String> {
        let driver = params["driver"].as_str().unwrap_or("memory").to_string();
        let subject = params["subject"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or("'subject' is required")?
            .to_string();
        let url = params["url"].as_str().map(str::to_owned);

        let publish = match params.get("publish") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(a)) => a.iter().map(message_bytes).collect(),
            Some(v) => vec![message_bytes(v)],
        };

        let subscribe = match params.get("subscribe") {
            None | Some(Value::Null) => None,
            Some(Value::Object(m)) => Some(SubscribeSpec {
                count: m.get("count").map(|v| u64_param(v, 1)).unwrap_or(1),
                until_contains: match m.get("until_contains") {
                    None => None,
                    Some(v) => Some(
                        v.as_str()
                            .ok_or("'subscribe.until_contains' must be a string")?
                            .to_string(),
                    ),
                },
                timeout_ms: m
                    .get("timeout_ms")
                    .map(|v| u64_param(v, 5000))
                    .unwrap_or(5000),
            }),
            Some(_) => return Err("'subscribe' must be an object".into()),
        };

        if publish.is_empty() && subscribe.is_none() {
            return Err("at least one of 'publish' or 'subscribe' is required".into());
        }

        Ok(PubSubParams {
            driver,
            subject,
            url,
            publish,
            subscribe,
        })
    }
}

/// One `publish` entry as wire bytes: strings verbatim, other JSON values
/// serialized to their JSON text.
fn message_bytes(v: &Value) -> Vec<u8> {
    match v {
        Value::String(s) => s.clone().into_bytes(),
        other => other.to_string().into_bytes(),
    }
}

// ---------------------------------------------------------------------------
// Outcome
// ---------------------------------------------------------------------------

/// What a driver exchange ended with. Public so downstream driver crates can
/// construct it.
#[derive(Debug, Default)]
pub struct PubSubOutcome {
    /// Messages accepted by the transport.
    pub published: u64,
    /// Messages counted toward `subscribe.count` (each matched
    /// `until_contains`, when set). Newline-joined into `body` by the caller.
    pub matched: Vec<String>,
    /// Messages received but rejected by the `until_contains` matcher.
    pub rejected: u64,
    /// One sample per matched message: start of the publish phase → consumed.
    pub e2e_ms: Vec<f64>,
}

// ---------------------------------------------------------------------------
// Driver seam — pluggable transports
// ---------------------------------------------------------------------------

/// Boxed future returned by [`PubSubDriver::run`] — the trait stays
/// object-safe the same way [`super::actions::ActionHandler`] does.
pub type PubSubFuture<'a> =
    Pin<Box<dyn Future<Output = Result<PubSubOutcome, String>> + Send + 'a>>;

/// A pluggable pub/sub transport supplied by this crate (`memory`, `nats`)
/// or a downstream one (proprietary Kafka, Redis, …).
///
/// Implementations own the whole exchange: subscribe first (when requested),
/// publish, then collect matching messages until `subscribe.count` arrive or
/// `subscribe.timeout_ms` passes. A timeout is NOT an `Err` — return
/// `Ok(outcome)` with `outcome.matched.len() < count` and let the caller
/// frame the failure; reserve `Err` for hard failures (connect, publish).
pub trait PubSubDriver: Send + Sync {
    /// Driver name as used in `with: { driver: "…" }` (e.g. `"memory"`).
    fn name(&self) -> &'static str;

    /// Run one exchange. `params` already has `${{ }}` interpolation applied.
    fn run<'a>(&'a self, params: &'a PubSubParams) -> PubSubFuture<'a>;
}

fn driver_registry() -> &'static RwLock<Vec<Arc<dyn PubSubDriver>>> {
    static REGISTRY: OnceLock<RwLock<Vec<Arc<dyn PubSubDriver>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| RwLock::new(Vec::new()))
}

/// Register a custom [`PubSubDriver`]. Typically called once at startup by a
/// downstream (proprietary) crate; registering the same name twice shadows
/// the earlier driver (lookup scans in registration order).
pub fn register_pubsub_driver(driver: Arc<dyn PubSubDriver>) {
    driver_registry().write().unwrap().push(driver);
}

/// Resolve a driver by name, registering the built-ins lazily on first use
/// (no init call needed from outside). An unknown name fails with the list
/// of registered drivers — the user's cue that their build lacks a pro crate.
fn lookup_driver(name: &str) -> Result<Arc<dyn PubSubDriver>, String> {
    static BUILTINS: Once = Once::new();
    BUILTINS.call_once(|| {
        register_pubsub_driver(Arc::new(MemoryDriver));
        register_pubsub_driver(Arc::new(NatsDriver));
    });
    let reg = driver_registry().read().unwrap();
    reg.iter()
        .find(|d| d.name() == name)
        .cloned()
        .ok_or_else(|| {
            let mut names: Vec<&str> = reg.iter().map(|d| d.name()).collect();
            names.sort_unstable();
            format!(
                "unknown pubsub driver '{name}' — registered: {}",
                names.join(", ")
            )
        })
}

// ---------------------------------------------------------------------------
// Shared collect loop
// ---------------------------------------------------------------------------

/// A received-message byte stream, as produced by either driver's transport.
type ByteStream<'a> = Pin<Box<dyn Stream<Item = Vec<u8>> + Send + 'a>>;

/// Read from `stream` until `spec.count` messages have matched (or the
/// timeout fires / the stream ends), filling `matched`, `rejected`, and
/// `e2e_ms` of `out`. `anchor` is the start of the publish phase.
async fn collect_matching(
    stream: &mut ByteStream<'_>,
    spec: &SubscribeSpec,
    anchor: Instant,
    out: &mut PubSubOutcome,
) {
    let deadline = Instant::now() + Duration::from_millis(spec.timeout_ms);
    while (out.matched.len() as u64) < spec.count {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, stream.next()).await {
            Err(_) | Ok(None) => break,
            Ok(Some(bytes)) => {
                let text = String::from_utf8_lossy(&bytes).into_owned();
                if spec
                    .until_contains
                    .as_deref()
                    .is_some_and(|needle| !text.contains(needle))
                {
                    out.rejected += 1;
                } else {
                    out.e2e_ms.push(anchor.elapsed().as_secs_f64() * 1000.0);
                    out.matched.push(text);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// memory driver — process-global in-process bus
// ---------------------------------------------------------------------------

/// One broadcast channel per subject, shared by all VUs in the process.
static BUS: LazyLock<Mutex<HashMap<String, broadcast::Sender<Vec<u8>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Per-subject channel capacity. A slow consumer lags (and misses) rather
/// than back-pressuring publishers — the right posture for a load tool.
const BUS_CAPACITY: usize = 1024;

fn bus_channel(subject: &str) -> broadcast::Sender<Vec<u8>> {
    BUS.lock()
        .unwrap()
        .entry(subject.to_owned())
        .or_insert_with(|| broadcast::channel(BUS_CAPACITY).0)
        .clone()
}

struct MemoryDriver;

impl PubSubDriver for MemoryDriver {
    fn name(&self) -> &'static str {
        "memory"
    }

    fn run<'a>(&'a self, params: &'a PubSubParams) -> PubSubFuture<'a> {
        Box::pin(async move {
            let tx = bus_channel(&params.subject);
            // Receivers BEFORE publishing (the subscribe-first rule). A
            // receiver is held even for publish-only steps so broadcast's
            // `send` — which errors with zero receivers — always succeeds.
            let rx = tx.subscribe();
            let anchor = Instant::now();
            let mut outcome = PubSubOutcome::default();
            for msg in &params.publish {
                tx.send(msg.clone())
                    .map_err(|e| format!("publish '{}': {e}", params.subject))?;
                outcome.published += 1;
            }
            if let Some(spec) = &params.subscribe {
                // Lagged items are dropped (mapped to None): a slow consumer
                // misses messages instead of back-pressuring publishers.
                let mut stream: ByteStream = Box::pin(
                    tokio_stream::wrappers::BroadcastStream::new(rx)
                        .filter_map(|r| async move { r.ok() }),
                );
                collect_matching(&mut stream, spec, anchor, &mut outcome).await;
            }
            Ok(outcome)
        })
    }
}

// ---------------------------------------------------------------------------
// nats driver — real broker via async-nats (core NATS, no JetStream)
// ---------------------------------------------------------------------------

struct NatsDriver;

impl PubSubDriver for NatsDriver {
    fn name(&self) -> &'static str {
        "nats"
    }

    fn run<'a>(&'a self, params: &'a PubSubParams) -> PubSubFuture<'a> {
        Box::pin(async move {
            let url = params
                .url
                .as_deref()
                .ok_or("'url' is required for the nats driver (e.g. nats://127.0.0.1:4222)")?;
            let client = async_nats::connect(url)
                .await
                .map_err(|e| format!("connect {url}: {e}"))?;

            // Subscribe BEFORE publishing, so a same-subject roundtrip sees
            // its own messages.
            let subscriber = match &params.subscribe {
                Some(_) => Some(
                    client
                        .subscribe(params.subject.clone())
                        .await
                        .map_err(|e| format!("subscribe '{}': {e}", params.subject))?,
                ),
                None => None,
            };

            let anchor = Instant::now();
            let mut outcome = PubSubOutcome::default();
            for msg in &params.publish {
                client
                    .publish(params.subject.clone(), msg.clone().into())
                    .await
                    .map_err(|e| format!("publish '{}': {e}", params.subject))?;
                outcome.published += 1;
            }
            if !params.publish.is_empty() {
                // Push the queued messages out before waiting, so e2e samples
                // measure the broker round trip, not client-side buffering.
                client.flush().await.map_err(|e| format!("flush: {e}"))?;
            }

            if let (Some(spec), Some(subscriber)) = (&params.subscribe, subscriber) {
                let mut stream: ByteStream = Box::pin(subscriber.map(|msg| msg.payload.to_vec()));
                collect_matching(&mut stream, spec, anchor, &mut outcome).await;
            }
            Ok(outcome)
        })
    }
}

// ---------------------------------------------------------------------------
// std/pubsub@v1
// ---------------------------------------------------------------------------

pub(crate) async fn pubsub_action(params: &Value, step_name: &str) -> ActionOutput {
    let parsed = match PubSubParams::from_params(params) {
        Ok(p) => p,
        Err(msg) => return err(step_name, msg.as_str()),
    };
    let driver = match lookup_driver(&parsed.driver) {
        Ok(d) => d,
        Err(msg) => return err(step_name, msg.as_str()),
    };

    let t0 = Instant::now();
    let result = driver.run(&parsed).await;
    let duration_ms = t0.elapsed().as_secs_f64() * 1000.0;

    let outcome = match result {
        Ok(o) => o,
        Err(msg) => {
            return ActionOutput {
                value: json!({
                    "driver": parsed.driver,
                    "subject": parsed.subject,
                    "error": msg,
                    "duration_ms": duration_ms,
                }),
                logs: vec![(
                    LogTag::Err,
                    format!(
                        "{step_name}: PUBSUB {} {}: {msg}",
                        parsed.driver, parsed.subject
                    ),
                )],
                success: false,
                http_sample: None,
            };
        }
    };

    let mut value = json!({
        "driver": parsed.driver,
        "subject": parsed.subject,
        "published": outcome.published,
        "duration_ms": duration_ms,
    });
    let mut metrics = json!({ "pubsub_msgs_published": outcome.published });

    // A subscribe wait that fell short of `count` fails the step; how many
    // arrived (and how many the matcher rejected) is the actionable part.
    let mut failure: Option<String> = None;
    if let Some(spec) = &parsed.subscribe {
        let received = outcome.matched.len() as u64;
        value["received"] = json!(received);
        value["body"] = json!(outcome.matched.join("\n"));
        if outcome.rejected > 0 {
            value["rejected"] = json!(outcome.rejected);
        }
        metrics["pubsub_msgs_received"] = json!(received);
        if !outcome.e2e_ms.is_empty() {
            metrics["pubsub_e2e_ms"] = json!(outcome.e2e_ms);
        }
        if received < spec.count {
            let mut why = format!(
                "subscribe timeout: {received} of {} message(s) arrived",
                spec.count
            );
            if spec.until_contains.is_some() {
                why += &format!(" ({} rejected by until_contains)", outcome.rejected);
            }
            failure = Some(why);
        }
    }
    value["metrics"] = metrics;
    if let Some(why) = &failure {
        value["error"] = json!(why);
    }

    let mut parts = Vec::new();
    if !parsed.publish.is_empty() {
        parts.push(format!("published {}", outcome.published));
    }
    if parsed.subscribe.is_some() {
        parts.push(format!("received {}", outcome.matched.len()));
    }
    let mut logs = vec![(
        if failure.is_none() {
            LogTag::Out
        } else {
            LogTag::Err
        },
        format!(
            "PUBSUB {} {} → {} ({duration_ms:.2}ms)",
            parsed.driver,
            parsed.subject,
            parts.join(", ")
        ),
    )];
    if let Some(why) = &failure {
        logs.push((
            LogTag::Err,
            format!(
                "{step_name}: PUBSUB {} {}: {why}",
                parsed.driver, parsed.subject
            ),
        ));
    }

    ActionOutput {
        value,
        logs,
        success: failure.is_none(),
        http_sample: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::step::actions::execute_action;
    use crate::step::context::Context;

    /// Unique subject per test — the memory bus is process-global, so tests
    /// must not talk to each other through shared topic names.
    fn unique_subject(prefix: &str) -> String {
        format!("{prefix}.{}", uuid::Uuid::new_v4())
    }

    // -----------------------------------------------------------------
    // Parameter parsing
    // -----------------------------------------------------------------

    #[test]
    fn params_defaults() {
        let p = PubSubParams::from_params(&json!({
            "subject": "s",
            "publish": "hi",
        }))
        .unwrap();
        assert_eq!(p.driver, "memory");
        assert!(p.url.is_none());
        assert_eq!(p.publish, vec![b"hi".to_vec()]);
        assert!(p.subscribe.is_none());

        let p = PubSubParams::from_params(&json!({
            "subject": "s",
            "subscribe": {},
        }))
        .unwrap();
        let spec = p.subscribe.unwrap();
        assert_eq!(spec.count, 1);
        assert_eq!(spec.timeout_ms, 5000);
        assert!(spec.until_contains.is_none());
    }

    #[test]
    fn params_publish_array_serializes_non_strings_as_json() {
        let p = PubSubParams::from_params(&json!({
            "subject": "s",
            "publish": ["plain", { "a": 1 }, 42],
        }))
        .unwrap();
        assert_eq!(p.publish.len(), 3);
        assert_eq!(p.publish[0], b"plain");
        assert_eq!(p.publish[1], br#"{"a":1}"#.to_vec());
        assert_eq!(p.publish[2], b"42".to_vec());
    }

    #[tokio::test]
    async fn missing_subject_is_rejected() {
        let out = execute_action(
            "std/pubsub@v1",
            &json!({ "publish": "hi" }),
            &Context::new(),
            "step",
        )
        .await;
        assert!(!out.success);
        let text = &out.logs[0].1;
        assert!(text.contains("'subject' is required"), "{text}");
    }

    #[tokio::test]
    async fn neither_publish_nor_subscribe_is_rejected() {
        let out = execute_action(
            "std/pubsub@v1",
            &json!({ "subject": "s" }),
            &Context::new(),
            "step",
        )
        .await;
        assert!(!out.success);
        let text = &out.logs[0].1;
        assert!(
            text.contains("at least one of 'publish' or 'subscribe'"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn unknown_driver_lists_registered_drivers() {
        let out = execute_action(
            "std/pubsub@v1",
            &json!({ "subject": "s", "driver": "kafka", "publish": "hi" }),
            &Context::new(),
            "step",
        )
        .await;
        assert!(!out.success);
        let text = &out.logs[0].1;
        assert!(
            text.contains("unknown pubsub driver 'kafka' — registered: memory, nats"),
            "{text}"
        );
    }

    // -----------------------------------------------------------------
    // memory driver
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn memory_publish_subscribe_roundtrip_same_subject() {
        let subject = unique_subject("roundtrip");
        let out = execute_action(
            "std/pubsub@v1",
            &json!({
                "subject": subject,
                "publish": ["order-1 created", "order-2 created"],
                "subscribe": { "count": 2, "timeout_ms": 2000 },
            }),
            &Context::new(),
            "roundtrip",
        )
        .await;
        assert!(out.success, "{:?}", out.logs);
        assert_eq!(out.value["published"], 2);
        assert_eq!(out.value["received"], 2);
        assert_eq!(out.value["body"], "order-1 created\norder-2 created");
        // Custom metrics ride the reserved `metrics` key.
        assert_eq!(out.value["metrics"]["pubsub_msgs_published"], 2);
        assert_eq!(out.value["metrics"]["pubsub_msgs_received"], 2);
        let e2e = out.value["metrics"]["pubsub_e2e_ms"].as_array().unwrap();
        assert_eq!(e2e.len(), 2);
        assert!(e2e.iter().all(|s| s.as_f64().unwrap() >= 0.0));
    }

    #[tokio::test]
    async fn memory_until_contains_match_and_mismatch() {
        // Match: the counted message must contain the substring.
        let subject = unique_subject("until-match");
        let out = execute_action(
            "std/pubsub@v1",
            &json!({
                "subject": subject,
                "publish": "order-42 created",
                "subscribe": { "count": 1, "until_contains": "order", "timeout_ms": 2000 },
            }),
            &Context::new(),
            "match",
        )
        .await;
        assert!(out.success, "{:?}", out.logs);
        assert_eq!(out.value["received"], 1);

        // Mismatch: the message arrives but the matcher rejects it, so the
        // wait times out and the error says so.
        let subject = unique_subject("until-mismatch");
        let out = execute_action(
            "std/pubsub@v1",
            &json!({
                "subject": subject,
                "publish": "unrelated noise",
                "subscribe": { "count": 1, "until_contains": "order", "timeout_ms": 200 },
            }),
            &Context::new(),
            "mismatch",
        )
        .await;
        assert!(!out.success);
        assert_eq!(out.value["received"], 0);
        assert_eq!(out.value["rejected"], 1);
        let err = out.value["error"].as_str().unwrap();
        assert!(err.contains("0 of 1 message(s) arrived"), "{err}");
        assert!(err.contains("1 rejected by until_contains"), "{err}");
    }

    #[tokio::test]
    async fn memory_subscribe_timeout_reports_partial_count() {
        let subject = unique_subject("timeout");
        let out = execute_action(
            "std/pubsub@v1",
            &json!({
                "subject": subject,
                "subscribe": { "count": 2, "timeout_ms": 200 },
            }),
            &Context::new(),
            "consumer",
        )
        .await;
        assert!(!out.success);
        let err = out.value["error"].as_str().unwrap();
        assert!(err.contains("0 of 2 message(s) arrived"), "{err}");
        // Failures carry a clear [err] log line.
        assert!(out.logs.iter().any(|(tag, _)| *tag == LogTag::Err));
    }

    #[tokio::test]
    async fn memory_cross_task_delivery() {
        // One task subscribes, another publishes from the same process — the
        // process-global bus delivers across tasks (and across VUs in a run).
        let subject = unique_subject("cross-task");
        let sub_subject = subject.clone();
        let subscriber = tokio::spawn(async move {
            execute_action(
                "std/pubsub@v1",
                &json!({
                    "subject": sub_subject,
                    "subscribe": { "count": 1, "timeout_ms": 2000 },
                }),
                &Context::new(),
                "subscriber",
            )
            .await
        });
        // Let the subscriber establish its receiver before publishing.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let published = execute_action(
            "std/pubsub@v1",
            &json!({ "subject": subject, "publish": "hello from another task" }),
            &Context::new(),
            "publisher",
        )
        .await;
        assert!(published.success, "{:?}", published.logs);

        let received = subscriber.await.unwrap();
        assert!(received.success, "{:?}", received.logs);
        assert_eq!(received.value["body"], "hello from another task");
    }

    #[tokio::test]
    async fn memory_publish_only_omits_received_and_e2e() {
        let subject = unique_subject("producer");
        let out = execute_action(
            "std/pubsub@v1",
            &json!({ "subject": subject, "publish": ["a", "b"] }),
            &Context::new(),
            "producer",
        )
        .await;
        assert!(out.success, "{:?}", out.logs);
        assert_eq!(out.value["published"], 2);
        assert!(out.value.get("received").is_none());
        assert!(out.value.get("body").is_none());
        assert_eq!(out.value["metrics"]["pubsub_msgs_published"], 2);
        assert!(out.value["metrics"].get("pubsub_msgs_received").is_none());
        assert!(out.value["metrics"].get("pubsub_e2e_ms").is_none());
    }

    // -----------------------------------------------------------------
    // nats driver — gated on a reachable broker (like the jmeter tests gate
    // on the binary): NATS_URL, or something answering on 127.0.0.1:4222.
    // -----------------------------------------------------------------

    async fn nats_url() -> Option<String> {
        if let Ok(url) = std::env::var("NATS_URL") {
            return Some(url);
        }
        tokio::net::TcpStream::connect("127.0.0.1:4222")
            .await
            .ok()
            .map(|_| "nats://127.0.0.1:4222".to_string())
    }

    #[tokio::test]
    async fn nats_roundtrip_when_broker_available() {
        let Some(url) = nats_url().await else {
            eprintln!("skipping: no NATS broker (set NATS_URL or run one on 127.0.0.1:4222)");
            return;
        };
        let subject = unique_subject("nats.roundtrip");
        let out = execute_action(
            "std/pubsub@v1",
            &json!({
                "driver": "nats",
                "url": url,
                "subject": subject,
                "publish": ["n1", "n2"],
                "subscribe": { "count": 2, "until_contains": "n", "timeout_ms": 5000 },
            }),
            &Context::new(),
            "nats-roundtrip",
        )
        .await;
        assert!(out.success, "{:?}", out.logs);
        assert_eq!(out.value["published"], 2);
        assert_eq!(out.value["received"], 2);
        assert_eq!(
            out.value["metrics"]["pubsub_e2e_ms"]
                .as_array()
                .map(Vec::len),
            Some(2)
        );
    }

    #[tokio::test]
    async fn nats_subscribe_timeout_when_broker_available() {
        let Some(url) = nats_url().await else {
            eprintln!("skipping: no NATS broker (set NATS_URL or run one on 127.0.0.1:4222)");
            return;
        };
        let subject = unique_subject("nats.timeout");
        let out = execute_action(
            "std/pubsub@v1",
            &json!({
                "driver": "nats",
                "url": url,
                "subject": subject,
                "subscribe": { "count": 1, "timeout_ms": 300 },
            }),
            &Context::new(),
            "nats-timeout",
        )
        .await;
        assert!(!out.success);
        let err = out.value["error"].as_str().unwrap();
        assert!(err.contains("0 of 1 message(s) arrived"), "{err}");
    }
}
