use std::collections::{BTreeMap, HashMap};

use perfscale_core::step::{parse_duration_ms_strict, thresholds};
use perfscale_core::yaml::{parse_config_file, parse_test_file};
use serde_json::{json, Map, Number, Value};
use tree_sitter::{Node, Parser};

#[derive(Debug)]
pub struct Converted {
    pub test_yaml: String,
    pub config_yaml: String,
}

pub fn convert(source: &str) -> Result<Converted, String> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_javascript::LANGUAGE.into())
        .map_err(|e| format!("initialize JavaScript parser: {e}"))?;
    let tree = parser
        .parse(source, None)
        .ok_or("JavaScript parser timed out")?;
    let root = tree.root_node();
    if root.has_error() {
        let error = first_error(root).unwrap_or(root);
        return Err(format!(
            "line {}: invalid JavaScript syntax",
            error.start_position().row + 1
        ));
    }

    let mut converter = Converter::new(source);
    let mut options = None;
    let mut default_function = None;
    for node in named(root) {
        match node.kind() {
            "comment" | "empty_statement" => {}
            "import_statement" => converter.import(node)?,
            "export_statement" => {
                if named(node).iter().any(|n| n.kind() == "lexical_declaration") {
                    let declaration = named(node).into_iter().find(|n| n.kind() == "lexical_declaration").unwrap();
                    for declarator in descendants(declaration, "variable_declarator") {
                        let name = field(declarator, "name").map(|n| converter.text(n)).unwrap_or("");
                        if name == "options" {
                            if options.is_some() {
                                return Err(converter.err(declarator, "duplicate options export"));
                            }
                            options = field(declarator, "value");
                        } else {
                            return Err(converter.err(declarator, "only `export const options` and the default function are supported"));
                        }
                    }
                } else if converter.text(node).starts_with("export default") {
                    let function = named(node).into_iter().find(|n| matches!(n.kind(), "function_declaration" | "function_expression" | "arrow_function"));
                    if function.is_none() {
                        return Err(converter.err(node, "default export must be a function"));
                    }
                    default_function = function;
                } else {
                    return Err(converter.err(node, "setup/teardown and named exports are not convertible"));
                }
            }
            "lexical_declaration" | "variable_declaration" => converter.declare_constants(node)?,
            "expression_statement" if converter.text(node).trim_start().starts_with("'use strict'")
                || converter.text(node).trim_start().starts_with("\"use strict\"") => {}
            _ => return Err(converter.err(node, "unsupported top-level JavaScript; use static const values, options, imports, and one default function")),
        }
    }
    let options = options
        .ok_or("line 1: `export const options` with an explicit load profile is required")?;
    let default_function = default_function.ok_or("line 1: missing `export default function`")?;
    let mut config = converter.convert_options(options)?;
    converter.convert_default(default_function)?;
    if converter.steps.is_empty() {
        return Err(converter.err(
            default_function,
            "the default function has no convertible HTTP or sleep steps",
        ));
    }
    if !converter.after.is_empty() {
        config
            .as_object_mut()
            .unwrap()
            .insert("after".into(), Value::Array(converter.after.clone()));
    }
    let test = json!({ "steps": converter.steps });
    let test_yaml = serde_yaml::to_string(&test).map_err(|e| e.to_string())?;
    let config_yaml = serde_yaml::to_string(&config).map_err(|e| e.to_string())?;
    parse_test_file(&test_yaml).map_err(|e| format!("generated test is invalid: {e}"))?;
    parse_config_file(&config_yaml).map_err(|e| format!("generated config is invalid: {e}"))?;
    Ok(Converted {
        test_yaml,
        config_yaml,
    })
}

struct Converter<'tree, 'source> {
    source: &'source str,
    constants: HashMap<String, Node<'tree>>,
    http_alias: String,
    check_alias: String,
    sleep_alias: String,
    group_alias: String,
    steps: Vec<Value>,
    responses: HashMap<String, usize>,
    groups: Vec<String>,
    after: Vec<Value>,
    insecure: bool,
}

impl<'tree, 'source> Converter<'tree, 'source> {
    fn new(source: &'source str) -> Self {
        Self {
            source,
            constants: HashMap::new(),
            http_alias: "http".into(),
            check_alias: "check".into(),
            sleep_alias: "sleep".into(),
            group_alias: "group".into(),
            steps: Vec::new(),
            responses: HashMap::new(),
            groups: Vec::new(),
            after: Vec::new(),
            insecure: false,
        }
    }

    fn text(&self, node: Node<'tree>) -> &'source str {
        &self.source[node.byte_range()]
    }

    fn err(&self, node: Node<'tree>, message: impl AsRef<str>) -> String {
        format!(
            "line {}:{}: {}",
            node.start_position().row + 1,
            node.start_position().column + 1,
            message.as_ref()
        )
    }

    fn import(&mut self, node: Node<'tree>) -> Result<(), String> {
        let module_node = named(node)
            .into_iter()
            .rev()
            .find(|n| n.kind() == "string")
            .ok_or_else(|| self.err(node, "import must name a static module"))?;
        let module = self.string_value(module_node)?;
        let clause = named(node)
            .into_iter()
            .find(|n| n.kind() == "import_clause");
        match module.as_str() {
            "k6/http" => {
                let alias = clause
                    .and_then(|c| named(c).into_iter().find(|n| n.kind() == "identifier"))
                    .map(|n| self.text(n).to_string())
                    .ok_or_else(|| self.err(node, "k6/http needs a default import"))?;
                self.http_alias = alias;
            }
            "k6" => {
                let Some(imports) = clause.and_then(|c| {
                    descendants(c, "import_specifier")
                        .into_iter()
                        .next()
                        .map(|_| c)
                }) else {
                    return Err(self.err(node, "k6 import must name check, sleep, or group"));
                };
                for specifier in descendants(imports, "import_specifier") {
                    let ids = named(specifier)
                        .into_iter()
                        .filter(|n| n.kind() == "identifier")
                        .collect::<Vec<_>>();
                    let imported = ids.first().map(|n| self.text(*n)).unwrap_or("");
                    let local = ids
                        .last()
                        .map(|n| self.text(*n))
                        .unwrap_or(imported)
                        .to_string();
                    match imported {
                        "check" => self.check_alias = local,
                        "sleep" => self.sleep_alias = local,
                        "group" => self.group_alias = local,
                        _ => {
                            return Err(self.err(
                                specifier,
                                format!("k6 import `{imported}` is not supported"),
                            ))
                        }
                    }
                }
            }
            _ => {
                return Err(self.err(
                    node,
                    format!(
                        "module `{module}` is not supported; xk6 extensions stay on the k6 runner"
                    ),
                ))
            }
        }
        Ok(())
    }

    fn declare_constants(&mut self, node: Node<'tree>) -> Result<(), String> {
        for declarator in named(node)
            .into_iter()
            .filter(|n| n.kind() == "variable_declarator")
        {
            let name_node = field(declarator, "name").unwrap();
            let name = self.text(name_node).to_string();
            if name_node.kind() != "identifier" || self.constants.contains_key(&name) {
                return Err(self.err(name_node, "constants need unique identifier names"));
            }
            let value = field(declarator, "value")
                .ok_or_else(|| self.err(declarator, "constant needs a value"))?;
            self.constants.insert(name, value);
        }
        Ok(())
    }

    fn convert_default(&mut self, function: Node<'tree>) -> Result<(), String> {
        let body = field(function, "body")
            .ok_or_else(|| self.err(function, "default function has no body"))?;
        self.convert_body(body)
    }

    fn convert_body(&mut self, body: Node<'tree>) -> Result<(), String> {
        if body.kind() != "statement_block" {
            return self.convert_expression(body).map(|_| ());
        }
        for statement in named(body) {
            match statement.kind() {
                "comment" | "empty_statement" => {}
                "lexical_declaration" | "variable_declaration" => {
                    for declarator in named(statement).into_iter().filter(|n| n.kind() == "variable_declarator") {
                        let name_node = field(declarator, "name").unwrap();
                        if name_node.kind() != "identifier" {
                            return Err(self.err(name_node, "destructuring is not supported"));
                        }
                        let name = self.text(name_node).to_string();
                        if self.constants.contains_key(&name) || self.responses.contains_key(&name) {
                            return Err(self.err(name_node, "variable shadowing is not supported; rename the declaration so every response/constant is unique"));
                        }
                        let value = field(declarator, "value").ok_or_else(|| self.err(declarator, "variable needs a value"))?;
                        if self.is_http_call(value) {
                            self.convert_http(value, Some(&name))?;
                        } else {
                            self.constants.insert(name, value);
                        }
                    }
                }
                "expression_statement" => {
                    let expression = named(statement).into_iter().next().ok_or_else(|| self.err(statement, "empty expression"))?;
                    self.convert_expression(expression)?;
                }
                "return_statement" if named(statement).is_empty() => {}
                _ => return Err(self.err(statement, format!("unsupported statement `{}`; conditions, loops, helper calls, and assignments must be rewritten explicitly", statement.kind()))),
            }
        }
        Ok(())
    }

    fn convert_expression(&mut self, expression: Node<'tree>) -> Result<(), String> {
        let expression = self.unwrap(expression);
        if expression.kind() != "call_expression" {
            return Err(self.err(expression, "only k6 call expressions are supported"));
        }
        if self.is_http_call(expression) {
            self.convert_http(expression, None)?;
            return Ok(());
        }
        let function = field(expression, "function").unwrap();
        if function.kind() != "identifier" {
            return Err(self.err(expression, "unsupported computed k6 call"));
        }
        let name = self.text(function);
        let args = self.arguments(expression);
        if name == self.sleep_alias {
            if args.len() != 1 {
                return Err(self.err(expression, "sleep needs exactly one numeric argument"));
            }
            let seconds = self.number(args[0])?;
            let milliseconds = seconds * 1000.0;
            let rounded = milliseconds.round();
            if !milliseconds.is_finite()
                || seconds < 0.0
                || (milliseconds - rounded).abs() > 0.000_001
            {
                return Err(self.err(
                    args[0],
                    "sleep must be a non-negative whole number of milliseconds",
                ));
            }
            self.push_step(
                "sleep",
                "std/sleep@v1",
                json!({ "ms": rounded as u64 }),
                None,
            );
            return Ok(());
        }
        if name == self.check_alias {
            return self.convert_check(expression);
        }
        if name == self.group_alias {
            if args.len() != 2 {
                return Err(self.err(expression, "group needs a static name and a function"));
            }
            let group = self.string(args[0])?;
            let body = field(args[1], "body")
                .ok_or_else(|| self.err(args[1], "group callback must be a function"))?;
            self.groups.push(group);
            let result = self.convert_body(body);
            self.groups.pop();
            return result;
        }
        Err(self.err(expression, format!("call `{name}` is not supported")))
    }

    fn is_http_call(&self, expression: Node<'tree>) -> bool {
        let expression = self.unwrap(expression);
        if expression.kind() != "call_expression" {
            return false;
        }
        let Some(function) = field(expression, "function") else {
            return false;
        };
        if function.kind() != "member_expression" {
            return false;
        }
        field(function, "object")
            .map(|n| self.text(n) == self.http_alias)
            .unwrap_or(false)
            && field(function, "property")
                .map(|n| {
                    matches!(
                        self.text(n),
                        "get"
                            | "post"
                            | "put"
                            | "patch"
                            | "del"
                            | "delete"
                            | "head"
                            | "options"
                            | "request"
                    )
                })
                .unwrap_or(false)
    }

    fn convert_http(&mut self, call: Node<'tree>, output: Option<&str>) -> Result<usize, String> {
        let function = field(call, "function").unwrap();
        let helper = self.text(field(function, "property").unwrap());
        let args = self.arguments(call);
        let (method, url_index, body_index, params_index) = match helper {
            "get" | "head" | "options" => (helper.to_uppercase(), 0, None, 1),
            "del" | "delete" => ("DELETE".into(), 0, None, 1),
            "post" | "put" | "patch" => (helper.to_uppercase(), 0, Some(1), 2),
            "request" => {
                if args.is_empty() {
                    return Err(self.err(call, "http.request needs a method"));
                }
                (self.string(args[0])?.to_uppercase(), 1, Some(2), 3)
            }
            _ => return Err(self.err(call, "unsupported HTTP helper")),
        };
        if args.len() <= url_index {
            return Err(self.err(call, "HTTP request needs a URL"));
        }
        let url = self.string(args[url_index])?;
        if args.len() > params_index + 1 {
            return Err(self.err(call, "too many HTTP arguments"));
        }
        let mut with = Map::new();
        with.insert("method".into(), Value::String(method.clone()));
        with.insert("url".into(), Value::String(url.clone()));
        if let Some(index) = body_index {
            if let Some(body) = args.get(index) {
                let body = self.value(*body, 0)?;
                if !body.is_null() {
                    with.insert("body".into(), body);
                }
            }
        }
        if let Some(params) = args.get(params_index) {
            let value = self.value(*params, 0)?;
            let object = value
                .as_object()
                .ok_or_else(|| self.err(*params, "HTTP params must be a static object"))?;
            for (key, value) in object {
                match key.as_str() {
                    "headers" => {
                        let headers = value
                            .as_object()
                            .ok_or_else(|| self.err(*params, "headers must be an object"))?;
                        if headers.values().any(|v| !v.is_string()) {
                            return Err(self.err(*params, "header values must be strings"));
                        }
                        with.insert("headers".into(), value.clone());
                    }
                    "timeout" => {
                        let timeout = match value {
                            Value::Number(n) => n.as_u64(),
                            Value::String(s) => Some(
                                parse_duration_ms_strict(s).map_err(|e| self.err(*params, e))?,
                            ),
                            _ => None,
                        }
                        .ok_or_else(|| {
                            self.err(*params, "timeout must be milliseconds or a duration string")
                        })?;
                        with.insert("timeout".into(), json!(timeout));
                    }
                    _ => {
                        return Err(
                            self.err(*params, format!("HTTP param `{key}` is not supported"))
                        )
                    }
                }
            }
        }
        if self.insecure {
            with.insert("insecure".into(), Value::Bool(true));
        }
        let index = self.push_step(
            &format!("{method} {url}"),
            "std/http@v1",
            Value::Object(with),
            output,
        );
        if let Some(name) = output {
            self.responses.insert(name.to_string(), index);
        }
        Ok(index)
    }

    fn push_step(
        &mut self,
        default_name: &str,
        action: &str,
        with: Value,
        output: Option<&str>,
    ) -> usize {
        let name = if self.groups.is_empty() {
            default_name.to_string()
        } else {
            format!("{}: {default_name}", self.groups.join(" / "))
        };
        let mut step = Map::new();
        step.insert("name".into(), Value::String(name));
        step.insert("use".into(), Value::String(action.into()));
        step.insert("with".into(), with);
        if let Some(output) = output {
            step.insert("outputs".into(), Value::String(output.into()));
        }
        self.steps.push(Value::Object(step));
        self.steps.len() - 1
    }

    fn convert_check(&mut self, call: Node<'tree>) -> Result<(), String> {
        let args = self.arguments(call);
        if args.len() != 2 {
            return Err(self.err(call, "check needs a response and an object of predicates"));
        }
        let first = self.unwrap(args[0]);
        let step_index = if first.kind() == "identifier" {
            let name = self.text(first);
            *self
                .responses
                .get(name)
                .ok_or_else(|| self.err(first, format!("unknown HTTP response `{name}`")))?
        } else if self.is_http_call(first) {
            self.convert_http(first, None)?
        } else {
            return Err(self.err(
                first,
                "check target must be a captured HTTP response or an HTTP call",
            ));
        };
        let predicates = self.unwrap(args[1]);
        if predicates.kind() != "object" {
            return Err(self.err(predicates, "check predicates must be an object"));
        }
        let mut converted = Map::new();
        for pair in named(predicates) {
            if pair.kind() != "pair" {
                return Err(self.err(
                    pair,
                    "spread and computed check predicates are not supported",
                ));
            }
            let predicate = field(pair, "value").unwrap();
            let params = field(predicate, "parameters")
                .map(named)
                .unwrap_or_default();
            let parameter = params.first().map(|n| self.text(*n)).unwrap_or("r");
            let body = field(predicate, "body")
                .ok_or_else(|| self.err(predicate, "check predicate must be an arrow/function"))?;
            let expression = if body.kind() == "statement_block" {
                let statements = named(body);
                if statements.len() != 1 || statements[0].kind() != "return_statement" {
                    return Err(self.err(body, "check function must contain one return"));
                }
                named(statements[0])
                    .into_iter()
                    .next()
                    .ok_or_else(|| self.err(body, "check must return a predicate"))?
            } else {
                body
            };
            self.predicate(expression, parameter, &mut converted)?;
        }
        let step = self.steps[step_index].as_object_mut().unwrap();
        let existing = step
            .entry("check")
            .or_insert_with(|| Value::Object(Map::new()));
        existing.as_object_mut().unwrap().extend(converted);
        Ok(())
    }

    fn predicate(
        &self,
        expression: Node<'tree>,
        parameter: &str,
        output: &mut Map<String, Value>,
    ) -> Result<(), String> {
        let expression = self.unwrap(expression);
        if expression.kind() == "binary_expression" {
            let left = field(expression, "left").unwrap();
            let right = field(expression, "right").unwrap();
            let operator = self.source[left.end_byte()..right.start_byte()].trim();
            if operator == "&&" {
                self.predicate(left, parameter, output)?;
                return self.predicate(right, parameter, output);
            }
            if !matches!(operator, "===" | "==") {
                return Err(self.err(
                    expression,
                    "only status equality, body includes, and && are supported in checks",
                ));
            }
            let (status_node, expected) = if self.is_member(left, parameter, "status") {
                (right, true)
            } else if self.is_member(right, parameter, "status") {
                (left, true)
            } else {
                (left, false)
            };
            if expected {
                let status = self.number(status_node)?;
                if status < 0.0 || status.fract() != 0.0 || status > 999.0 {
                    return Err(self.err(status_node, "HTTP status must be an integer"));
                }
                output.insert("status".into(), json!(status as u64));
                return Ok(());
            }
        }
        if expression.kind() == "call_expression" {
            let function = field(expression, "function").unwrap();
            if function.kind() == "member_expression"
                && field(function, "property")
                    .map(|n| self.text(n) == "includes")
                    .unwrap_or(false)
                && field(function, "object")
                    .map(|n| self.is_member(n, parameter, "body"))
                    .unwrap_or(false)
            {
                let args = self.arguments(expression);
                if args.len() == 1 {
                    output.insert("body_contains".into(), Value::String(self.string(args[0])?));
                    return Ok(());
                }
            }
        }
        Err(self.err(
            expression,
            "only `r.status === N` and `r.body.includes(\"text\")` checks are supported",
        ))
    }

    fn is_member(&self, node: Node<'tree>, object: &str, property: &str) -> bool {
        let node = self.unwrap(node);
        node.kind() == "member_expression"
            && field(node, "object")
                .map(|n| self.text(self.unwrap(n)) == object)
                .unwrap_or(false)
            && field(node, "property")
                .map(|n| self.text(n) == property)
                .unwrap_or(false)
    }

    fn convert_options(&mut self, node: Node<'tree>) -> Result<Value, String> {
        let object = self.unwrap(node);
        if object.kind() != "object" {
            return Err(self.err(object, "options must be a static object"));
        }
        let mut vus = None;
        let mut duration = None;
        let mut stages = None;
        let mut scenarios = None;
        let mut threshold_node = None;
        for pair in named(object) {
            if pair.kind() != "pair" {
                return Err(self.err(pair, "options spread/methods are not supported"));
            }
            let key = self.key(pair)?;
            let value = field(pair, "value").unwrap();
            match key.as_str() {
                "vus" => vus = Some(self.u32(value)?),
                "duration" => {
                    let parsed = self.string(value)?;
                    parse_duration_ms_strict(&parsed).map_err(|e| self.err(value, e))?;
                    duration = Some(parsed);
                }
                "stages" => stages = Some(self.convert_stages(value)?),
                "scenarios" => scenarios = Some(value),
                "thresholds" => threshold_node = Some(value),
                "insecureSkipTLSVerify" => self.insecure = self.boolean(value)?,
                _ => {
                    return Err(self.err(
                        value,
                        format!("option `{key}` is not supported by the native converter"),
                    ))
                }
            }
        }
        if let Some(thresholds) = threshold_node {
            self.convert_thresholds(thresholds)?;
        }
        let fixed = match (vus, duration) {
            (Some(vus), Some(duration)) => Some((vus, duration)),
            (None, None) => None,
            _ => return Err(self.err(object, "fixed load needs both vus and duration")),
        };
        let mut config = Map::new();
        let shapes = usize::from(fixed.is_some())
            + usize::from(stages.is_some())
            + usize::from(scenarios.is_some());
        if shapes != 1 {
            return Err(self.err(object, "declare exactly one load shape: vus+duration, stages, or one ramping-arrival-rate scenario"));
        }
        if let Some((vus, duration)) = fixed {
            config.insert("vus".into(), json!(vus));
            config.insert("duration".into(), Value::String(duration));
        } else if let Some(stages) = stages {
            config.insert("stages".into(), stages);
        } else {
            self.convert_arrival(scenarios.unwrap(), &mut config)?;
        }
        Ok(Value::Object(config))
    }

    fn convert_stages(&mut self, node: Node<'tree>) -> Result<Value, String> {
        let stages = self.value(node, 0)?;
        let stages = stages
            .as_array()
            .ok_or_else(|| self.err(node, "stages must be an array"))?;
        if stages.is_empty() {
            return Err(self.err(node, "stages cannot be empty"));
        }
        for stage in stages {
            let object = stage
                .as_object()
                .ok_or_else(|| self.err(node, "stage must be an object"))?;
            if object.len() != 2 {
                return Err(self.err(node, "stage accepts only duration and target"));
            }
            let duration = object
                .get("duration")
                .and_then(Value::as_str)
                .ok_or_else(|| self.err(node, "stage needs duration"))?;
            parse_duration_ms_strict(duration).map_err(|e| self.err(node, e))?;
            if !object
                .get("target")
                .and_then(Value::as_u64)
                .is_some_and(|n| n <= u32::MAX as u64)
            {
                return Err(self.err(node, "stage target must be a non-negative integer"));
            }
        }
        Ok(Value::Array(stages.clone()))
    }

    fn convert_arrival(
        &mut self,
        node: Node<'tree>,
        config: &mut Map<String, Value>,
    ) -> Result<(), String> {
        let scenarios = self.unwrap(node);
        let pairs = named(scenarios);
        if scenarios.kind() != "object" || pairs.len() != 1 || pairs[0].kind() != "pair" {
            return Err(self.err(node, "exactly one static arrival scenario is supported"));
        }
        let scenario = field(pairs[0], "value").unwrap();
        let mut executor = None;
        let mut start_rate = 0.0;
        let mut time_unit = 1000;
        let mut stages = None;
        let mut max_vus = None;
        let mut pre_allocated = None;
        for pair in named(scenario) {
            let key = self.key(pair)?;
            let value = field(pair, "value").unwrap();
            match key.as_str() {
                "executor" => executor = Some(self.string(value)?),
                "startRate" => start_rate = self.number(value)?,
                "timeUnit" => {
                    time_unit = parse_duration_ms_strict(&self.string(value)?)
                        .map_err(|e| self.err(value, e))?
                }
                "stages" => stages = Some(self.convert_stages(value)?),
                "maxVUs" => max_vus = Some(self.u32(value)?),
                "preAllocatedVUs" => pre_allocated = Some(self.u32(value)?),
                "exec" if self.string(value)? == "default" => {}
                _ => {
                    return Err(self.err(
                        value,
                        format!("arrival scenario option `{key}` is not supported"),
                    ))
                }
            }
        }
        if executor.as_deref() != Some("ramping-arrival-rate") || start_rate != 0.0 {
            return Err(self.err(
                scenario,
                "only ramping-arrival-rate with startRate: 0 maps exactly to native arrival stages",
            ));
        }
        let max_vus = max_vus
            .filter(|n| *n > 0)
            .ok_or_else(|| self.err(scenario, "arrival scenario needs maxVUs >= 1"))?;
        if pre_allocated.is_some_and(|n| n > max_vus) {
            return Err(self.err(scenario, "preAllocatedVUs cannot exceed maxVUs"));
        }
        let stages = stages.ok_or_else(|| self.err(scenario, "arrival scenario needs stages"))?;
        let mut rates = Vec::new();
        for stage in stages.as_array().unwrap() {
            let object = stage.as_object().unwrap();
            let target = object["target"].as_f64().unwrap();
            let rate = target * 1000.0 / time_unit as f64;
            if !rate.is_finite() {
                return Err(self.err(scenario, "arrival rate is not finite"));
            }
            rates.push(json!({ "duration": object["duration"], "rate": rate }));
        }
        let mut arrival = Map::new();
        arrival.insert("max_vus".into(), json!(max_vus));
        if let Some(pre) = pre_allocated {
            arrival.insert("pre_allocated_vus".into(), json!(pre));
        }
        arrival.insert("stages".into(), Value::Array(rates));
        config.insert("arrival".into(), Value::Object(arrival));
        Ok(())
    }

    fn convert_thresholds(&mut self, node: Node<'tree>) -> Result<(), String> {
        if self.unwrap(node).kind() != "object" {
            return Err(self.err(node, "thresholds must be a static object"));
        }
        let mut normal: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut abort: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for pair in named(node) {
            let metric = self.key(pair)?;
            if !matches!(
                metric.as_str(),
                "http_req_duration"
                    | "http_req_failed"
                    | "http_reqs"
                    | "iterations"
                    | "dropped_iterations"
            ) {
                return Err(self.err(
                    pair,
                    format!("threshold metric `{metric}` has no exact native equivalent"),
                ));
            }
            let value = field(pair, "value").unwrap();
            let entries = if self.unwrap(value).kind() == "array" {
                named(self.unwrap(value))
            } else {
                vec![value]
            };
            for entry in entries {
                let (expression, abort_on_fail) = self.threshold_entry(entry)?;
                let target = if abort_on_fail {
                    &mut abort
                } else {
                    &mut normal
                };
                target.entry(metric.clone()).or_default().push(expression);
            }
        }
        if !normal.is_empty() {
            self.after.push(threshold_step("k6 thresholds", normal));
        }
        if !abort.is_empty() {
            let mut step = threshold_step("k6 abort thresholds", abort);
            step.as_object_mut()
                .unwrap()
                .insert("abort_on_fail".into(), Value::Bool(true));
            self.after.push(step);
        }
        Ok(())
    }

    fn threshold_entry(&mut self, node: Node<'tree>) -> Result<(String, bool), String> {
        if self.unwrap(node).kind() == "object" {
            let mut expression = None;
            let mut abort = false;
            for pair in named(self.unwrap(node)) {
                let key = self.key(pair)?;
                let value = field(pair, "value").unwrap();
                match key.as_str() {
                    "threshold" => expression = Some(self.string(value)?),
                    "abortOnFail" => abort = self.boolean(value)?,
                    _ => {
                        return Err(
                            self.err(value, format!("threshold option `{key}` is not supported"))
                        )
                    }
                }
            }
            let expression =
                expression.ok_or_else(|| self.err(node, "threshold object needs `threshold`"))?;
            return Ok((self.threshold_expression(&expression, node)?, abort));
        }
        let expression = self.string(node)?;
        Ok((self.threshold_expression(&expression, node)?, false))
    }

    fn threshold_expression(&self, expression: &str, node: Node<'tree>) -> Result<String, String> {
        let trimmed = expression.trim();
        let converted = if trimmed.starts_with("p(") {
            let end = trimmed
                .find(')')
                .ok_or_else(|| self.err(node, "unterminated percentile"))?;
            let percentile = &trimmed[2..end];
            if !matches!(percentile, "50" | "90" | "95" | "99") {
                return Err(self.err(node, "native percentiles are p50, p90, p95, and p99"));
            }
            format!("p{percentile}{}", &trimmed[end + 1..])
        } else {
            trimmed.to_string()
        };
        thresholds::parse_expr(&converted).map_err(|e| self.err(node, e))?;
        Ok(converted)
    }

    fn arguments(&self, call: Node<'tree>) -> Vec<Node<'tree>> {
        field(call, "arguments").map(named).unwrap_or_default()
    }

    fn value(&self, node: Node<'tree>, depth: usize) -> Result<Value, String> {
        if depth > 32 {
            return Err(self.err(node, "constant expansion is cyclic or too deep"));
        }
        let node = self.unwrap(node);
        match node.kind() {
            "string" => Ok(Value::String(self.string_value(node)?)),
            "template_string" => Ok(Value::String(self.template_value(node, depth)?)),
            "number" => self.number(node).and_then(|n| {
                number_value(n).ok_or_else(|| self.err(node, "number is not finite"))
            }),
            "true" => Ok(Value::Bool(true)),
            "false" => Ok(Value::Bool(false)),
            "null" => Ok(Value::Null),
            "identifier" => {
                let name = self.text(node);
                let constant = self.constants.get(name).copied().ok_or_else(|| {
                    self.err(
                        node,
                        format!("dynamic identifier `{name}` is not supported"),
                    )
                })?;
                self.value(constant, depth + 1)
            }
            "member_expression" => Ok(Value::String(self.env_value(node)?)),
            "binary_expression" => {
                let left = field(node, "left").unwrap();
                let right = field(node, "right").unwrap();
                let operator = self.source[left.end_byte()..right.start_byte()].trim();
                if operator != "+" {
                    return Err(self.err(node, "only string concatenation with + is supported"));
                }
                Ok(Value::String(format!(
                    "{}{}",
                    self.string_depth(left, depth + 1)?,
                    self.string_depth(right, depth + 1)?
                )))
            }
            "unary_expression" => {
                let value = self.number(field(node, "argument").unwrap())?;
                if self.text(node).starts_with('-') {
                    number_value(-value).ok_or_else(|| self.err(node, "number is not finite"))
                } else {
                    number_value(value).ok_or_else(|| self.err(node, "number is not finite"))
                }
            }
            "object" => {
                let mut object = Map::new();
                for child in named(node) {
                    if child.kind() == "shorthand_property_identifier" {
                        let key = self.text(child).to_string();
                        let value = self.value(child, depth + 1)?;
                        object.insert(key, value);
                    } else if child.kind() == "pair" {
                        let key = self.key(child)?;
                        let value = self.value(field(child, "value").unwrap(), depth + 1)?;
                        object.insert(key, value);
                    } else {
                        return Err(self.err(child, "object spread and methods are not supported"));
                    }
                }
                Ok(Value::Object(object))
            }
            "array" => named(node)
                .into_iter()
                .map(|child| self.value(child, depth + 1))
                .collect::<Result<Vec<_>, _>>()
                .map(Value::Array),
            "call_expression"
                if field(node, "function")
                    .map(|n| self.text(n) == "JSON.stringify")
                    .unwrap_or(false) =>
            {
                let args = self.arguments(node);
                if args.len() != 1 {
                    return Err(self.err(node, "JSON.stringify needs one static value"));
                }
                let value = self.value(args[0], depth + 1)?;
                Ok(Value::String(
                    serde_json::to_string(&value).map_err(|e| self.err(node, e.to_string()))?,
                ))
            }
            _ => Err(self.err(
                node,
                format!("dynamic expression `{}` is not supported", node.kind()),
            )),
        }
    }

    fn env_value(&self, node: Node<'tree>) -> Result<String, String> {
        let object = field(node, "object").unwrap();
        if self.text(self.unwrap(object)) != "__ENV" {
            return Err(self.err(node, "only __ENV member access is supported"));
        }
        let property = if let Some(property) = field(node, "property") {
            self.text(property).to_string()
        } else if let Some(index) = field(node, "index") {
            self.string(index)?
        } else {
            return Err(self.err(node, "invalid __ENV access"));
        };
        if property.is_empty()
            || !property
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Err(self.err(node, "environment variable name must be static"));
        }
        Ok(format!("${{{{ env.{property} }}}}"))
    }

    fn string_depth(&self, node: Node<'tree>, depth: usize) -> Result<String, String> {
        match self.value(node, depth)? {
            Value::String(value) => Ok(value),
            _ => Err(self.err(node, "expected a string")),
        }
    }

    fn string(&self, node: Node<'tree>) -> Result<String, String> {
        self.string_depth(node, 0)
    }

    fn number(&self, node: Node<'tree>) -> Result<f64, String> {
        let node = self.unwrap(node);
        if node.kind() == "identifier" {
            let constant = self
                .constants
                .get(self.text(node))
                .copied()
                .ok_or_else(|| self.err(node, "expected a static number"))?;
            return self.number(constant);
        }
        if node.kind() == "unary_expression" {
            let value = self.number(field(node, "argument").unwrap())?;
            return Ok(if self.text(node).starts_with('-') {
                -value
            } else {
                value
            });
        }
        if node.kind() != "number" {
            return Err(self.err(node, "expected a static number"));
        }
        let text = self.text(node).replace('_', "");
        let value = if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
            u64::from_str_radix(hex, 16)
                .map(|n| n as f64)
                .map_err(|_| self.err(node, "invalid hexadecimal number"))?
        } else {
            text.parse::<f64>()
                .map_err(|_| self.err(node, "invalid number"))?
        };
        if value.is_finite() {
            Ok(value)
        } else {
            Err(self.err(node, "number must be finite"))
        }
    }

    fn u32(&self, node: Node<'tree>) -> Result<u32, String> {
        let value = self.number(node)?;
        if value < 0.0 || value.fract() != 0.0 || value > u32::MAX as f64 {
            return Err(self.err(node, "expected a non-negative 32-bit integer"));
        }
        Ok(value as u32)
    }

    fn boolean(&self, node: Node<'tree>) -> Result<bool, String> {
        match self.value(node, 0)? {
            Value::Bool(value) => Ok(value),
            _ => Err(self.err(node, "expected a boolean")),
        }
    }

    fn key(&self, pair: Node<'tree>) -> Result<String, String> {
        let key = field(pair, "key").ok_or_else(|| self.err(pair, "missing object key"))?;
        match key.kind() {
            "property_identifier" | "identifier" => Ok(self.text(key).to_string()),
            "string" => self.string_value(key),
            _ => Err(self.err(key, "computed object keys are not supported")),
        }
    }

    fn string_value(&self, node: Node<'tree>) -> Result<String, String> {
        let raw = self.text(node);
        if raw.len() < 2 {
            return Err(self.err(node, "invalid string"));
        }
        decode_js_string(&raw[1..raw.len() - 1]).map_err(|e| self.err(node, e))
    }

    fn template_value(&self, node: Node<'tree>, depth: usize) -> Result<String, String> {
        let mut output = String::new();
        for child in named(node) {
            match child.kind() {
                "string_fragment" => output
                    .push_str(&decode_js_string(self.text(child)).map_err(|e| self.err(child, e))?),
                "template_substitution" => {
                    let expression = named(child)
                        .into_iter()
                        .next()
                        .ok_or_else(|| self.err(child, "empty template substitution"))?;
                    output.push_str(&self.string_depth(expression, depth + 1)?);
                }
                _ => {}
            }
        }
        Ok(output)
    }

    fn unwrap(&self, mut node: Node<'tree>) -> Node<'tree> {
        while matches!(
            node.kind(),
            "parenthesized_expression" | "expression_statement"
        ) {
            node = named(node).into_iter().next().unwrap_or(node);
        }
        node
    }
}

fn threshold_step(name: &str, thresholds: BTreeMap<String, Vec<String>>) -> Value {
    let with = thresholds
        .into_iter()
        .map(|(metric, expressions)| (metric, json!(expressions)))
        .collect::<Map<_, _>>();
    json!({ "name": name, "use": "std/thresholds@v1", "with": with, "severity": "fail" })
}

fn field<'tree>(node: Node<'tree>, name: &str) -> Option<Node<'tree>> {
    node.child_by_field_name(name)
}

fn named(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|child| child.kind() != "comment")
        .collect()
}

fn descendants<'tree>(node: Node<'tree>, kind: &str) -> Vec<Node<'tree>> {
    let mut result = Vec::new();
    for child in named(node) {
        if child.kind() == kind {
            result.push(child);
        }
        result.extend(descendants(child, kind));
    }
    result
}

fn first_error(node: Node<'_>) -> Option<Node<'_>> {
    if node.is_error() || node.is_missing() {
        return Some(node);
    }
    named(node).into_iter().find_map(first_error)
}

fn number_value(value: f64) -> Option<Value> {
    if value >= 0.0 && value.fract() == 0.0 && value <= u64::MAX as f64 {
        Some(Value::Number(Number::from(value as u64)))
    } else if value < 0.0 && value.fract() == 0.0 && value >= i64::MIN as f64 {
        Some(Value::Number(Number::from(value as i64)))
    } else {
        Number::from_f64(value).map(Value::Number)
    }
}

fn decode_js_string(raw: &str) -> Result<String, String> {
    let mut chars = raw.chars().peekable();
    let mut output = String::new();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            output.push(ch);
            continue;
        }
        let escaped = chars.next().ok_or("unterminated string escape")?;
        match escaped {
            'n' => output.push('\n'),
            'r' => output.push('\r'),
            't' => output.push('\t'),
            'b' => output.push('\u{0008}'),
            'f' => output.push('\u{000c}'),
            'v' => output.push('\u{000b}'),
            '0' => output.push('\0'),
            '\\' => output.push('\\'),
            '\'' => output.push('\''),
            '"' => output.push('"'),
            '`' => output.push('`'),
            '$' => output.push('$'),
            '\n' => {}
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
            }
            'x' => output.push(read_unicode(&mut chars, 2)?),
            'u' => {
                if chars.peek() == Some(&'{') {
                    chars.next();
                    let mut hex = String::new();
                    for ch in chars.by_ref() {
                        if ch == '}' {
                            break;
                        }
                        hex.push(ch);
                    }
                    let code =
                        u32::from_str_radix(&hex, 16).map_err(|_| "invalid Unicode escape")?;
                    output.push(char::from_u32(code).ok_or("invalid Unicode scalar")?);
                } else {
                    let high = read_hex(&mut chars, 4)?;
                    let code = if (0xd800..=0xdbff).contains(&high) {
                        if chars.next() != Some('\\') || chars.next() != Some('u') {
                            return Err("unpaired Unicode surrogate".into());
                        }
                        let low = read_hex(&mut chars, 4)?;
                        if !(0xdc00..=0xdfff).contains(&low) {
                            return Err("unpaired Unicode surrogate".into());
                        }
                        0x10000 + ((high - 0xd800) << 10) + (low - 0xdc00)
                    } else {
                        high
                    };
                    output.push(char::from_u32(code).ok_or("invalid Unicode scalar")?);
                }
            }
            other => output.push(other),
        }
    }
    Ok(output)
}

fn read_hex<I: Iterator<Item = char>>(chars: &mut I, count: usize) -> Result<u32, String> {
    let mut hex = String::new();
    for _ in 0..count {
        hex.push(chars.next().ok_or("short hexadecimal escape")?);
    }
    u32::from_str_radix(&hex, 16).map_err(|_| "invalid hexadecimal escape".into())
}

fn read_unicode<I: Iterator<Item = char>>(chars: &mut I, count: usize) -> Result<char, String> {
    char::from_u32(read_hex(chars, count)?).ok_or_else(|| "invalid hexadecimal escape".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn documents(source: &str) -> (Value, Value) {
        let output = convert(source).unwrap();
        (
            serde_yaml::from_str(&output.test_yaml).unwrap(),
            serde_yaml::from_str(&output.config_yaml).unwrap(),
        )
    }

    #[test]
    fn converts_the_hello_example() {
        let (test, config) = documents(
            r#"
            import http from 'k6/http';
            import { check, sleep } from 'k6';
            export const options = { vus: 5, duration: '30s' };
            export default function () {
              const res = http.get('https://httpbin.org/get');
              check(res, { 'status is 200': (r) => r.status === 200 });
              sleep(0.2);
            }
        "#,
        );
        assert_eq!(config, json!({"vus": 5, "duration": "30s"}));
        let steps = test["steps"].as_array().unwrap();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0]["use"], "std/http@v1");
        assert_eq!(steps[0]["with"]["method"], "GET");
        assert_eq!(steps[0]["check"]["status"], 200);
        assert_eq!(steps[0]["outputs"], "res");
        assert_eq!(steps[1]["with"]["ms"], 200);
    }

    #[test]
    fn converts_static_payloads_groups_checks_and_thresholds() {
        let (test, config) = documents(
            r#"
            import http from 'k6/http';
            import { check, group, sleep } from 'k6';
            export const options = {
              // Comments do not affect the generated documents.
              stages: [{ duration: '10s', target: 4 }, { duration: '20s', target: 0 }],
              insecureSkipTLSVerify: true,
              thresholds: {
                http_req_duration: ['p(95)<500', { threshold: 'p(99)<1000', abortOnFail: true }],
                http_req_failed: ['rate<0.01'],
              },
            };
            const payload = { kind: 'sync', count: 2 };
            export default function () {
              group('write', function () {
                const res = http.post(`${__ENV.BASE_URL}/items`, payload, {
                  headers: { 'X-Token': __ENV.TOKEN }, timeout: '2s',
                });
                check(res, { ok: (r) => r.status === 201 && r.body.includes('ok') });
              });
              sleep(0.25);
            }
        "#,
        );
        assert_eq!(config["stages"][1]["target"], 0);
        let step = &test["steps"][0];
        assert_eq!(step["name"], "write: POST ${{ env.BASE_URL }}/items");
        assert_eq!(step["with"]["body"], payload());
        assert_eq!(step["with"]["headers"]["X-Token"], "${{ env.TOKEN }}");
        assert_eq!(step["with"]["timeout"], 2000);
        assert_eq!(step["with"]["insecure"], true);
        assert_eq!(step["check"], json!({"status": 201, "body_contains": "ok"}));
        let after = config["after"].as_array().unwrap();
        assert_eq!(after.len(), 2);
        assert_eq!(after[0]["with"]["http_req_duration"], json!(["p95<500"]));
        assert_eq!(after[0]["with"]["http_req_failed"], json!(["rate<0.01"]));
        assert_eq!(after[1]["with"]["http_req_duration"], json!(["p99<1000"]));
        assert_eq!(after[1]["abort_on_fail"], true);
    }

    fn payload() -> Value {
        json!({"kind": "sync", "count": 2})
    }

    #[test]
    fn converts_ramping_arrival_rate_to_per_second_rates() {
        let (_, config) = documents(
            r#"
            import http from 'k6/http';
            export const options = { scenarios: { default: {
              executor: 'ramping-arrival-rate', startRate: 0, timeUnit: '2s',
              preAllocatedVUs: 2, maxVUs: 10,
              stages: [{ duration: '30s', target: 20 }],
            } } };
            export default function () { http.get('https://example.com'); }
        "#,
        );
        assert_eq!(config["arrival"]["max_vus"], 10);
        assert_eq!(config["arrival"]["pre_allocated_vus"], 2);
        assert_eq!(config["arrival"]["stages"][0]["rate"], 10.0);
        assert!(config.get("vus").is_none());
    }

    #[test]
    fn supports_import_aliases_and_json_stringify() {
        let (test, _) = documents(
            r#"
            import client from 'k6/http';
            import { check as verify } from 'k6';
            export const options = { vus: 1, duration: '1s' };
            export default function () {
              const res = client.put('https://example.com', JSON.stringify({ ok: true }));
              verify(res, { ok: (r) => r.status === 200 });
            }
        "#,
        );
        assert_eq!(test["steps"][0]["with"]["method"], "PUT");
        assert_eq!(test["steps"][0]["with"]["body"], "{\"ok\":true}");
    }

    #[test]
    fn output_is_deterministic() {
        let source = r##"
            import http from 'k6/http';
            export const options = { vus: 1, duration: '1s' };
            export default function () { http.get('https://example.com'); }
        "##;
        let a = convert(source).unwrap();
        let b = convert(source).unwrap();
        assert_eq!(a.test_yaml, b.test_yaml);
        assert_eq!(a.config_yaml, b.config_yaml);
    }

    #[test]
    fn unsupported_behavior_fails_closed_with_a_source_line() {
        let cases = [
            ("export const options = { vus: 1, duration: '1s' }; export default function () { if (true) {} }", "unsupported statement"),
            ("import sql from 'k6/x/sql'; export const options = { vus: 1, duration: '1s' }; export default function () {}", "not supported"),
            ("export const options = { vus: 1, duration: '1s', iterations: 2 }; export default function () {}", "option `iterations`"),
            ("export const options = { vus: 1 }; export default function () {}", "both vus and duration"),
            ("export const options = { vus: 1, duration: '1s' }; export default function () { const x = ; }", "invalid JavaScript syntax"),
            ("import http from 'k6/http'; export const options = { vus: 1, duration: '1s' }; export default function () { http.get('https://example.com', {}, {}); }", "too many HTTP arguments"),
            ("export const options = { vus: 1, duration: '1s', thresholds: 'p95<500' }; export default function () {}", "thresholds must be a static object"),
            ("import http from 'k6/http'; export const options = { vus: 1, duration: '1s' }; export default function () { const res = http.get('https://example.com'); const res = http.get('https://example.com/other'); }", "variable shadowing"),
        ];
        for (source, expected) in cases {
            let error = convert(source).unwrap_err();
            assert!(error.contains("line "), "{error}");
            assert!(
                error.contains(expected),
                "{error} did not contain {expected}"
            );
        }
    }

    #[test]
    fn js_string_escapes_and_unicode_survive() {
        let (test, _) = documents(
            r#"
            import http from 'k6/http';
            export const options = { vus: 1, duration: '1s' };
            export default function () {
              check(http.get('https://example.com/\uD83D\uDE00'), {
                ok: (r) => r.body.includes('line\n\uD83D\uDE00'),
              });
            }
        "#,
        );
        assert_eq!(test["steps"][0]["with"]["url"], "https://example.com/😀");
        assert_eq!(test["steps"][0]["check"]["body_contains"], "line\n😀");
    }
}
