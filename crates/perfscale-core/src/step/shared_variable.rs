//! Shared mutable state across VUs: `std/set_shared_variable@v1` and
//! `std/get_shared_variable@v1` — one run-scoped key/value store with atomic
//! operations, for cross-VU coordination (barriers, producer/consumer queues,
//! shared counters) that immutable per-VU `variables` cannot express.
//!
//! Every operation is atomic (a single lock acquisition per op) — there is no
//! read-modify-write pair of steps to race, and no lock primitive: atomics
//! cover the use cases, so there is nothing to deadlock.
//!
//! # Declaration (mandatory, validated before the run starts)
//!
//! ```yaml
//! # config.yaml
//! shared_variables:
//!   pending_orders: []      # type inferred: list
//!   approved_count: 0       # type inferred: number
//! ```
//!
//! A step referencing an undeclared name, or an `op` incompatible with the
//! declared type (`increment` on a list), fails **configuration validation**
//! before any VU starts — a typo must not become a silent `null` mid-test.
//! There is no dynamic creation: steps only use declared names.
//!
//! # Drivers
//!
//! The backing store is pluggable behind [`SharedVariableDriver`]; this crate
//! ships:
//!
//! | Driver   | Store                                                        |
//! |----------|--------------------------------------------------------------|
//! | `memory` | Process-global map (default), shared by every VU in the process |
//!
//! Proprietary drivers (Redis, …) live in closed crates and register
//! themselves via [`register_shared_variable_driver`] at process start — the
//! same extension posture as [`super::pubsub::register_pubsub_driver`]. An
//! unknown `driver` value fails the step with the list of registered drivers.
//!
//! The `memory` store is **process-global**: two perfscaled agents each have
//! their own store, so cross-agent sharing requires a networked driver.
//! At run start every registered driver is handed the declarations
//! ([`seed_shared_variables`]): declared names are (re)set to their initial
//! values, nothing else is touched.
//!
//! # `std/set_shared_variable@v1` parameters
//!
//! | Parameter | Type   | Default    | Description |
//! |-----------|--------|------------|-------------|
//! | `driver`  | string | `"memory"` | `memory` or any registered downstream driver |
//! | `name`    | string | required   | Declared shared-variable name |
//! | `op`      | string | `"set"`    | `set` (any type), `increment` (number), `append` (list) |
//! | `value`   | any    | required   | New value / increment delta / appended element |
//!
//! `increment` returns the new value, `append` the new list length, `set` the
//! stored value.
//!
//! # `std/get_shared_variable@v1` parameters
//!
//! | Parameter  | Type   | Default  | Description |
//! |------------|--------|----------|-------------|
//! | `driver`   | string | `"memory"` | `memory` or any registered downstream driver |
//! | `name`     | string | required | Declared shared-variable name |
//! | `op`       | string | `"get"`  | `get` (any type) or `pop` (list: remove and return the first element, `null` when empty) |
//! | `wait_for` | object | —        | `{ exists \| equals: <json> \| length_gte: <int>, timeout_ms }` — block until the condition holds; exactly one condition key, `timeout_ms` default 5000 |
//! | `extract`  | object | —        | `{ key: dotted-path }` — the same `$.a.b[0]` syntax as `std/llm@v1`'s `extract` |
//!
//! `wait_for` polls with a small async sleep (no spinning). On timeout the
//! step **fails** (`success: false` + an `[err]` line, the same contract as
//! `std/pubsub@v1`'s subscribe), reporting the last observed value.
//!
//! # Output
//!
//! ```json
//! { "driver": "memory", "name": "pending_orders", "op": "get",
//!   "value": <the resulting value>, "duration_ms": 0.05 }
//! ```
//!
//! With `extract`, each extracted key is added at the top level instead of
//! `value` (unresolvable paths map to `null`). A `wait_for` wait adds
//! `waited_ms` and a `shared_variable_wait_ms` metric sample.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, LazyLock, Mutex, Once, OnceLock, RwLock};
use std::time::Instant;

use serde_json::{json, Map, Value};
use tokio::time::Duration;

use super::actions::{err, ActionOutput, LogTag};
use super::llm::{parse_dotted_path, resolve_path, PathSegment};
use super::ws::u64_param;
use super::Step;

// ---------------------------------------------------------------------------
// Parameters
// ---------------------------------------------------------------------------

/// Mutating ops of `std/set_shared_variable@v1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetOp {
    Set,
    Increment,
    Append,
}

impl SetOp {
    fn parse(raw: &str) -> Result<SetOp, String> {
        match raw {
            "set" => Ok(SetOp::Set),
            "increment" => Ok(SetOp::Increment),
            "append" => Ok(SetOp::Append),
            other => Err(format!(
                "unknown op '{other}' for std/set_shared_variable@v1 — use set, increment, or append"
            )),
        }
    }

    fn as_str(&self) -> &'static str {
        match self {
            SetOp::Set => "set",
            SetOp::Increment => "increment",
            SetOp::Append => "append",
        }
    }
}

/// Reading ops of `std/get_shared_variable@v1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GetOp {
    Get,
    Pop,
}

impl GetOp {
    fn parse(raw: &str) -> Result<GetOp, String> {
        match raw {
            "get" => Ok(GetOp::Get),
            "pop" => Ok(GetOp::Pop),
            other => Err(format!(
                "unknown op '{other}' for std/get_shared_variable@v1 — use get or pop"
            )),
        }
    }

    fn as_str(&self) -> &'static str {
        match self {
            GetOp::Get => "get",
            GetOp::Pop => "pop",
        }
    }
}

/// The `wait_for` object of a get step: block until the condition holds.
#[derive(Debug, Clone)]
pub struct WaitSpec {
    pub condition: WaitCondition,
    /// Deadline for the wait, ms (default 5000).
    pub timeout_ms: u64,
}

/// One `wait_for` condition: `exists`, `equals: <json>`, or
/// `length_gte: <int>`.
#[derive(Debug, Clone)]
pub enum WaitCondition {
    /// The key is present in the store.
    Exists,
    /// The stored value deep-equals the given JSON.
    Equals(Value),
    /// The stored list holds at least N elements.
    LengthGte(u64),
}

impl WaitCondition {
    fn describe(&self) -> String {
        match self {
            WaitCondition::Exists => "exists".to_string(),
            WaitCondition::Equals(v) => format!("equals {v}"),
            WaitCondition::LengthGte(n) => format!("length_gte {n}"),
        }
    }

    /// Whether the condition holds for an observed value. Only called after
    /// the key was read successfully, so `Exists` is already satisfied.
    fn met_by(&self, value: &Value) -> bool {
        match self {
            WaitCondition::Exists => true,
            WaitCondition::Equals(expected) => value == expected,
            WaitCondition::LengthGte(n) => value.as_array().is_some_and(|a| a.len() as u64 >= *n),
        }
    }
}

/// Resolved `with:` parameters of a `std/set_shared_variable@v1` step.
/// Public so downstream driver crates can read them.
#[derive(Debug, Clone)]
pub struct SetSharedVariableParams {
    /// Requested driver name (`memory`, or a downstream-registered one).
    pub driver: String,
    /// Declared shared-variable name.
    pub name: String,
    pub op: SetOp,
    /// New value / increment delta / appended element.
    pub value: Value,
}

/// Resolved `with:` parameters of a `std/get_shared_variable@v1` step.
/// Public so downstream driver crates can read them.
#[derive(Debug, Clone)]
pub struct GetSharedVariableParams {
    /// Requested driver name (`memory`, or a downstream-registered one).
    pub driver: String,
    /// Declared shared-variable name.
    pub name: String,
    pub op: GetOp,
    /// Optional blocking wait applied before the read.
    pub wait_for: Option<WaitSpec>,
    /// `extract` object: output key → parsed dotted path (the `std/llm@v1`
    /// path syntax). Empty = default 1-1 mapping of the whole value.
    pub extract: Vec<(String, Vec<PathSegment>)>,
}

fn driver_and_name(params: &Value) -> Result<(String, String), String> {
    let driver = params["driver"].as_str().unwrap_or("memory").to_string();
    let name = params["name"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("'name' is required")?
        .to_string();
    Ok((driver, name))
}

impl SetSharedVariableParams {
    fn from_params(params: &Value) -> Result<SetSharedVariableParams, String> {
        let (driver, name) = driver_and_name(params)?;
        let op = match params.get("op") {
            None | Some(Value::Null) => SetOp::Set,
            Some(Value::String(s)) => SetOp::parse(s)?,
            Some(_) => return Err("'op' must be a string".into()),
        };
        let value = params
            .get("value")
            .filter(|v| !v.is_null())
            .cloned()
            .ok_or("'value' is required")?;
        Ok(SetSharedVariableParams {
            driver,
            name,
            op,
            value,
        })
    }
}

impl GetSharedVariableParams {
    fn from_params(params: &Value) -> Result<GetSharedVariableParams, String> {
        let (driver, name) = driver_and_name(params)?;
        let op = match params.get("op") {
            None | Some(Value::Null) => GetOp::Get,
            Some(Value::String(s)) => GetOp::parse(s)?,
            Some(_) => return Err("'op' must be a string".into()),
        };

        let wait_for = match params.get("wait_for") {
            None | Some(Value::Null) => None,
            Some(Value::Object(m)) => {
                let mut condition: Option<WaitCondition> = None;
                let mut take = |cond: WaitCondition| -> Result<(), String> {
                    if condition.is_some() {
                        return Err(
                            "'wait_for' takes exactly one of exists, equals, length_gte".into()
                        );
                    }
                    condition = Some(cond);
                    Ok(())
                };
                if m.contains_key("exists") {
                    take(WaitCondition::Exists)?;
                }
                if let Some(v) = m.get("equals") {
                    take(WaitCondition::Equals(v.clone()))?;
                }
                if let Some(v) = m.get("length_gte") {
                    let n = v
                        .as_u64()
                        .ok_or("'wait_for.length_gte' must be a non-negative integer")?;
                    take(WaitCondition::LengthGte(n))?;
                }
                let Some(condition) = condition else {
                    return Err(
                        "'wait_for' needs a condition: one of exists, equals, length_gte".into(),
                    );
                };
                Some(WaitSpec {
                    condition,
                    timeout_ms: m
                        .get("timeout_ms")
                        .map(|v| u64_param(v, 5000))
                        .unwrap_or(5000),
                })
            }
            Some(_) => return Err("'wait_for' must be an object".into()),
        };

        let mut extract = Vec::new();
        if let Some(v) = params.get("extract") {
            let Value::Object(m) = v else {
                return Err("'extract' must be an object".into());
            };
            for (key, sel) in m {
                let Value::String(raw) = sel else {
                    return Err(format!("'extract.{key}' must be a string"));
                };
                let rest = raw.strip_prefix("$.").ok_or_else(|| {
                    format!("'extract.{key}' must be a dotted path starting with '$.'")
                })?;
                extract.push((key.clone(), parse_dotted_path(rest)?));
            }
        }

        Ok(GetSharedVariableParams {
            driver,
            name,
            op,
            wait_for,
            extract,
        })
    }
}

// ---------------------------------------------------------------------------
// Driver seam — pluggable stores
// ---------------------------------------------------------------------------

/// One atomic operation handed to a [`SharedVariableDriver`].
#[derive(Debug, Clone)]
pub enum SharedVarOp {
    /// Overwrite with the given value (any type).
    Set(Value),
    /// Add the given number to the stored number; result = new value.
    Increment(Value),
    /// Append the given element to the stored list; result = new length.
    Append(Value),
    /// Read the stored value.
    Get,
    /// Remove and return the first element of the stored list; `null` when
    /// empty.
    Pop,
}

/// Boxed futures returned by the driver methods — the trait stays
/// object-safe the same way [`super::pubsub::PubSubDriver`] does.
pub type SharedVarFuture<'a> = Pin<Box<dyn Future<Output = Result<Value, String>> + Send + 'a>>;
pub type SharedVarResetFuture<'a> = Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;

/// A pluggable shared-variable store supplied by this crate (`memory`) or a
/// downstream one (proprietary Redis, …).
///
/// Every op must be atomic: one lock acquisition (or one native atomic
/// command) per [`apply`](SharedVariableDriver::apply) call. The result value
/// is what the step reports: `set` → the stored value, `increment` → the new
/// value, `append` → the new length, `get` → the value, `pop` → the removed
/// element or `null`. An op on a name that was never declared (seeded) is an
/// `Err` — there is no dynamic creation.
pub trait SharedVariableDriver: Send + Sync {
    /// Driver name as used in `with: { driver: "…" }` (e.g. `"memory"`).
    fn name(&self) -> &'static str;

    /// Apply one atomic op. `name` already has `${{ }}` interpolation applied.
    fn apply<'a>(&'a self, name: &'a str, op: SharedVarOp) -> SharedVarFuture<'a>;

    /// (Re)set every declared name to its initial value, called once at run
    /// start via [`seed_shared_variables`]. Drivers without run-scoped state
    /// keep the default no-op.
    fn reset<'a>(&'a self, decls: &'a Map<String, Value>) -> SharedVarResetFuture<'a> {
        let _ = decls;
        Box::pin(async { Ok(()) })
    }
}

fn driver_registry() -> &'static RwLock<Vec<Arc<dyn SharedVariableDriver>>> {
    static REGISTRY: OnceLock<RwLock<Vec<Arc<dyn SharedVariableDriver>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| RwLock::new(Vec::new()))
}

/// Register a custom [`SharedVariableDriver`]. Typically called once at
/// startup by a downstream (proprietary) crate; registering the same name
/// twice shadows the earlier driver (lookup scans in registration order).
pub fn register_shared_variable_driver(driver: Arc<dyn SharedVariableDriver>) {
    driver_registry().write().unwrap().push(driver);
}

fn register_builtins() {
    static BUILTINS: Once = Once::new();
    BUILTINS.call_once(|| {
        register_shared_variable_driver(Arc::new(MemoryDriver));
    });
}

/// Resolve a driver by name, registering the built-ins lazily on first use.
/// An unknown name fails with the list of registered drivers — the user's cue
/// that their build lacks a pro crate.
fn lookup_driver(name: &str) -> Result<Arc<dyn SharedVariableDriver>, String> {
    register_builtins();
    let reg = driver_registry().read().unwrap();
    reg.iter()
        .find(|d| d.name() == name)
        .cloned()
        .ok_or_else(|| {
            let mut names: Vec<&str> = reg.iter().map(|d| d.name()).collect();
            names.sort_unstable();
            format!(
                "unknown shared variable driver '{name}' — registered: {}",
                names.join(", ")
            )
        })
}

/// Hand the run's `shared_variables` declarations to every registered driver
/// (each declared name is (re)set to its initial value). Called once at run
/// start, before any step executes, so `before:` steps can already use them.
pub async fn seed_shared_variables(decls: &Map<String, Value>) -> Result<(), String> {
    if decls.is_empty() {
        return Ok(());
    }
    register_builtins();
    let drivers: Vec<Arc<dyn SharedVariableDriver>> =
        driver_registry().read().unwrap().iter().cloned().collect();
    for d in drivers {
        d.reset(decls).await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Pre-start validation — declaration is mandatory
// ---------------------------------------------------------------------------

/// The type a declaration infers from its initial value, for error messages
/// and op/type checks.
fn declared_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "list",
        Value::Object(_) => "object",
    }
}

/// Validate one step against the `shared_variables` declarations. `action` is
/// the step's `use` id, `with` its raw params, `label` a human-readable step
/// name for error messages. Steps of other actions pass. A `${{ }}` placeholder
/// in `name`/`op` skips the static check — it resolves only at run time.
pub fn check_shared_variable_step(
    action: &str,
    with: Option<&Value>,
    label: &str,
    decls: &Map<String, Value>,
) -> Result<(), String> {
    let is_set = matches!(action, "std/set_shared_variable@v1" | "set_shared_variable");
    let is_get = matches!(action, "std/get_shared_variable@v1" | "get_shared_variable");
    if !is_set && !is_get {
        return Ok(());
    }
    let name = with
        .and_then(|w| w.get("name"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let Some(name) = name else {
        return Err(format!(
            "step '{label}' ({action}) needs a 'name' referencing a declared shared variable"
        ));
    };
    if name.contains("${{") {
        return Ok(());
    }
    let Some(initial) = decls.get(name) else {
        return Err(format!(
            "step '{label}' references undeclared shared variable '{name}' — declare it under `shared_variables:` in the config file"
        ));
    };
    let ty = declared_type(initial);

    let op = with
        .and_then(|w| w.get("op"))
        .and_then(Value::as_str)
        .filter(|s| !s.contains("${{"));
    if let Some(op) = op {
        if is_set {
            let parsed = SetOp::parse(op)?;
            if parsed == SetOp::Increment && !initial.is_number() {
                return Err(format!(
                    "step '{label}': op 'increment' requires a number, but shared variable '{name}' is declared as {ty}"
                ));
            }
            if parsed == SetOp::Append && !initial.is_array() {
                return Err(format!(
                    "step '{label}': op 'append' requires a list, but shared variable '{name}' is declared as {ty}"
                ));
            }
        } else {
            let parsed = GetOp::parse(op)?;
            if parsed == GetOp::Pop && !initial.is_array() {
                return Err(format!(
                    "step '{label}': op 'pop' requires a list, but shared variable '{name}' is declared as {ty}"
                ));
            }
        }
    }

    if is_get {
        if let Some(m) = with
            .and_then(|w| w.get("wait_for"))
            .and_then(Value::as_object)
        {
            if m.contains_key("length_gte") && !initial.is_array() {
                return Err(format!(
                    "step '{label}': wait_for 'length_gte' requires a list, but shared variable '{name}' is declared as {ty}"
                ));
            }
        }
    }
    Ok(())
}

/// Validate every step of a run (test steps plus the config's `before:` /
/// `after:` blocks) against the `shared_variables` declarations. Called before
/// the run starts; the first problem aborts with a configuration error.
pub fn validate_shared_variable_usage(
    before: &[Step],
    steps: &[Step],
    after: &[Step],
    decls: &Map<String, Value>,
) -> Result<(), String> {
    for (section, list) in [("before", before), ("steps", steps), ("after", after)] {
        for (i, step) in list.iter().enumerate() {
            let label = step
                .name
                .clone()
                .unwrap_or_else(|| format!("{section}/{i}"));
            check_shared_variable_step(&step.action, step.with.as_ref(), &label, decls)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// memory driver — process-global store
// ---------------------------------------------------------------------------

/// One entry per declared name, shared by all VUs in the process.
static STORE: LazyLock<Mutex<HashMap<String, Value>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

struct MemoryDriver;

impl MemoryDriver {
    /// Whole `apply` under one lock acquisition — every op is atomic.
    fn apply_locked(name: &str, op: SharedVarOp) -> Result<Value, String> {
        let mut store = STORE.lock().unwrap();
        let entry = store.get_mut(name).ok_or_else(|| {
            format!(
                "shared variable '{name}' is not declared in `shared_variables:` (no dynamic creation)"
            )
        })?;
        match op {
            SharedVarOp::Set(v) => {
                *entry = v.clone();
                Ok(v)
            }
            SharedVarOp::Increment(delta) => {
                let next = increment_value(entry, &delta)
                    .map_err(|why| format!("shared variable '{name}': {why} (op 'increment')"))?;
                *entry = next.clone();
                Ok(next)
            }
            SharedVarOp::Append(element) => {
                let Value::Array(list) = entry else {
                    return Err(format!(
                        "shared variable '{name}' holds a {}, op 'append' requires a list",
                        declared_type(entry)
                    ));
                };
                list.push(element);
                Ok(json!(list.len()))
            }
            SharedVarOp::Get => Ok(entry.clone()),
            SharedVarOp::Pop => {
                let Value::Array(list) = entry else {
                    return Err(format!(
                        "shared variable '{name}' holds a {}, op 'pop' requires a list",
                        declared_type(entry)
                    ));
                };
                if list.is_empty() {
                    Ok(Value::Null)
                } else {
                    Ok(list.remove(0))
                }
            }
        }
    }
}

/// Integer addition stays integer; anything else falls back to f64 (same as
/// Redis INCR vs INCRBYFLOAT).
fn increment_value(current: &Value, delta: &Value) -> Result<Value, String> {
    let (Value::Number(c), Value::Number(d)) = (current, delta) else {
        return Err(format!(
            "requires numbers, got {} and {}",
            declared_type(current),
            declared_type(delta)
        ));
    };
    if let (Some(c), Some(d)) = (c.as_i64(), d.as_i64()) {
        if let Some(sum) = c.checked_add(d) {
            return Ok(json!(sum));
        }
    }
    Ok(json!(c.as_f64().unwrap_or(0.0) + d.as_f64().unwrap_or(0.0)))
}

impl SharedVariableDriver for MemoryDriver {
    fn name(&self) -> &'static str {
        "memory"
    }

    fn apply<'a>(&'a self, name: &'a str, op: SharedVarOp) -> SharedVarFuture<'a> {
        Box::pin(async move { Self::apply_locked(name, op) })
    }

    fn reset<'a>(&'a self, decls: &'a Map<String, Value>) -> SharedVarResetFuture<'a> {
        Box::pin(async move {
            let mut store = STORE.lock().unwrap();
            for (name, initial) in decls {
                store.insert(name.clone(), initial.clone());
            }
            Ok(())
        })
    }
}

// ---------------------------------------------------------------------------
// std/set_shared_variable@v1
// ---------------------------------------------------------------------------

pub(crate) async fn set_shared_variable_action(params: &Value, step_name: &str) -> ActionOutput {
    let parsed = match SetSharedVariableParams::from_params(params) {
        Ok(p) => p,
        Err(msg) => return err(step_name, msg.as_str()),
    };
    let driver = match lookup_driver(&parsed.driver) {
        Ok(d) => d,
        Err(msg) => return err(step_name, msg.as_str()),
    };

    let op = match parsed.op {
        SetOp::Set => SharedVarOp::Set(parsed.value.clone()),
        SetOp::Increment => SharedVarOp::Increment(parsed.value.clone()),
        SetOp::Append => SharedVarOp::Append(parsed.value.clone()),
    };

    let t0 = Instant::now();
    let result = driver.apply(&parsed.name, op).await;
    let duration_ms = t0.elapsed().as_secs_f64() * 1000.0;

    match result {
        Ok(value) => ActionOutput {
            value: json!({
                "driver": parsed.driver,
                "name": parsed.name,
                "op": parsed.op.as_str(),
                "value": value,
                "duration_ms": duration_ms,
            }),
            logs: vec![(
                LogTag::Out,
                format!(
                    "SHARED_VAR {} {} {} → {} ({duration_ms:.2}ms)",
                    parsed.driver,
                    parsed.name,
                    parsed.op.as_str(),
                    value
                ),
            )],
            success: true,
            http_sample: None,
        },
        Err(msg) => ActionOutput {
            value: json!({
                "driver": parsed.driver,
                "name": parsed.name,
                "op": parsed.op.as_str(),
                "error": msg,
                "duration_ms": duration_ms,
            }),
            logs: vec![(
                LogTag::Err,
                format!(
                    "{step_name}: SHARED_VAR {} {} {}: {msg}",
                    parsed.driver,
                    parsed.name,
                    parsed.op.as_str()
                ),
            )],
            success: false,
            http_sample: None,
        },
    }
}

// ---------------------------------------------------------------------------
// std/get_shared_variable@v1
// ---------------------------------------------------------------------------

/// Poll interval of a `wait_for` wait: small enough to react quickly, large
/// enough to never spin the runtime.
const WAIT_POLL_MS: u64 = 10;

pub(crate) async fn get_shared_variable_action(params: &Value, step_name: &str) -> ActionOutput {
    let parsed = match GetSharedVariableParams::from_params(params) {
        Ok(p) => p,
        Err(msg) => return err(step_name, msg.as_str()),
    };
    let driver = match lookup_driver(&parsed.driver) {
        Ok(d) => d,
        Err(msg) => return err(step_name, msg.as_str()),
    };

    let t0 = Instant::now();
    let mut failure: Option<String> = None;
    let mut last_observed: Option<Value> = None;
    let mut waited_ms: Option<f64> = None;

    // Blocking wait: poll the store until the condition holds or the deadline
    // passes. A timeout FAILS the step (same contract as pubsub subscribe).
    if let Some(spec) = &parsed.wait_for {
        let deadline = Instant::now() + Duration::from_millis(spec.timeout_ms);
        loop {
            match driver.apply(&parsed.name, SharedVarOp::Get).await {
                Ok(v) => {
                    if spec.condition.met_by(&v) {
                        last_observed = Some(v);
                        break;
                    }
                    last_observed = Some(v);
                }
                Err(msg) => {
                    failure = Some(msg);
                    break;
                }
            }
            if Instant::now() >= deadline {
                failure = Some(format!(
                    "wait_for timeout: {} not met within {}ms (last value: {})",
                    spec.condition.describe(),
                    spec.timeout_ms,
                    last_observed
                        .as_ref()
                        .map(Value::to_string)
                        .unwrap_or_else(|| "<none>".into())
                ));
                break;
            }
            tokio::time::sleep(Duration::from_millis(WAIT_POLL_MS)).await;
        }
        waited_ms = Some(t0.elapsed().as_secs_f64() * 1000.0);
    }

    let result = if failure.is_some() {
        None
    } else {
        let op = match parsed.op {
            GetOp::Get => SharedVarOp::Get,
            GetOp::Pop => SharedVarOp::Pop,
        };
        match driver.apply(&parsed.name, op).await {
            Ok(v) => Some(v),
            Err(msg) => {
                failure = Some(msg);
                None
            }
        }
    };

    let duration_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let mut value = json!({
        "driver": parsed.driver,
        "name": parsed.name,
        "op": parsed.op.as_str(),
        "duration_ms": duration_ms,
    });
    if let Some(ms) = waited_ms {
        value["waited_ms"] = json!(ms);
        value["metrics"] = json!({ "shared_variable_wait_ms": [ms] });
    }
    // Output mapping: `extract` paths land at the top level (unresolvable
    // paths map to null); without `extract` the whole value maps 1-1.
    match (&result, parsed.extract.is_empty()) {
        (Some(v), true) => value["value"] = v.clone(),
        (Some(v), false) => {
            for (key, path) in &parsed.extract {
                value[key] = resolve_path(v, path).cloned().unwrap_or(Value::Null);
            }
            // The whole value stays available under `value` too — extract
            // keys are conveniences, not a replacement.
            value["value"] = v.clone();
        }
        (None, _) => {
            if let Some(last) = &last_observed {
                value["value"] = last.clone();
            }
        }
    }
    if let Some(why) = &failure {
        value["error"] = json!(why);
    }

    let mut logs = vec![(
        if failure.is_none() {
            LogTag::Out
        } else {
            LogTag::Err
        },
        format!(
            "SHARED_VAR {} {} {} → {} ({duration_ms:.2}ms)",
            parsed.driver,
            parsed.name,
            parsed.op.as_str(),
            result
                .as_ref()
                .map(Value::to_string)
                .unwrap_or_else(|| "-".into())
        ),
    )];
    if let Some(why) = &failure {
        logs.push((
            LogTag::Err,
            format!(
                "{step_name}: SHARED_VAR {} {} {}: {why}",
                parsed.driver,
                parsed.name,
                parsed.op.as_str()
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

    /// Unique variable names per test — the memory store is process-global,
    /// so tests must not talk to each other through shared names.
    fn unique_name(prefix: &str) -> String {
        format!("{prefix}.{}", uuid::Uuid::new_v4())
    }

    async fn seed(pairs: &[(&str, Value)]) {
        let decls: Map<String, Value> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect();
        seed_shared_variables(&decls).await.unwrap();
    }

    fn step(action: &str, name: Option<&str>, with: Value) -> Step {
        Step {
            name: name.map(str::to_owned),
            action: action.into(),
            with: Some(with),
            check: None,
            outputs: None,
            severity: None,
            message: None,
        }
    }

    // -----------------------------------------------------------------
    // Parameter parsing
    // -----------------------------------------------------------------

    #[test]
    fn set_params_defaults() {
        let p = SetSharedVariableParams::from_params(&json!({ "name": "n", "value": 1 })).unwrap();
        assert_eq!(p.driver, "memory");
        assert_eq!(p.op, SetOp::Set);
        assert_eq!(p.value, json!(1));
    }

    #[test]
    fn get_params_defaults() {
        let p = GetSharedVariableParams::from_params(&json!({ "name": "n" })).unwrap();
        assert_eq!(p.driver, "memory");
        assert_eq!(p.op, GetOp::Get);
        assert!(p.wait_for.is_none());
        assert!(p.extract.is_empty());

        let p = GetSharedVariableParams::from_params(&json!({
            "name": "n",
            "wait_for": { "length_gte": 3 },
        }))
        .unwrap();
        let spec = p.wait_for.unwrap();
        assert_eq!(spec.timeout_ms, 5000);
        assert!(matches!(spec.condition, WaitCondition::LengthGte(3)));
    }

    #[test]
    fn params_rejections() {
        for bad in [
            json!({ "value": 1 }),                            // missing name
            json!({ "name": "n" }),                           // set without value
            json!({ "name": "n", "op": "swap", "value": 1 }), // unknown op
            json!({ "name": "n", "op": 3, "value": 1 }),      // non-string op
        ] {
            assert!(SetSharedVariableParams::from_params(&bad).is_err(), "{bad}");
        }
        for bad in [
            json!({ "wait_for": {} }),                              // no condition
            json!({ "wait_for": { "exists": true, "equals": 1 } }), // two conditions
            json!({ "wait_for": "equals 1" }),                      // not an object
            json!({ "wait_for": { "length_gte": -1 } }),            // negative
            json!({ "extract": "$.a" }),                            // not an object
            json!({ "extract": { "k": "a.b" } }),                   // missing $. prefix
            json!({ "extract": { "k": "$.a..b" } }),                // empty segment
            json!({ "extract": { "k": 5 } }),                       // non-string selector
        ] {
            let mut with = bad;
            with["name"] = json!("n");
            assert!(
                GetSharedVariableParams::from_params(&with).is_err(),
                "{with}"
            );
        }
    }

    // -----------------------------------------------------------------
    // Pre-start validation
    // -----------------------------------------------------------------

    fn decls() -> Map<String, Value> {
        json!({ "count": 0, "queue": [], "label": "x" })
            .as_object()
            .unwrap()
            .clone()
    }

    #[test]
    fn validation_accepts_compatible_ops() {
        let steps = vec![
            step(
                "std/set_shared_variable@v1",
                None,
                json!({ "name": "count", "op": "increment", "value": 1 }),
            ),
            step(
                "std/set_shared_variable@v1",
                None,
                json!({ "name": "queue", "op": "append", "value": 1 }),
            ),
            step(
                "std/get_shared_variable@v1",
                None,
                json!({ "name": "queue", "op": "pop" }),
            ),
            step(
                "std/get_shared_variable@v1",
                None,
                json!({ "name": "queue", "wait_for": { "length_gte": 2 } }),
            ),
            step(
                "std/set_shared_variable@v1",
                None,
                json!({ "name": "label", "value": "y" }),
            ),
            step("std/http@v1", None, json!({ "url": "https://example.com" })),
        ];
        validate_shared_variable_usage(&[], &steps, &[], &decls()).unwrap();
    }

    #[test]
    fn validation_rejects_undeclared_name() {
        let steps = vec![step(
            "std/get_shared_variable@v1",
            Some("take order"),
            json!({ "name": "pending_orders" }),
        )];
        let msg = validate_shared_variable_usage(&[], &steps, &[], &decls()).unwrap_err();
        assert!(
            msg.contains("undeclared shared variable 'pending_orders'"),
            "{msg}"
        );
        assert!(msg.contains("take order"), "{msg}");
    }

    #[test]
    fn validation_rejects_op_type_mismatch() {
        let cases = [
            (
                "std/set_shared_variable@v1",
                json!({ "name": "queue", "op": "increment", "value": 1 }),
                "requires a number",
            ),
            (
                "std/set_shared_variable@v1",
                json!({ "name": "count", "op": "append", "value": 1 }),
                "requires a list",
            ),
            (
                "std/get_shared_variable@v1",
                json!({ "name": "count", "op": "pop" }),
                "requires a list",
            ),
            (
                "std/get_shared_variable@v1",
                json!({ "name": "count", "wait_for": { "length_gte": 1 } }),
                "length_gte",
            ),
        ];
        for (action, with, needle) in cases {
            let steps = vec![step(action, None, with)];
            let msg = validate_shared_variable_usage(&[], &steps, &[], &decls()).unwrap_err();
            assert!(msg.contains(needle), "{msg} must contain {needle}");
        }
    }

    #[test]
    fn validation_skips_placeholder_names() {
        let steps = vec![step(
            "std/get_shared_variable@v1",
            None,
            json!({ "name": "${{ vars.which }}" }),
        )];
        validate_shared_variable_usage(&[], &steps, &[], &decls()).unwrap();
    }

    #[test]
    fn validation_scans_before_and_after_too() {
        let before = vec![step(
            "std/set_shared_variable@v1",
            None,
            json!({ "name": "nope", "value": 1 }),
        )];
        let msg = validate_shared_variable_usage(&before, &[], &[], &decls()).unwrap_err();
        assert!(msg.contains("undeclared"), "{msg}");
    }

    // -----------------------------------------------------------------
    // memory driver — atomic ops
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn set_get_roundtrip() {
        let name = unique_name("roundtrip");
        seed(&[(&name, json!("initial"))]).await;
        let out = execute_action(
            "std/get_shared_variable@v1",
            &json!({ "name": name }),
            &Context::new(),
            "get",
        )
        .await;
        assert!(out.success, "{:?}", out.logs);
        assert_eq!(out.value["value"], "initial");

        let out = execute_action(
            "std/set_shared_variable@v1",
            &json!({ "name": name, "value": { "a": 1 } }),
            &Context::new(),
            "set",
        )
        .await;
        assert!(out.success, "{:?}", out.logs);
        assert_eq!(out.value["value"], json!({ "a": 1 }));
        assert_eq!(out.value["driver"], "memory");
        assert_eq!(out.value["op"], "set");
    }

    #[tokio::test]
    async fn increment_returns_new_value_and_stays_integer() {
        let name = unique_name("incr");
        seed(&[(&name, json!(0))]).await;
        let out = execute_action(
            "std/set_shared_variable@v1",
            &json!({ "name": name, "op": "increment", "value": 5 }),
            &Context::new(),
            "incr",
        )
        .await;
        assert!(out.success, "{:?}", out.logs);
        assert_eq!(out.value["value"], 5);
        let out = execute_action(
            "std/set_shared_variable@v1",
            &json!({ "name": name, "op": "increment", "value": 0.5 }),
            &Context::new(),
            "incr",
        )
        .await;
        assert_eq!(out.value["value"], 5.5);
    }

    #[tokio::test]
    async fn append_returns_new_length_pop_returns_element_then_null() {
        let name = unique_name("queue");
        seed(&[(&name, json!([]))]).await;
        for (element, len) in [("a", 1), ("b", 2)] {
            let out = execute_action(
                "std/set_shared_variable@v1",
                &json!({ "name": name, "op": "append", "value": element }),
                &Context::new(),
                "append",
            )
            .await;
            assert!(out.success, "{:?}", out.logs);
            assert_eq!(out.value["value"], len);
        }
        let out = execute_action(
            "std/get_shared_variable@v1",
            &json!({ "name": name, "op": "pop" }),
            &Context::new(),
            "pop",
        )
        .await;
        assert_eq!(out.value["value"], "a"); // FIFO, like Redis LPOP
        execute_action(
            "std/get_shared_variable@v1",
            &json!({ "name": name, "op": "pop" }),
            &Context::new(),
            "pop",
        )
        .await;
        let out = execute_action(
            "std/get_shared_variable@v1",
            &json!({ "name": name, "op": "pop" }),
            &Context::new(),
            "pop",
        )
        .await;
        assert!(out.success);
        assert_eq!(out.value["value"], Value::Null); // empty → null
    }

    #[tokio::test]
    async fn undeclared_name_fails_at_runtime_too() {
        // Defense in depth: validation is the primary gate, but a run that
        // skipped it (or a driver that was never seeded) still refuses to
        // create names dynamically.
        let name = unique_name("never-declared");
        let out = execute_action(
            "std/get_shared_variable@v1",
            &json!({ "name": name }),
            &Context::new(),
            "get",
        )
        .await;
        assert!(!out.success);
        let text = &out.logs[1].1;
        assert!(text.contains("is not declared"), "{text}");
    }

    #[tokio::test]
    async fn unknown_driver_lists_registered_drivers() {
        let out = execute_action(
            "std/get_shared_variable@v1",
            &json!({ "name": "n", "driver": "redis" }),
            &Context::new(),
            "get",
        )
        .await;
        assert!(!out.success);
        let text = &out.logs[0].1;
        assert!(
            text.contains("unknown shared variable driver 'redis' — registered: memory"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn concurrent_increments_are_atomic() {
        let name = unique_name("counter");
        seed(&[(&name, json!(0))]).await;
        let mut handles = Vec::new();
        for _ in 0..8 {
            let name = name.clone();
            handles.push(tokio::spawn(async move {
                for _ in 0..50 {
                    let out = execute_action(
                        "std/set_shared_variable@v1",
                        &json!({ "name": name, "op": "increment", "value": 1 }),
                        &Context::new(),
                        "incr",
                    )
                    .await;
                    assert!(out.success);
                }
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let out = execute_action(
            "std/get_shared_variable@v1",
            &json!({ "name": name }),
            &Context::new(),
            "get",
        )
        .await;
        assert_eq!(out.value["value"], 400);
    }

    #[tokio::test]
    async fn concurrent_appends_and_pops_are_atomic() {
        let name = unique_name("atomic-queue");
        seed(&[(&name, json!([]))]).await;
        // 4 tasks append 25 elements each — every length is returned exactly
        // once, so the lengths must cover 1..=100 without duplicates.
        let mut handles = Vec::new();
        for _ in 0..4 {
            let name = name.clone();
            handles.push(tokio::spawn(async move {
                let mut lengths = Vec::new();
                for _ in 0..25 {
                    let out = execute_action(
                        "std/set_shared_variable@v1",
                        &json!({ "name": name, "op": "append", "value": 1 }),
                        &Context::new(),
                        "append",
                    )
                    .await;
                    lengths.push(out.value["value"].as_u64().unwrap());
                }
                lengths
            }));
        }
        let mut all: Vec<u64> = Vec::new();
        for h in handles {
            all.extend(h.await.unwrap());
        }
        all.sort_unstable();
        assert_eq!(all, (1..=100).collect::<Vec<u64>>());

        // 4 tasks pop 25 each — 100 pops must return exactly the 100 elements.
        let mut handles = Vec::new();
        for _ in 0..4 {
            let name = name.clone();
            handles.push(tokio::spawn(async move {
                let mut popped = 0;
                for _ in 0..25 {
                    let out = execute_action(
                        "std/get_shared_variable@v1",
                        &json!({ "name": name, "op": "pop" }),
                        &Context::new(),
                        "pop",
                    )
                    .await;
                    if !out.value["value"].is_null() {
                        popped += 1;
                    }
                }
                popped
            }));
        }
        let mut total = 0;
        for h in handles {
            total += h.await.unwrap();
        }
        assert_eq!(total, 100);
    }

    // -----------------------------------------------------------------
    // wait_for
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn wait_for_equals_succeeds_when_another_task_sets() {
        let name = unique_name("wait-equals");
        seed(&[(&name, json!(0))]).await;
        let setter_name = name.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            execute_action(
                "std/set_shared_variable@v1",
                &json!({ "name": setter_name, "op": "increment", "value": 10 }),
                &Context::new(),
                "setter",
            )
            .await;
        });
        let out = execute_action(
            "std/get_shared_variable@v1",
            &json!({ "name": name, "wait_for": { "equals": 10, "timeout_ms": 2000 } }),
            &Context::new(),
            "waiter",
        )
        .await;
        assert!(out.success, "{:?}", out.logs);
        assert_eq!(out.value["value"], 10);
        assert!(out.value["waited_ms"].as_f64().unwrap() >= 0.0);
        assert!(out.value["metrics"]["shared_variable_wait_ms"].is_array());
    }

    #[tokio::test]
    async fn wait_for_length_gte_succeeds() {
        let name = unique_name("wait-len");
        seed(&[(&name, json!([]))]).await;
        let producer_name = name.clone();
        tokio::spawn(async move {
            for _ in 0..3 {
                execute_action(
                    "std/set_shared_variable@v1",
                    &json!({ "name": producer_name, "op": "append", "value": "order" }),
                    &Context::new(),
                    "producer",
                )
                .await;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });
        let out = execute_action(
            "std/get_shared_variable@v1",
            &json!({ "name": name, "wait_for": { "length_gte": 3, "timeout_ms": 2000 } }),
            &Context::new(),
            "consumer",
        )
        .await;
        assert!(out.success, "{:?}", out.logs);
        assert_eq!(out.value["value"].as_array().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn wait_for_exists_is_true_for_a_declared_name() {
        let name = unique_name("wait-exists");
        seed(&[(&name, json!(null))]).await;
        let out = execute_action(
            "std/get_shared_variable@v1",
            &json!({ "name": name, "wait_for": { "exists": true } }),
            &Context::new(),
            "waiter",
        )
        .await;
        assert!(out.success, "{:?}", out.logs);
    }

    #[tokio::test]
    async fn wait_for_timeout_fails_reporting_last_value() {
        let name = unique_name("wait-timeout");
        seed(&[(&name, json!(7))]).await;
        let out = execute_action(
            "std/get_shared_variable@v1",
            &json!({ "name": name, "wait_for": { "equals": 42, "timeout_ms": 150 } }),
            &Context::new(),
            "waiter",
        )
        .await;
        assert!(!out.success);
        let err = out.value["error"].as_str().unwrap();
        assert!(err.contains("wait_for timeout"), "{err}");
        assert!(err.contains("equals 42"), "{err}");
        assert!(err.contains("last value: 7"), "{err}");
        // Same failure contract as pubsub subscribe: [err] line + last value.
        assert!(out.logs.iter().any(|(tag, _)| *tag == LogTag::Err));
        assert_eq!(out.value["value"], 7);
    }

    // -----------------------------------------------------------------
    // extract mapping
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn extract_maps_dotted_paths_to_output_keys() {
        let name = unique_name("extract");
        seed(&[(&name, json!(null))]).await;
        execute_action(
            "std/set_shared_variable@v1",
            &json!({ "name": name, "value": { "usage": { "completion_tokens": 12 }, "choices": [{ "text": "hi" }] } }),
            &Context::new(),
            "set",
        )
        .await;
        let out = execute_action(
            "std/get_shared_variable@v1",
            &json!({
                "name": name,
                "extract": {
                    "tokens": "$.usage.completion_tokens",
                    "first": "$.choices[0].text",
                    "missing": "$.nope",
                },
            }),
            &Context::new(),
            "get",
        )
        .await;
        assert!(out.success, "{:?}", out.logs);
        assert_eq!(out.value["tokens"], 12);
        assert_eq!(out.value["first"], "hi");
        assert_eq!(out.value["missing"], Value::Null);
        // The whole value stays available too.
        assert_eq!(out.value["value"]["usage"]["completion_tokens"], 12);
    }

    #[tokio::test]
    async fn extract_over_a_list_value() {
        let name = unique_name("extract-list");
        seed(&[(&name, json!([{ "total": 99 }]))]).await;
        let out = execute_action(
            "std/get_shared_variable@v1",
            &json!({ "name": name, "extract": { "first_total": "$.[0].total" } }),
            &Context::new(),
            "get",
        )
        .await;
        assert!(out.success, "{:?}", out.logs);
        assert_eq!(out.value["first_total"], 99);
    }
}
