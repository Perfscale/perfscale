//! GraphQL actions.
//!
//! | Action ID        | What it does                                              |
//! |------------------|-----------------------------------------------------------|
//! | `std/graphql@v1` | GraphQL query/mutation over HTTP with schema validation   |
//!
//! # Request shape
//!
//! A step sends one GraphQL operation per execution. The document comes from
//! `query` (inline, multiline YAML) or `query_file` (a `.graphql` file on
//! disk — filesystem access, so it requires `allow_file_actions` and honours
//! `fs_root` confinement). `variables` is a JSON object; `${{ … }}`
//! interpolation applies to every parameter, so values extracted from
//! previous steps flow in naturally (`variables: { id: "${{ create.data.id
//! }}" }`), and single-brace `${…}` generator tokens (`${uuid}`, `${rand}`,
//! `${now}`, …) expand per execution like in the ws/grpc payloads.
//! `operation` sets the `operationName` — required when the document
//! holds more than one operation.
//!
//! Transport is HTTP POST (the default, mandatory for mutations in practice)
//! or opt-in GET (`method: GET`) for CDN-cacheable queries — the document
//! and variables travel as URL query parameters.
//!
//! # Schema validation
//!
//! Every query is parsed before it is sent (a syntax error fails the step
//! without a network call) and, when a schema is available, validated against
//! it — an unknown field fails the step with a did-you-mean suggestion
//! instead of burning a request against the target:
//!
//! - `introspection` (default `true`) — fetch the schema from the endpoint
//!   with an introspection query. The schema is cached process-wide per URL,
//!   so hundreds of VUs pay one introspection round trip per run.
//! - `schema_file` — validate against a local SDL file instead (filesystem
//!   access, same confinement as `query_file`). Also the fallback when the
//!   endpoint refuses introspection: fetch fails → SDL is used when given,
//!   otherwise the step runs unvalidated (a `[sys]` line says so once).
//!
//! # Response semantics
//!
//! GraphQL errors travel in a `200 OK` body, so HTTP status alone is not the
//! verdict: a response with `errors` and no `data` fails the step, while
//! partial `data` plus `errors` is a success (the server resolved what it
//! could) and the errors are counted in the `graphql_errors` metric.
//!
//! Output: `{ status, data, errors?, body, duration_ms, headers, metrics }`.
//! `data`/`errors` are the decoded GraphQL payload (extraction reads
//! `${{ step.data.viewer.id }}`); `body` is the raw text for
//! `body_contains` checks.
//!
//! # Connection pooling
//!
//! `pool: per-vu` (default) pins the step to the VU's HTTP client shard —
//! same keep-alive behaviour as `std/http@v1`. `pool: shared` puts every VU
//! on one process-global client, maximising connection reuse against a
//! single endpoint at the cost of pool-lock contention.
//!
//! # Metrics
//!
//! Emitted via the reserved `metrics` key plus the standard HTTP sample:
//!
//! - `graphql_req_duration` (histogram) — every execution, so thresholds can
//!   gate on it (`graphql_req_duration p99 < 200`); the runner derives the
//!   matching `graphql_req_failed` rate.
//! - `graphql_errors` (counter) — GraphQL-level errors seen, including the
//!   partial-data ones that pass the step.
//! - `graphql_op_<operationName>_duration` (histogram) — only when the
//!   operation is named (explicit `operation` or a single named operation in
//!   the document), keeping metric cardinality bounded by the test
//!   definition.
//! - `http_req_duration` / `http_req_failed` / `http_reqs` — the request
//!   also feeds the standard HTTP aggregates, like `std/http@v1`.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use graphql_parser::query::{
    Definition as QueryDefinition, Document as QueryDocument, OperationDefinition, Selection,
    SelectionSet, Value as GqlValue,
};
use graphql_parser::schema::{Definition as SchemaDefinition, Type as SchemaType, TypeDefinition};
use serde_json::{json, Value};
use tokio::time::Duration;

use super::actions::{confine_fs_path, err, ActionOutput, HttpSample, LogTag};
use super::context::Context;
use super::http::{
    client as http_client, error_chain, request_line, timed_exchange, transport_error, BodyKind,
    ClientPool,
};
use super::ws::{bool_param, u64_param};
use crate::generate::{expand_tokens, Gen};
use crate::lint::closest_name;

/// Recursion guard for the validation walk: fragment spreads can cycle
/// (`fragment A` spreads `B`, `B` spreads `A`), which the GraphQL spec
/// forbids but a hand-written query can still contain.
const MAX_VALIDATE_DEPTH: usize = 64;

// ---------------------------------------------------------------------------
// Parameters
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct Params {
    url: String,
    /// The GraphQL document (inline or loaded from `query_file`).
    query: String,
    variables: Option<Value>,
    operation: Option<String>,
    get: bool,
    headers: Vec<(String, String)>,
    timeout_ms: u64,
    insecure: bool,
    pool: ClientPool,
    introspection: bool,
    schema_file: Option<String>,
}

/// Parse and validate the step parameters. `query_file` is read here (not
/// per send) so a missing file fails before any request is made.
async fn parse_params(
    params: &Value,
    step_name: &str,
    ctx: &Context,
) -> Result<Params, ActionOutput> {
    let Some(url) = params["url"].as_str() else {
        return Err(err(step_name, "'url' is required"));
    };

    let inline = params["query"].as_str();
    let file = params["query_file"].as_str();
    let query = match (inline, file) {
        (Some(q), None) => q.to_string(),
        (None, Some(path)) => {
            let path = match confine_fs_path(ctx, path, false) {
                Ok(p) => p,
                Err(msg) => return Err(err(step_name, &format!("'query_file': {msg}"))),
            };
            match tokio::fs::read_to_string(&path).await {
                Ok(q) => q,
                Err(e) => {
                    return Err(err(
                        step_name,
                        &format!("'query_file': cannot read '{}': {e}", path.display()),
                    ))
                }
            }
        }
        (Some(_), Some(_)) => {
            return Err(err(
                step_name,
                "'query' and 'query_file' are mutually exclusive",
            ))
        }
        (None, None) => return Err(err(step_name, "'query' or 'query_file' is required")),
    };

    let method = params["method"].as_str().unwrap_or("POST").to_uppercase();
    let get = match method.as_str() {
        "POST" => false,
        "GET" => true,
        _ => return Err(err(step_name, "'method' must be POST or GET")),
    };

    let mut headers = Vec::new();
    if let Some(obj) = params["headers"].as_object() {
        for (k, v) in obj {
            if let Some(val) = v.as_str() {
                headers.push((k.clone(), val.to_string()));
            }
        }
    }

    let pool = match ClientPool::from_params(params) {
        Ok(p) => p,
        Err(msg) => return Err(err(step_name, &msg)),
    };

    if let Some(v) = params.get("variables") {
        if !v.is_null() && !v.is_object() {
            return Err(err(step_name, "'variables' must be a JSON object"));
        }
    }

    Ok(Params {
        url: url.to_string(),
        query,
        variables: params.get("variables").filter(|v| v.is_object()).cloned(),
        operation: params["operation"].as_str().map(str::to_string),
        get,
        headers,
        timeout_ms: u64_param(&params["timeout"], 10_000),
        insecure: bool_param(&params["insecure"]),
        pool,
        // `introspection: false` opts out of schema fetching entirely.
        introspection: params.get("introspection").is_none_or(bool_param),
        schema_file: params["schema_file"].as_str().map(str::to_string),
    })
}

// ---------------------------------------------------------------------------
// Schema — built from server introspection or a local SDL file
// ---------------------------------------------------------------------------

/// The slice of a GraphQL schema the validator needs: root type names and,
/// per composite type, each field's unwrapped named type. Everything else
/// (argument types, descriptions, deprecations) does not affect whether a
/// selection set is well-formed.
#[derive(Debug, Clone, Default)]
pub struct GraphqlSchema {
    query_type: Option<String>,
    mutation_type: Option<String>,
    /// Object/interface type name → field name → named field type.
    fields: HashMap<String, HashMap<String, String>>,
    /// Union type name → member type names (direct selections are illegal).
    unions: HashMap<String, Vec<String>>,
}

impl GraphqlSchema {
    /// Unwrap a field's named type, if the field exists.
    fn field_type(&self, parent: &str, field: &str) -> Option<&str> {
        self.fields.get(parent)?.get(field).map(String::as_str)
    }

    /// True for object/interface/union types (selections allowed inside).
    fn is_composite(&self, type_name: &str) -> bool {
        self.fields.contains_key(type_name) || self.unions.contains_key(type_name)
    }
}

/// The introspection query, trimmed to what the validator consumes — the
/// full canonical query pulls descriptions/args/deprecations this engine
/// never reads, and bigger responses cost the target more to build.
const INTROSPECTION_QUERY: &str = r#"query IntrospectionQuery {
  __schema {
    queryType { name }
    mutationType { name }
    types {
      kind
      name
      fields { name type { ...TypeRef } }
      possibleTypes { name }
    }
  }
}
fragment TypeRef on __Type {
  kind
  name
  ofType { kind name ofType { kind name ofType { kind name ofType { kind name ofType { kind name ofType { kind name ofType { kind name } } } } } } }
}"#;

/// POST the introspection query to `url` and build a [`GraphqlSchema`] from
/// the response. Shared by the runtime action (through the process-wide
/// cache) and the `perfscale lint` network pass (directly).
pub async fn introspect_schema(
    client: &reqwest::Client,
    url: &str,
    headers: &[(String, String)],
    timeout_ms: u64,
) -> Result<GraphqlSchema, String> {
    let mut req = client
        .post(url)
        .timeout(Duration::from_millis(timeout_ms))
        .json(&json!({ "query": INTROSPECTION_QUERY, "operationName": "IntrospectionQuery" }));
    for (k, v) in headers {
        req = req.header(k.as_str(), v.as_str());
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("introspection request failed: {}", error_chain(&e)))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| format!("introspection response read failed: {e}"))?;
    let json: Value = serde_json::from_str(&body)
        .map_err(|_| format!("introspection returned non-JSON (HTTP {status})"))?;
    if let Some(errors) = json.get("errors").and_then(|e| e.as_array()) {
        let msgs: Vec<&str> = errors
            .iter()
            .filter_map(|e| e["message"].as_str())
            .collect();
        return Err(format!(
            "introspection rejected: {}",
            if msgs.is_empty() {
                format!("{} error(s)", errors.len())
            } else {
                msgs.join("; ")
            }
        ));
    }
    let schema_json = json
        .pointer("/data/__schema")
        .cloned()
        .ok_or_else(|| "introspection response has no data.__schema".to_string())?;
    schema_from_introspection(&schema_json)
}

/// Build a [`GraphqlSchema`] from the `__schema` value of an introspection
/// response.
fn schema_from_introspection(s: &Value) -> Result<GraphqlSchema, String> {
    let named = |t: &Value| -> String {
        // Walk ofType wrappers (NON_NULL/LIST) down to the named type.
        let mut cur = t;
        loop {
            if let Some(name) = cur["name"].as_str() {
                return name.to_string();
            }
            match cur.get("ofType") {
                Some(inner) if !inner.is_null() => cur = inner,
                _ => return String::new(),
            }
        }
    };

    let mut out = GraphqlSchema {
        query_type: s["queryType"]["name"].as_str().map(str::to_string),
        mutation_type: s["mutationType"]["name"].as_str().map(str::to_string),
        ..Default::default()
    };
    let Some(types) = s["types"].as_array() else {
        return Err("introspection response has no types".into());
    };
    for t in types {
        let (Some(kind), Some(name)) = (t["kind"].as_str(), t["name"].as_str()) else {
            continue;
        };
        // Introspection internals pollute did-you-mean suggestions.
        if name.starts_with("__") {
            continue;
        }
        match kind {
            "OBJECT" | "INTERFACE" => {
                let mut fields = HashMap::new();
                for f in t["fields"].as_array().into_iter().flatten() {
                    if let Some(fname) = f["name"].as_str() {
                        fields.insert(fname.to_string(), named(&f["type"]));
                    }
                }
                out.fields.insert(name.to_string(), fields);
            }
            "UNION" => {
                let members = t["possibleTypes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|m| m["name"].as_str().map(str::to_string))
                    .collect();
                out.unions.insert(name.to_string(), members);
            }
            _ => {}
        }
    }
    if out.query_type.is_none() {
        return Err("introspection response names no query type".into());
    }
    Ok(out)
}

/// Build a [`GraphqlSchema`] from SDL text (a `schema_file`). Root types come
/// from the `schema { ... }` block when present, else default to the
/// conventional `Query`/`Mutation` type names.
pub fn schema_from_sdl(sdl: &str) -> Result<GraphqlSchema, String> {
    let doc = graphql_parser::parse_schema::<&str>(sdl).map_err(|e| format!("invalid SDL: {e}"))?;
    let mut out = GraphqlSchema::default();
    for def in &doc.definitions {
        match def {
            SchemaDefinition::SchemaDefinition(sd) => {
                out.query_type = sd.query.map(|q| q.to_string());
                out.mutation_type = sd.mutation.map(|m| m.to_string());
            }
            SchemaDefinition::TypeDefinition(td) => match td {
                TypeDefinition::Object(obj) => {
                    let mut fields = HashMap::new();
                    for f in &obj.fields {
                        fields.insert(f.name.to_string(), named_schema_type(&f.field_type));
                    }
                    out.fields.insert(obj.name.to_string(), fields);
                }
                TypeDefinition::Interface(iface) => {
                    let mut fields = HashMap::new();
                    for f in &iface.fields {
                        fields.insert(f.name.to_string(), named_schema_type(&f.field_type));
                    }
                    out.fields.insert(iface.name.to_string(), fields);
                }
                TypeDefinition::Union(u) => {
                    out.unions.insert(
                        u.name.to_string(),
                        u.types.iter().map(|t| t.to_string()).collect(),
                    );
                }
                _ => {}
            },
            _ => {}
        }
    }
    if out.query_type.is_none() && out.fields.contains_key("Query") {
        out.query_type = Some("Query".into());
    }
    if out.mutation_type.is_none() && out.fields.contains_key("Mutation") {
        out.mutation_type = Some("Mutation".into());
    }
    if out.query_type.is_none() {
        return Err("SDL names no query type (no schema block and no Query type)".into());
    }
    Ok(out)
}

/// Unwrap SDL `Type` (NonNull/List wrappers) down to its named type.
fn named_schema_type<'a>(t: &SchemaType<'a, &'a str>) -> String {
    match t {
        SchemaType::NamedType(n) => n.to_string(),
        SchemaType::ListType(inner) | SchemaType::NonNullType(inner) => named_schema_type(inner),
    }
}

// ---------------------------------------------------------------------------
// Query validation
// ---------------------------------------------------------------------------

/// Syntax-check a GraphQL document. Shared by the linter (offline) and the
/// action (before every send — parsing a small document costs microseconds
/// against a network round trip, so no cache).
pub fn validate_query_syntax(query: &str) -> Result<(), String> {
    graphql_parser::parse_query::<&str>(query)
        .map(|_| ())
        .map_err(|e| format!("invalid GraphQL syntax: {e}"))
}

/// Validate a parsed document against a [`GraphqlSchema`]: every selected
/// field must exist on its parent type, composites must have sub-selections,
/// leaf types must not, and used variables must be defined by the operation.
/// Errors name the selection path (`Mutation.createUser.viewr`) and suggest
/// the closest known field on a typo.
pub fn validate_against_schema(schema: &GraphqlSchema, query: &str) -> Result<(), String> {
    let doc = graphql_parser::parse_query::<&str>(query)
        .map_err(|e| format!("invalid GraphQL syntax: {e}"))?;
    validate_document(schema, &doc)
}

fn validate_document<'a>(
    schema: &'a GraphqlSchema,
    doc: &'a QueryDocument<'a, &'a str>,
) -> Result<(), String> {
    let fragments: HashMap<&str, &graphql_parser::query::FragmentDefinition<'a, &'a str>> = doc
        .definitions
        .iter()
        .filter_map(|d| match d {
            QueryDefinition::Fragment(f) => Some((f.name, f)),
            _ => None,
        })
        .collect();

    for def in &doc.definitions {
        let QueryDefinition::Operation(op) = def else {
            continue;
        };
        let (root, selection_set, var_defs, label) = match op {
            OperationDefinition::Query(q) => (
                schema.query_type.as_deref(),
                &q.selection_set,
                &q.variable_definitions,
                "Query",
            ),
            OperationDefinition::Mutation(m) => (
                schema.mutation_type.as_deref(),
                &m.selection_set,
                &m.variable_definitions,
                "Mutation",
            ),
            OperationDefinition::Subscription(_) => {
                return Err(
                    "subscriptions are not supported by std/graphql@v1 (HTTP transport)".into(),
                )
            }
            OperationDefinition::SelectionSet(ss) => {
                (schema.query_type.as_deref(), ss, &Vec::new(), "Query")
            }
        };
        let Some(root) = root else {
            return Err(format!("schema has no {} root type", label.to_lowercase()));
        };
        check_variables_defined(selection_set, var_defs)?;
        validate_selection_set(schema, &fragments, root, selection_set, label, 0)?;
    }
    Ok(())
}

/// Every `$var` referenced in the operation's arguments must be declared in
/// its variable definitions — an undeclared variable is a server-side error
/// that schema validation should catch first.
fn check_variables_defined<'a>(
    selection_set: &'a SelectionSet<'a, &'a str>,
    var_defs: &[graphql_parser::query::VariableDefinition<'a, &'a str>],
) -> Result<(), String> {
    let mut used = Vec::new();
    collect_variables(selection_set, &mut used);
    for name in used {
        if !var_defs.iter().any(|d| d.name == name) {
            return Err(format!("variable '${name}' is used but not defined"));
        }
    }
    Ok(())
}

fn collect_variables<'a>(selection_set: &'a SelectionSet<'a, &'a str>, out: &mut Vec<&'a str>) {
    fn walk_value<'a>(v: &'a GqlValue<'a, &'a str>, out: &mut Vec<&'a str>) {
        match v {
            GqlValue::Variable(name) => out.push(name),
            GqlValue::List(items) => items.iter().for_each(|v| walk_value(v, out)),
            GqlValue::Object(map) => map.values().for_each(|v| walk_value(v, out)),
            _ => {}
        }
    }
    for sel in &selection_set.items {
        match sel {
            Selection::Field(f) => {
                for (_, v) in &f.arguments {
                    walk_value(v, out);
                }
                collect_variables(&f.selection_set, out);
            }
            Selection::InlineFragment(f) => collect_variables(&f.selection_set, out),
            // Fragment spreads are checked within the fragment definition's
            // own walk; variables there share the operation's definitions.
            Selection::FragmentSpread(_) => {}
        }
    }
}

fn validate_selection_set<'a>(
    schema: &'a GraphqlSchema,
    fragments: &HashMap<&str, &'a graphql_parser::query::FragmentDefinition<'a, &'a str>>,
    parent_type: &'a str,
    selection_set: &'a SelectionSet<'a, &'a str>,
    path: &str,
    depth: usize,
) -> Result<(), String> {
    if depth > MAX_VALIDATE_DEPTH {
        return Err(format!(
            "selection at '{path}' nests too deep (fragment cycle?)"
        ));
    }
    if schema.unions.contains_key(parent_type) {
        // A union exposes no direct fields — only __typename and fragments.
        for sel in &selection_set.items {
            match sel {
                Selection::Field(f) if f.name == "__typename" => {}
                Selection::Field(f) => {
                    return Err(format!(
                        "union type '{parent_type}' has no field '{}' — select members via inline fragments (`... on Type {{ ... }}`)",
                        f.name
                    ))
                }
                Selection::InlineFragment(f) => {
                    let t = fragment_target(&f.type_condition, parent_type);
                    validate_selection_set(schema, fragments, t, &f.selection_set, path, depth + 1)?;
                }
                Selection::FragmentSpread(spread) => {
                    let Some(frag) = fragments.get(spread.fragment_name) else {
                        return Err(format!("unknown fragment '{}'", spread.fragment_name));
                    };
                    let t = fragment_def_target(&frag.type_condition, parent_type);
                    validate_selection_set(schema, fragments, t, &frag.selection_set, path, depth + 1)?;
                }
            }
        }
        return Ok(());
    }

    let Some(fields) = schema.fields.get(parent_type) else {
        // Scalars/enums/custom leaf types simply have no entry — but then a
        // non-empty selection set is an error at the parent level, reported
        // by the caller's composite check. Reaching here with selections
        // means the schema is unusual; accept rather than false-positive.
        return Ok(());
    };

    for sel in &selection_set.items {
        match sel {
            Selection::Field(f) => {
                if f.name == "__typename" {
                    continue;
                }
                let field_path = format!("{path}.{}", f.name);
                let Some(field_type) = schema.field_type(parent_type, f.name) else {
                    let suggestion = closest_name(f.name, fields.keys().map(String::as_str))
                        .map(|k| format!(" — did you mean '{k}'?"));
                    return Err(format!(
                        "unknown field '{}' on type '{parent_type}'{}",
                        f.name,
                        suggestion.unwrap_or_default()
                    ));
                };
                let has_sub = !f.selection_set.items.is_empty();
                if has_sub {
                    if !schema.is_composite(field_type) {
                        return Err(format!(
                            "field '{field_path}' of leaf type '{field_type}' must not have a selection set"
                        ));
                    }
                    validate_selection_set(
                        schema,
                        fragments,
                        field_type,
                        &f.selection_set,
                        &field_path,
                        depth + 1,
                    )?;
                } else if schema.is_composite(field_type) {
                    return Err(format!(
                        "field '{field_path}' of composite type '{field_type}' needs a selection set"
                    ));
                }
            }
            Selection::InlineFragment(f) => {
                let t = fragment_target(&f.type_condition, parent_type);
                validate_selection_set(schema, fragments, t, &f.selection_set, path, depth + 1)?;
            }
            Selection::FragmentSpread(spread) => {
                let Some(frag) = fragments.get(spread.fragment_name) else {
                    return Err(format!("unknown fragment '{}'", spread.fragment_name));
                };
                let t = fragment_def_target(&frag.type_condition, parent_type);
                validate_selection_set(schema, fragments, t, &frag.selection_set, path, depth + 1)?;
            }
        }
    }
    Ok(())
}

/// Resolve a fragment's type condition to the type name to validate against
/// (no condition = the enclosing type).
fn fragment_target<'a>(
    cond: &'a Option<graphql_parser::query::TypeCondition<'a, &'a str>>,
    default: &'a str,
) -> &'a str {
    match cond {
        Some(graphql_parser::query::TypeCondition::On(name)) => name,
        None => default,
    }
}

/// Same for a named fragment definition, whose type condition is mandatory
/// (`TypeCondition` has a single variant).
fn fragment_def_target<'a>(
    cond: &'a graphql_parser::query::TypeCondition<'a, &'a str>,
    _default: &'a str,
) -> &'a str {
    let graphql_parser::query::TypeCondition::On(name) = cond;
    name
}

// ---------------------------------------------------------------------------
// Schema cache — one introspection fetch (or SDL parse) per process
// ---------------------------------------------------------------------------

/// Cached schema for one endpoint (or SDL file). `None` is the negative
/// entry: introspection was tried and refused — the step then runs
/// unvalidated instead of re-fetching every iteration.
struct SchemaEntry {
    schema: Option<GraphqlSchema>,
}

fn schema_cache() -> &'static Mutex<HashMap<String, SchemaEntry>> {
    static CACHE: OnceLock<Mutex<HashMap<String, SchemaEntry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Resolve the schema for a step: `schema_file` wins when given (with an
/// introspection attempt first only when the file is absent — see below);
/// otherwise introspect the endpoint. Results (including negative ones) are
/// cached process-wide, keyed by URL or file path — hundreds of VUs pay one
/// fetch per run.
async fn resolve_schema(
    params: &Params,
    ctx: &Context,
    client: &reqwest::Client,
) -> Option<GraphqlSchema> {
    // SDL file: parse once per path, cache under `file:<path>`.
    if let Some(path) = &params.schema_file {
        let key = format!("file:{path}");
        if let Some(entry) = schema_cache()
            .lock()
            .unwrap()
            .get(&key)
            .map(|e| e.schema.clone())
        {
            return entry;
        }
        let parsed = match confine_fs_path(ctx, path, false) {
            Ok(p) => match tokio::fs::read_to_string(&p).await {
                Ok(sdl) => schema_from_sdl(&sdl).ok(),
                Err(_) => None,
            },
            Err(_) => None,
        };
        if parsed.is_some() {
            schema_cache().lock().unwrap().insert(
                key,
                SchemaEntry {
                    schema: parsed.clone(),
                },
            );
            return parsed;
        }
        // Fall through to introspection when the SDL file is unusable and
        // introspection is allowed.
    }

    if !params.introspection {
        return None;
    }
    let key = params.url.clone();
    if let Some(entry) = schema_cache()
        .lock()
        .unwrap()
        .get(&key)
        .map(|e| e.schema.clone())
    {
        return entry;
    }
    let fetched = introspect_schema(client, &params.url, &params.headers, params.timeout_ms)
        .await
        .ok();
    schema_cache().lock().unwrap().insert(
        key,
        SchemaEntry {
            schema: fetched.clone(),
        },
    );
    fetched
}

// ---------------------------------------------------------------------------
// The action
// ---------------------------------------------------------------------------

pub(crate) async fn graphql_action(params: &Value, ctx: &Context, step_name: &str) -> ActionOutput {
    let params = match parse_params(params, step_name, ctx).await {
        Ok(p) => p,
        Err(out) => return out,
    };

    // Syntax gate: a malformed document fails without a network call.
    if let Err(msg) = validate_query_syntax(&params.query) {
        return err(step_name, &msg);
    }

    // Operation name: explicit `operation` wins; else a single named
    // operation in the document provides it. Multiple operations require the
    // explicit form (the server rejects an anonymous multi-operation doc).
    let operation = match resolve_operation(&params) {
        Ok(op) => op,
        Err(msg) => return err(step_name, &msg),
    };

    // `${…}` tokens in variables expand per execution — a fresh generator per
    // call, like the one-shot `std/grpc@v1`: `${uuid}`/`${rand}`/`${now}`
    // give per-iteration values (`${seq}` restarts at 1 without a live
    // connection to carry the counter).
    let variables = params.variables.as_ref().map(|v| {
        let mut gen = Gen::new(uuid::Uuid::new_v4().as_u128() as u64);
        gen.begin_message();
        expand_tokens(v, &mut gen)
    });

    let client = http_client(params.pool, params.insecure, ctx.http_client_shard);

    // Schema gate: validate against the cached/fetched schema when available.
    // Unavailability is not a failure — the step runs unvalidated.
    let mut logs: Vec<(LogTag, String)> = Vec::new();
    match resolve_schema(&params, ctx, client).await {
        Some(schema) => {
            if let Err(msg) = validate_against_schema(&schema, &params.query) {
                return err(step_name, &format!("query validation failed: {msg}"));
            }
        }
        None if params.introspection && params.schema_file.is_none() => {
            logs.push((
                LogTag::Sys,
                format!(
                    "{step_name}: introspection unavailable for {} — running unvalidated",
                    params.url
                ),
            ));
        }
        None => {}
    }

    // Build the request: POST carries a JSON body; GET moves the same fields
    // into URL query parameters (CDN-cacheable reads).
    let method = if params.get { "GET" } else { "POST" };
    let mut req = if params.get {
        let mut qp: Vec<(&str, String)> = vec![("query", params.query.clone())];
        if let Some(v) = &variables {
            qp.push(("variables", v.to_string()));
        }
        if let Some(op) = &operation {
            qp.push(("operationName", op.clone()));
        }
        client.get(&params.url).query(&qp)
    } else {
        let mut body = json!({ "query": params.query });
        if let Some(v) = &variables {
            body["variables"] = v.clone();
        }
        if let Some(op) = &operation {
            body["operationName"] = json!(op);
        }
        client.post(&params.url).json(&body)
    };
    req = req.timeout(Duration::from_millis(params.timeout_ms));
    for (k, v) in &params.headers {
        req = req.header(k.as_str(), v.as_str());
    }

    match timed_exchange(req, BodyKind::Text).await {
        Ok(out) => {
            // Decode the GraphQL payload. A non-JSON body (proxy error page,
            // CDN block) has no data — the step fails on that below.
            let parsed: Option<Value> = serde_json::from_str(&out.body).ok();
            let data = parsed.as_ref().and_then(|p| p.get("data")).cloned();
            let errors: Vec<Value> = parsed
                .as_ref()
                .and_then(|p| p.get("errors"))
                .and_then(|e| e.as_array())
                .cloned()
                .unwrap_or_default();

            // The verdict: HTTP failures fail; GraphQL errors without any
            // data fail; partial data with errors passes (counted below).
            let has_data = data.as_ref().is_some_and(|d| !d.is_null());
            let failed = out.status >= 400 || (!errors.is_empty() && !has_data);

            let mut metrics = json!({
                "graphql_req_duration": [out.duration_ms],
                "graphql_errors": errors.len() as u64,
            });
            if let Some(op) = &operation {
                metrics[format!("graphql_op_{op}_duration")] = json!([out.duration_ms]);
            }

            let mut value = json!({
                "status": out.status,
                "data": data.unwrap_or(Value::Null),
                "body": out.body,
                "duration_ms": out.duration_ms,
                "headers": out.headers,
                "metrics": metrics,
            });
            if !errors.is_empty() {
                value["errors"] = json!(errors.clone());
            }

            let extra = format!(
                "{}, graphql_errors={}",
                operation
                    .as_ref()
                    .map(|o| format!(", op={o}"))
                    .unwrap_or_default(),
                errors.len()
            );
            let first_error = errors
                .first()
                .and_then(|e| e["message"].as_str())
                .map(|m| format!(" — {m}"))
                .unwrap_or_default();
            logs.push((
                if failed { LogTag::Err } else { LogTag::Out },
                format!(
                    "{}{}",
                    request_line(
                        method,
                        &params.url,
                        out.status,
                        &out.reason,
                        out.duration_ms,
                        &extra
                    ),
                    if failed { first_error.as_str() } else { "" },
                ),
            ));

            ActionOutput {
                value,
                logs,
                success: !failed,
                http_sample: Some(HttpSample {
                    duration_ms: out.duration_ms,
                    status: out.status,
                    failed,
                }),
            }
        }
        Err((duration_ms, e)) => {
            let mut out = transport_error(step_name, method, &params.url, &e, duration_ms);
            // The introspection sys line, when present, precedes the failure.
            out.logs.splice(0..0, logs);
            out
        }
    }
}

/// Determine the `operationName` to send: the explicit `operation` parameter,
/// or the name of the document's single operation. A document with several
/// operations and no explicit choice is an error (the server would reject it
/// with "must provide operation name").
fn resolve_operation(params: &Params) -> Result<Option<String>, String> {
    if let Some(op) = &params.operation {
        return Ok(Some(op.clone()));
    }
    // Already syntax-checked by the caller.
    let Ok(doc) = graphql_parser::parse_query::<&str>(&params.query) else {
        return Ok(None);
    };
    let ops: Vec<&OperationDefinition<'_, &str>> = doc
        .definitions
        .iter()
        .filter_map(|d| match d {
            QueryDefinition::Operation(op) => Some(op),
            _ => None,
        })
        .collect();
    match ops.as_slice() {
        [only] => Ok(operation_name(only).map(str::to_string)),
        [] => Ok(None),
        _ => Err("document has multiple operations — set 'operation' to pick one".into()),
    }
}

fn operation_name<'a>(op: &'a OperationDefinition<'a, &'a str>) -> Option<&'a str> {
    match op {
        OperationDefinition::Query(q) => q.name,
        OperationDefinition::Mutation(m) => m.name,
        OperationDefinition::Subscription(s) => s.name,
        OperationDefinition::SelectionSet(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::step::actions::execute_action;
    use serde_json::json;
    use wiremock::matchers::{method as wm_method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // ---------------------------------------------------------------
    // Schema construction (SDL + introspection JSON)
    // ---------------------------------------------------------------

    const SDL: &str = r#"
        type Query {
            viewer: Viewer
            search(term: String): [SearchResult!]!
        }
        type Mutation {
            createWidget(name: String!): Widget!
        }
        type Viewer {
            id: ID!
            name: String
            widgets: [Widget!]
        }
        type Widget {
            id: ID!
            name: String!
        }
        union SearchResult = Viewer | Widget
    "#;

    fn sdl_schema() -> GraphqlSchema {
        schema_from_sdl(SDL).unwrap()
    }

    #[test]
    fn sdl_schema_defaults_root_names() {
        let s = sdl_schema();
        assert_eq!(s.query_type.as_deref(), Some("Query"));
        assert_eq!(s.mutation_type.as_deref(), Some("Mutation"));
        assert_eq!(s.field_type("Query", "viewer"), Some("Viewer"));
        assert_eq!(s.field_type("Viewer", "widgets"), Some("Widget"));
        assert_eq!(s.unions["SearchResult"], vec!["Viewer", "Widget"]);
    }

    #[test]
    fn sdl_schema_honours_schema_block() {
        let s = schema_from_sdl("schema { query: RootQ } type RootQ { ok: Boolean }").unwrap();
        assert_eq!(s.query_type.as_deref(), Some("RootQ"));
        assert_eq!(s.field_type("RootQ", "ok"), Some("Boolean"));
    }

    #[test]
    fn sdl_schema_without_query_type_is_an_error() {
        assert!(schema_from_sdl("type Widget { id: ID }").is_err());
        assert!(schema_from_sdl("not graphql {{{").is_err());
    }

    #[test]
    fn introspection_json_builds_schema() {
        let introspection = json!({
            "queryType": { "name": "Query" },
            "mutationType": { "name": "Mutation" },
            "types": [
                { "kind": "OBJECT", "name": "Query", "fields": [
                    { "name": "viewer", "type": { "kind": "OBJECT", "name": "Viewer", "ofType": null } },
                    { "name": "now", "type": { "kind": "NON_NULL", "name": null, "ofType": { "kind": "SCALAR", "name": "String", "ofType": null } } }
                ]},
                { "kind": "OBJECT", "name": "Mutation", "fields": [
                    { "name": "noop", "type": { "kind": "SCALAR", "name": "Boolean", "ofType": null } }
                ]},
                { "kind": "OBJECT", "name": "Viewer", "fields": [
                    { "name": "id", "type": { "kind": "NON_NULL", "name": null, "ofType": { "kind": "SCALAR", "name": "ID", "ofType": null } } }
                ]},
                { "kind": "UNION", "name": "Any", "fields": null, "possibleTypes": [ { "name": "Viewer" } ]},
                { "kind": "OBJECT", "name": "__Schema", "fields": null }
            ]
        });
        let s = schema_from_introspection(&introspection).unwrap();
        assert_eq!(s.query_type.as_deref(), Some("Query"));
        // NON_NULL/LIST wrappers unwrap to the named type.
        assert_eq!(s.field_type("Query", "now"), Some("String"));
        assert_eq!(s.field_type("Query", "viewer"), Some("Viewer"));
        assert_eq!(s.field_type("Viewer", "id"), Some("ID"));
        assert_eq!(s.unions["Any"], vec!["Viewer"]);
        // Introspection internals are excluded.
        assert!(!s.fields.contains_key("__Schema"));
    }

    // ---------------------------------------------------------------
    // Syntax + schema validation
    // ---------------------------------------------------------------

    #[test]
    fn syntax_gate_accepts_and_rejects() {
        assert!(validate_query_syntax("{ viewer { id } }").is_ok());
        assert!(validate_query_syntax("query GetUser { viewer { id } }").is_ok());
        let e = validate_query_syntax("{ viewer { id ").unwrap_err();
        assert!(e.contains("invalid GraphQL syntax"), "got: {e}");
    }

    #[test]
    fn validation_accepts_valid_query() {
        let s = sdl_schema();
        assert!(validate_against_schema(&s, "{ viewer { id name } }").is_ok());
        assert!(
            validate_against_schema(&s, "mutation M { createWidget(name: \"x\") { id } }").is_ok()
        );
    }

    #[test]
    fn validation_unknown_root_field_suggests() {
        let s = sdl_schema();
        let e = validate_against_schema(&s, "{ viewr { id } }").unwrap_err();
        assert!(
            e.contains("unknown field 'viewr' on type 'Query'"),
            "got: {e}"
        );
        assert!(e.contains("did you mean 'viewer'"), "got: {e}");
    }

    #[test]
    fn validation_unknown_nested_field_names_path() {
        let s = sdl_schema();
        let e = validate_against_schema(&s, "{ viewer { widgets { nam } } }").unwrap_err();
        assert!(
            e.contains("unknown field 'nam' on type 'Widget'"),
            "got: {e}"
        );
        assert!(e.contains("did you mean 'name'"), "got: {e}");
    }

    #[test]
    fn validation_composite_needs_selection_set() {
        let s = sdl_schema();
        let e = validate_against_schema(&s, "{ viewer }").unwrap_err();
        assert!(e.contains("needs a selection set"), "got: {e}");
    }

    #[test]
    fn validation_leaf_rejects_selection_set() {
        let s = sdl_schema();
        let e = validate_against_schema(&s, "{ viewer { name { first } } }").unwrap_err();
        assert!(
            e.contains("leaf type 'String' must not have a selection set"),
            "got: {e}"
        );
    }

    #[test]
    fn validation_fragments_and_unions() {
        let s = sdl_schema();
        // Named fragment on an object type.
        let q = "fragment V on Viewer { id } { viewer { ...V } }";
        assert!(validate_against_schema(&s, q).is_ok());
        // Union access via inline fragments.
        let q = "{ search(term: \"x\") { ... on Viewer { id } ... on Widget { name } } }";
        assert!(validate_against_schema(&s, q).is_ok());
        // Direct field on a union is rejected.
        let e = validate_against_schema(&s, "{ search(term: \"x\") { id } }").unwrap_err();
        assert!(
            e.contains("union type 'SearchResult' has no field 'id'"),
            "got: {e}"
        );
        // Unknown fragment is rejected.
        let e = validate_against_schema(&s, "{ viewer { ...Missing } }").unwrap_err();
        assert!(e.contains("unknown fragment 'Missing'"), "got: {e}");
    }

    #[test]
    fn validation_variables_must_be_defined() {
        let s = sdl_schema();
        assert!(validate_against_schema(
            &s,
            "mutation M($n: String!) { createWidget(name: $n) { id } }"
        )
        .is_ok());
        let e = validate_against_schema(&s, "mutation M { createWidget(name: $n) { id } }")
            .unwrap_err();
        assert!(
            e.contains("variable '$n' is used but not defined"),
            "got: {e}"
        );
    }

    #[test]
    fn validation_rejects_subscriptions() {
        let s = sdl_schema();
        let e = validate_against_schema(&s, "subscription { viewer { id } }").unwrap_err();
        assert!(e.contains("subscriptions are not supported"), "got: {e}");
    }

    #[test]
    fn validation_mutation_root_absent() {
        let s = schema_from_sdl("type Query { ok: Boolean }").unwrap();
        let e = validate_against_schema(&s, "mutation { ok }").unwrap_err();
        assert!(e.contains("no mutation root type"), "got: {e}");
    }

    #[test]
    fn resolve_operation_rules() {
        let p = |query: &str, operation: Option<&str>| Params {
            url: "http://x".into(),
            query: query.into(),
            variables: None,
            operation: operation.map(str::to_string),
            get: false,
            headers: vec![],
            timeout_ms: 1000,
            insecure: false,
            pool: ClientPool::PerVu,
            introspection: false,
            schema_file: None,
        };
        assert_eq!(
            resolve_operation(&p("query GetUser { viewer { id } }", None)).unwrap(),
            Some("GetUser".into())
        );
        assert_eq!(
            resolve_operation(&p("{ viewer { id } }", None)).unwrap(),
            None
        );
        assert!(resolve_operation(&p(
            "query A { viewer { id } } query B { viewer { id } }",
            None
        ))
        .is_err());
        assert_eq!(
            resolve_operation(&p(
                "query A { viewer { id } } query B { viewer { id } }",
                Some("B")
            ))
            .unwrap(),
            Some("B".into())
        );
    }

    // ---------------------------------------------------------------
    // HTTP behaviour against a mock server
    // ---------------------------------------------------------------

    fn no_introspection(url: &str, query: &str) -> Value {
        json!({
            "url": url,
            "query": query,
            "introspection": false,
        })
    }

    #[tokio::test]
    async fn posts_document_variables_and_operation() {
        let server = MockServer::start().await;
        Mock::given(wm_method("POST"))
            .and(path("/graphql"))
            .and(wiremock::matchers::body_json(json!({
                "query": "query GetUser { viewer { id } }",
                "variables": { "id": "42" },
                "operationName": "GetUser"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "viewer": { "id": "42" } }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("{}/graphql", server.uri());
        let mut params = no_introspection(&url, "query GetUser { viewer { id } }");
        params["variables"] = json!({ "id": "42" });
        params["operation"] = json!("GetUser");
        let out = execute_action("std/graphql@v1", &params, &Context::new(), "step").await;

        assert!(out.success, "logs: {:?}", out.logs);
        assert_eq!(out.value["status"], 200);
        assert_eq!(out.value["data"]["viewer"]["id"], "42");
        assert!(out.value.get("errors").is_none());
        assert!(out.value["body"].as_str().unwrap().contains("viewer"));
        assert!(out.value["metrics"]["graphql_req_duration"].is_array());
        assert_eq!(out.value["metrics"]["graphql_errors"], 0);
        assert!(out.value["metrics"]["graphql_op_GetUser_duration"].is_array());
        assert!(out.http_sample.is_some());
        server.verify().await;
    }

    #[tokio::test]
    async fn get_method_uses_query_params() {
        let server = MockServer::start().await;
        Mock::given(wm_method("GET"))
            .and(path("/graphql"))
            .and(query_param("query", "{ viewer { id } }"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "viewer": { "id": "1" } }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("{}/graphql", server.uri());
        let mut params = no_introspection(&url, "{ viewer { id } }");
        params["method"] = json!("GET");
        let out = execute_action("std/graphql@v1", &params, &Context::new(), "step").await;
        assert!(out.success, "logs: {:?}", out.logs);
        server.verify().await;
    }

    #[tokio::test]
    async fn custom_headers_are_forwarded() {
        let server = MockServer::start().await;
        Mock::given(wm_method("POST"))
            .and(wiremock::matchers::header("authorization", "Bearer t"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": {} })))
            .expect(1)
            .mount(&server)
            .await;

        let mut params = no_introspection(&server.uri(), "{ a }");
        params["headers"] = json!({ "authorization": "Bearer t" });
        let out = execute_action("std/graphql@v1", &params, &Context::new(), "step").await;
        assert!(out.success, "logs: {:?}", out.logs);
        server.verify().await;
    }

    #[tokio::test]
    async fn graphql_errors_without_data_fail_the_step() {
        let server = MockServer::start().await;
        Mock::given(wm_method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": null,
                "errors": [ { "message": "Cannot query field \"nope\"" } ]
            })))
            .mount(&server)
            .await;

        let params = no_introspection(&server.uri(), "{ nope }");
        let out = execute_action("std/graphql@v1", &params, &Context::new(), "step").await;
        assert!(!out.success);
        assert_eq!(out.value["metrics"]["graphql_errors"], 1);
        assert!(out.value["errors"].is_array());
        assert!(out.http_sample.as_ref().unwrap().failed);
        let line = &out.logs.last().unwrap().1;
        assert!(line.contains("Cannot query field"), "got: {line}");
    }

    #[tokio::test]
    async fn partial_data_with_errors_passes_but_counts() {
        let server = MockServer::start().await;
        Mock::given(wm_method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "viewer": { "id": "7" } },
                "errors": [ { "message": "widgets timed out", "path": ["viewer", "widgets"] } ]
            })))
            .mount(&server)
            .await;

        let params = no_introspection(&server.uri(), "{ viewer { id } }");
        let out = execute_action("std/graphql@v1", &params, &Context::new(), "step").await;
        assert!(out.success, "partial data passes: {:?}", out.logs);
        assert_eq!(out.value["data"]["viewer"]["id"], "7");
        assert_eq!(out.value["metrics"]["graphql_errors"], 1);
        assert!(!out.http_sample.as_ref().unwrap().failed);
    }

    #[tokio::test]
    async fn http_500_fails() {
        let server = MockServer::start().await;
        Mock::given(wm_method("POST"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let params = no_introspection(&server.uri(), "{ a }");
        let out = execute_action("std/graphql@v1", &params, &Context::new(), "step").await;
        assert!(!out.success);
        assert_eq!(out.value["status"], 500);
        assert!(out.http_sample.as_ref().unwrap().failed);
    }

    #[tokio::test]
    async fn syntax_error_skips_the_network() {
        let server = MockServer::start().await;
        // No mocks: any request would 404. The step must fail before that.
        let params = no_introspection(&server.uri(), "{ broken {{ ");
        let out = execute_action("std/graphql@v1", &params, &Context::new(), "step").await;
        assert!(!out.success);
        assert!(out.logs[0].1.contains("invalid GraphQL syntax"));
        assert!(out.http_sample.is_none(), "no request was made");
    }

    #[tokio::test]
    async fn param_validation_errors() {
        let ctx = Context::new();
        for (params, needle) in [
            (
                json!({ "query": "{ a }", "introspection": false }),
                "'url' is required",
            ),
            (
                json!({ "url": "http://x", "introspection": false }),
                "'query' or 'query_file' is required",
            ),
            (
                json!({ "url": "http://x", "query": "{ a }", "query_file": "q.graphql", "introspection": false }),
                "mutually exclusive",
            ),
            (
                json!({ "url": "http://x", "query": "{ a }", "method": "PUT", "introspection": false }),
                "'method' must be POST or GET",
            ),
            (
                json!({ "url": "http://x", "query": "{ a }", "pool": "fancy", "introspection": false }),
                "invalid pool 'fancy'",
            ),
            (
                json!({ "url": "http://x", "query": "{ a }", "variables": [1], "introspection": false }),
                "'variables' must be a JSON object",
            ),
        ] {
            let out = execute_action("std/graphql@v1", &params, &ctx, "step").await;
            assert!(!out.success);
            assert!(
                out.logs.iter().any(|(_, l)| l.contains(needle)),
                "expected '{needle}' in {:?}",
                out.logs
            );
        }
    }

    #[tokio::test]
    async fn introspection_unavailable_runs_unvalidated_with_sys_line() {
        // The mock answers everything with 404 HTML — introspection fails,
        // the actual query then runs without validation (negative cache).
        let server = MockServer::start().await;
        Mock::given(wm_method("POST"))
            .and(wiremock::matchers::body_string_contains(
                "IntrospectionQuery",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "errors": [ { "message": "introspection is disabled" } ]
            })))
            .mount(&server)
            .await;
        Mock::given(wm_method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": { "a": 1 } })))
            .mount(&server)
            .await;

        // Introspection left at its default (true).
        let params = json!({ "url": server.uri(), "query": "{ a }" });
        let out = execute_action("std/graphql@v1", &params, &Context::new(), "step").await;
        assert!(out.success, "logs: {:?}", out.logs);
        assert!(
            out.logs
                .iter()
                .any(|(t, l)| *t == LogTag::Sys && l.contains("running unvalidated")),
            "sys line about unvalidated run: {:?}",
            out.logs
        );
    }

    #[tokio::test]
    async fn variables_expand_generator_tokens() {
        let server = MockServer::start().await;
        Mock::given(wm_method("POST"))
            .and(wiremock::matchers::body_string_contains("renamed-"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": {} })))
            .expect(1)
            .mount(&server)
            .await;

        let mut params = no_introspection(
            &server.uri(),
            "mutation M($n: String!) { rename(name: $n) }",
        );
        params["variables"] = json!({ "n": "renamed-${uuid}" });
        let out = execute_action("std/graphql@v1", &params, &Context::new(), "step").await;
        assert!(out.success, "logs: {:?}", out.logs);
        // The token expanded — the literal "${uuid}" never reached the wire.
        let requests = server.received_requests().await.unwrap();
        let body = std::str::from_utf8(&requests[0].body).unwrap();
        assert!(!body.contains("${uuid}"), "token left verbatim: {body}");
        assert!(body.contains("renamed-"), "got: {body}");
        server.verify().await;
    }

    #[tokio::test]
    async fn query_file_reads_document_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("get-user.graphql");
        std::fs::write(&file, "{ viewer { id } }").unwrap();

        let server = MockServer::start().await;
        Mock::given(wm_method("POST"))
            .and(wiremock::matchers::body_string_contains("viewer"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": {} })))
            .expect(1)
            .mount(&server)
            .await;

        let mut ctx = Context::new();
        ctx.allow_file_actions = true;
        let params = json!({
            "url": server.uri(),
            "query_file": file.to_string_lossy(),
            "introspection": false,
        });
        let out = execute_action("std/graphql@v1", &params, &ctx, "step").await;
        assert!(out.success, "logs: {:?}", out.logs);
        server.verify().await;
    }

    #[tokio::test]
    async fn query_file_requires_file_actions() {
        let params = json!({
            "url": "http://localhost:1",
            "query_file": "anything.graphql",
            "introspection": false,
        });
        let out = execute_action("std/graphql@v1", &params, &Context::new(), "step").await;
        assert!(!out.success);
        assert!(out.logs[0].1.contains("file actions disabled"));
    }

    // ---------------------------------------------------------------
    // Real GraphQL server (async-graphql): introspection + validation
    // ---------------------------------------------------------------

    struct Viewer {
        id: String,
    }

    #[async_graphql::Object]
    impl Viewer {
        async fn id(&self) -> &str {
            &self.id
        }
        async fn name(&self) -> &str {
            "Ada"
        }
    }

    struct TestQuery;

    #[async_graphql::Object]
    impl TestQuery {
        async fn viewer(&self) -> Viewer {
            Viewer { id: "u-1".into() }
        }
    }

    struct TestMutation;

    #[async_graphql::Object]
    impl TestMutation {
        async fn create_widget(&self, name: String) -> String {
            format!("widget-{name}")
        }
    }

    /// Spin up a real GraphQL server on an ephemeral port; returns its
    /// /graphql URL. Introspection is enabled (async-graphql default).
    async fn graphql_server() -> String {
        use async_graphql::{EmptySubscription, Schema};
        let schema = Schema::build(TestQuery, TestMutation, EmptySubscription).finish();
        let app = axum::Router::new().route(
            "/graphql",
            axum::routing::get_service(async_graphql_axum::GraphQL::new(schema.clone()))
                .post_service(async_graphql_axum::GraphQL::new(schema)),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}/graphql")
    }

    #[tokio::test]
    async fn real_server_valid_query_passes_introspection_gate() {
        let url = graphql_server().await;
        let params = json!({
            "url": url,
            "query": "query GetViewer { viewer { id name } }",
        });
        let out = execute_action("std/graphql@v1", &params, &Context::new(), "step").await;
        assert!(out.success, "logs: {:?}", out.logs);
        assert_eq!(out.value["data"]["viewer"]["name"], "Ada");
        assert!(out.value["metrics"]["graphql_op_GetViewer_duration"].is_array());
    }

    #[tokio::test]
    async fn real_server_invalid_query_fails_before_resolving() {
        let url = graphql_server().await;
        let params = json!({
            "url": url,
            "query": "{ viewr { id } }",
        });
        let out = execute_action("std/graphql@v1", &params, &Context::new(), "step").await;
        assert!(!out.success);
        let line = &out.logs[0].1;
        assert!(line.contains("query validation failed"), "got: {line}");
        assert!(line.contains("did you mean 'viewer'"), "got: {line}");
        assert!(out.http_sample.is_none(), "rejected before any request");
    }

    #[tokio::test]
    async fn real_server_mutation_and_get() {
        let url = graphql_server().await;
        let params = json!({
            "url": url,
            "query": "mutation Create($n: String!) { createWidget(name: $n) }",
            "variables": { "n": "sprocket" },
        });
        let out = execute_action("std/graphql@v1", &params, &Context::new(), "step").await;
        assert!(out.success, "logs: {:?}", out.logs);
        assert_eq!(out.value["data"]["createWidget"], "widget-sprocket");

        let params = json!({
            "url": url,
            "query": "{ viewer { id } }",
            "method": "GET",
        });
        let out = execute_action("std/graphql@v1", &params, &Context::new(), "step").await;
        assert!(out.success, "GET logs: {:?}", out.logs);
        assert_eq!(out.value["data"]["viewer"]["id"], "u-1");
    }
}
