//! The schema-layer validation walk (layer 1 of 3 — see
//! `docs/services/session-runner.md` § Validation).
//!
//! One recursive pass over the raw JSON both **validates** and **builds**
//! the typed model (parse-don't-validate): every rule the format pins is
//! checked with a targeted message and an exact JSON Pointer, and a
//! [`Document`] is produced only when zero issues were found. The
//! published `schema/workflow-v1.schema.json` remains the external
//! contract; this walk is deliberately *stronger* than the schema (it
//! also checks the rules the schema cannot express: uniqueness,
//! expression grammar, duration semantics, `$expr` placement, namespace
//! scoping). The one-directional agreement — everything this walk
//! accepts passes the schema, everything the schema rejects this walk
//! rejects — is enforced by the schema-agreement test suite.
//!
//! Recursion depth is explicitly bounded: [`build`] rejects values
//! nested deeper than [`MAX_NESTING`] (`serde_json`'s own default parser
//! limit) with a single issue *before* walking, measured iteratively so
//! the check itself cannot recurse. Values arriving through
//! `Document::parse` are already bounded by `serde_json`, but
//! `Document::from_value` is public and can be handed an arbitrarily
//! deep, programmatically built `Value` — without the gate that could
//! overflow the walk's stack.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use super::duration;
use super::model::{
    ArgValue, Bound, Document, Instruction, InstructionKind, Log, LogLevel, ParameterDecl,
    ParameterType, Repeat, RepeatMode, Retry, SetEntry, ToolCall, Trigger, TriggerSource, Wait,
};
use super::ValidationIssue;
use crate::expr::Expression;

/// The format version this engine implements.
const SUPPORTED_VERSION: u64 = 1;

/// Which contextual namespaces are in scope for expressions at the
/// current position. `params`, `session`, and `result` are always in
/// scope; `event` and `error` only in the positions the design names.
#[derive(Clone, Copy, Debug)]
struct Scope {
    event_ok: bool,
    error_ok: bool,
}

/// The procedure tree outside any `catch`/`finally` or trigger.
const TREE: Scope = Scope {
    event_ok: false,
    error_ok: false,
};

/// The maximum JSON nesting depth the walk accepts — `serde_json`'s own
/// default parser recursion limit, so no document that `serde_json`
/// could parse is ever affected.
const MAX_NESTING: usize = 128;

pub(super) fn build(value: &Value) -> Result<Document, Vec<ValidationIssue>> {
    if nesting_exceeds(value, MAX_NESTING) {
        return Err(vec![ValidationIssue {
            pointer: String::new(),
            message: format!("document nesting exceeds {MAX_NESTING} levels"),
            expr_span: None,
        }]);
    }
    let mut b = Builder::default();
    let doc = b.document(value);
    match doc {
        Some(doc) if b.issues.is_empty() => Ok(doc),
        _ => Err(b.issues),
    }
}

/// Whether `value` nests **containers** (objects / arrays) deeper than
/// `limit` levels — primitives do not add a level, so this counts
/// exactly what `serde_json`'s parser recursion limit counts. `serde_json`
/// itself errors on *entering* container number `limit`, i.e. accepts
/// at most `limit - 1` nested containers, so every parseable document
/// clears this gate with a level to spare. Iterative (an explicit work
/// stack), so the guard itself is safe on input the recursive walk
/// could not survive.
fn nesting_exceeds(value: &Value, limit: usize) -> bool {
    let mut stack = vec![(value, 1usize)];
    while let Some((v, depth)) = stack.pop() {
        match v {
            Value::Object(_) | Value::Array(_) if depth > limit => return true,
            Value::Object(m) => stack.extend(m.values().map(|c| (c, depth.saturating_add(1)))),
            Value::Array(a) => stack.extend(a.iter().map(|c| (c, depth.saturating_add(1)))),
            _ => {}
        }
    }
    false
}

/// Appends `key` to a JSON Pointer, escaping per RFC 6901.
pub(super) fn child(ptr: &str, key: &str) -> String {
    if key.contains(['~', '/']) {
        let escaped = key.replace('~', "~0").replace('/', "~1");
        format!("{ptr}/{escaped}")
    } else {
        format!("{ptr}/{key}")
    }
}

fn element(ptr: &str, index: usize) -> String {
    format!("{ptr}/{index}")
}

fn article(word: &str) -> &'static str {
    if word.starts_with(['a', 'e', 'i', 'o', 'u']) {
        "an"
    } else {
        "a"
    }
}

fn quoted_list(items: &[&str]) -> String {
    let mut out = String::new();
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push('`');
        out.push_str(item);
        out.push('`');
    }
    out
}

#[derive(Default)]
struct Builder {
    issues: Vec<ValidationIssue>,
    /// `once` key → pointer of its first use (uniqueness is
    /// document-wide, triggers included).
    once_keys: BTreeMap<String, String>,
    /// Trigger `id` → pointer of its first use.
    trigger_ids: BTreeMap<String, String>,
}

impl Builder {
    fn issue(&mut self, ptr: &str, message: impl Into<String>) {
        self.issues.push(ValidationIssue {
            pointer: ptr.to_owned(),
            message: message.into(),
            expr_span: None,
        });
    }

    // ---- document ------------------------------------------------------

    fn document(&mut self, value: &Value) -> Option<Document> {
        let Value::Object(obj) = value else {
            self.issue("", "a workflow document must be a JSON object");
            return None;
        };

        // Version gate, before anything else: a document for a version
        // this engine does not implement cannot be judged against the v1
        // rules, so it gets exactly one error and no further noise.
        let version = if let Some(v) = obj.get("version") {
            match v.as_u64() {
                Some(n) if n == SUPPORTED_VERSION => n,
                _ => {
                    self.issue(
                        "/version",
                        format!(
                            "unsupported document version {v}: this engine implements version \
                         {SUPPORTED_VERSION}"
                        ),
                    );
                    return None;
                }
            }
        } else {
            self.issue("", "missing required key `version`");
            SUPPORTED_VERSION // best effort: validate the rest as v1
        };

        const TOP_KEYS: [&str; 8] = [
            "version",
            "name",
            "description",
            "parameters",
            "estimated_duration",
            "max_duration",
            "triggers",
            "root",
        ];
        for k in obj.keys() {
            if !TOP_KEYS.contains(&k.as_str()) {
                self.issue(
                    &child("", k),
                    format!("unknown key `{k}` in a workflow document"),
                );
            }
        }

        let name = if let Some(v) = obj.get("name") {
            self.string_field(v, "/name", "`name`")
        } else {
            self.issue("", "missing required key `name`");
            None
        };

        let description = match obj.get("description") {
            None => None,
            Some(Value::String(s)) => Some(s.clone()),
            Some(_) => {
                self.issue("/description", "`description` must be a string");
                None
            }
        };

        let parameters = self.parameters(obj.get("parameters"));

        let estimated_duration = obj
            .get("estimated_duration")
            .and_then(|v| self.duration_field(v, "/estimated_duration", "`estimated_duration`"));
        let max_duration = obj
            .get("max_duration")
            .and_then(|v| self.duration_field(v, "/max_duration", "`max_duration`"));

        // The procedure tree before the triggers, so document-wide
        // bookkeeping (e.g. duplicate `once` keys) reports the tree
        // occurrence as the original and the trigger one as the duplicate.
        let root = if let Some(v) = obj.get("root") {
            self.instruction(v, "/root", TREE)
        } else {
            self.issue("", "missing required key `root`");
            None
        };

        let triggers = self.triggers(obj.get("triggers"));

        Some(Document {
            version,
            name: name?,
            description,
            parameters: parameters?,
            estimated_duration,
            max_duration,
            triggers: triggers?,
            root: root?,
        })
    }

    fn parameters(&mut self, value: Option<&Value>) -> Option<BTreeMap<String, ParameterDecl>> {
        let mut out = BTreeMap::new();
        let Some(value) = value else {
            return Some(out);
        };
        let Value::Object(obj) = value else {
            self.issue("/parameters", "`parameters` must be a JSON object");
            return None;
        };
        let mut ok = true;
        for (name, decl) in obj {
            let ptr = child("/parameters", name);
            if !self.parameter_name(name, &ptr) {
                ok = false;
                continue;
            }
            match self.parameter_decl(decl, &ptr) {
                Some(d) => {
                    out.insert(name.clone(), d);
                }
                None => ok = false,
            }
        }
        ok.then_some(out)
    }

    fn parameter_name(&mut self, name: &str, ptr: &str) -> bool {
        if name.starts_with('_') {
            self.issue(
                ptr,
                format!(
                    "parameter names beginning with `_` are reserved for the engine \
                     (e.g. `params._recovery`); `{name}` cannot be declared"
                ),
            );
            return false;
        }
        let mut chars = name.chars();
        let head_ok = chars.next().is_some_and(|c| c.is_ascii_alphabetic());
        let tail_ok = chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !(head_ok && tail_ok) {
            self.issue(
                ptr,
                format!(
                    "`{name}` is not a valid parameter name — names are a letter followed \
                     by letters, digits, or underscores"
                ),
            );
            return false;
        }
        true
    }

    fn parameter_decl(&mut self, value: &Value, ptr: &str) -> Option<ParameterDecl> {
        let Value::Object(obj) = value else {
            self.issue(ptr, "a parameter declaration must be a JSON object");
            return None;
        };
        for k in obj.keys() {
            if !["type", "required", "default"].contains(&k.as_str()) {
                self.issue(
                    &child(ptr, k),
                    format!(
                        "unknown key `{k}` in a parameter declaration \
                         (allowed: `type`, `required`, `default`)"
                    ),
                );
            }
        }
        let ty = match obj.get("type") {
            Some(Value::String(s)) => match s.as_str() {
                "string" => Some(ParameterType::String),
                "integer" => Some(ParameterType::Integer),
                "number" => Some(ParameterType::Number),
                "boolean" => Some(ParameterType::Boolean),
                "duration" => Some(ParameterType::Duration),
                "array" => Some(ParameterType::Array),
                other => {
                    self.issue(
                        &child(ptr, "type"),
                        format!(
                            "unknown parameter type `{other}` (allowed: `string`, `integer`, \
                             `number`, `boolean`, `duration`, `array`)"
                        ),
                    );
                    None
                }
            },
            Some(_) => {
                self.issue(&child(ptr, "type"), "`type` must be a string");
                None
            }
            None => {
                self.issue(ptr, "a parameter declaration requires a `type`");
                None
            }
        };

        let required = obj.get("required");
        let default = obj.get("default");
        if let Some(r) = required {
            if r != &Value::Bool(true) {
                self.issue(
                    &child(ptr, "required"),
                    "`required` must be `true` — omit it and give a `default` to make a \
                     parameter optional",
                );
                return None;
            }
        }
        match (required, default) {
            (Some(_), Some(_)) => {
                self.issue(
                    ptr,
                    "a parameter is either `required: true` or has a `default`, not both",
                );
                None
            }
            (None, None) => {
                self.issue(
                    ptr,
                    "a parameter needs either `required: true` or a `default`",
                );
                None
            }
            (Some(_), None) => Some(ParameterDecl {
                ty: ty?,
                default: None,
            }),
            (None, Some(d)) => {
                let ty = ty?;
                if let Err(msg) = super::params::type_check(ty, d) {
                    self.issue(&child(ptr, "default"), format!("invalid `default`: {msg}"));
                    return None;
                }
                Some(ParameterDecl {
                    ty,
                    default: Some(d.clone()),
                })
            }
        }
    }

    // ---- shared field kinds ---------------------------------------------

    fn string_field(&mut self, v: &Value, ptr: &str, what: &str) -> Option<String> {
        match v {
            Value::String(s) if !s.is_empty() => Some(s.clone()),
            _ => {
                self.issue(ptr, format!("{what} must be a non-empty string"));
                None
            }
        }
    }

    fn duration_field(&mut self, v: &Value, ptr: &str, what: &str) -> Option<std::time::Duration> {
        let Value::String(s) = v else {
            self.issue(
                ptr,
                format!("{what} must be a duration string (e.g. \"300s\", \"1h30m\")"),
            );
            return None;
        };
        match duration::parse_duration(s) {
            Ok(d) => Some(d),
            Err(msg) => {
                self.issue(ptr, msg);
                None
            }
        }
    }

    fn expression(&mut self, v: &Value, ptr: &str, scope: Scope) -> Option<Expression> {
        let Value::String(src) = v else {
            self.issue(ptr, "must be an expression string");
            return None;
        };
        let expr = match Expression::parse(src) {
            Ok(e) => e,
            Err(e) => {
                self.issues.push(ValidationIssue {
                    pointer: ptr.to_owned(),
                    message: format!("invalid expression: {}", e.message),
                    expr_span: Some(e.span),
                });
                return None;
            }
        };
        let mut ok = true;
        {
            let roots = expr.namespaces();
            if !scope.event_ok {
                if let Some(span) = roots.get("event") {
                    self.issues.push(ValidationIssue {
                        pointer: ptr.to_owned(),
                        message: "`event` is not in scope here — event.* is only available \
                                  inside a trigger's `when`, `while`, and `do`"
                            .to_owned(),
                        expr_span: Some(*span),
                    });
                    ok = false;
                }
            }
            if !scope.error_ok {
                if let Some(span) = roots.get("error") {
                    self.issues.push(ValidationIssue {
                        pointer: ptr.to_owned(),
                        message: "`error` is not in scope here — error.* is only available \
                                  inside `catch` and `finally` blocks"
                            .to_owned(),
                        expr_span: Some(*span),
                    });
                    ok = false;
                }
            }
        }
        ok.then_some(expr)
    }

    /// A loop bound: a literal integer (≥ `min`), or an `$expr` wrapper
    /// evaluated once at loop entry.
    fn bound(&mut self, v: &Value, ptr: &str, scope: Scope, min: u64, what: &str) -> Option<Bound> {
        if let Value::Object(obj) = v {
            return self.expr_wrapper(obj, ptr, scope, what).map(Bound::Expr);
        }
        match v.as_u64() {
            Some(n) if n >= min => Some(Bound::Literal(n)),
            _ => {
                let kind = if min == 0 {
                    "a non-negative integer"
                } else {
                    "a positive integer"
                };
                self.issue(
                    ptr,
                    format!("{what} must be {kind} or an {{\"$expr\": …}} wrapper"),
                );
                None
            }
        }
    }

    /// An `{"$expr": "…"}` wrapper object (already known to be an object).
    fn expr_wrapper(
        &mut self,
        obj: &Map<String, Value>,
        ptr: &str,
        scope: Scope,
        what: &str,
    ) -> Option<Expression> {
        match obj.get("$expr") {
            Some(inner) if obj.len() == 1 => self.expression(inner, &child(ptr, "$expr"), scope),
            Some(_) => {
                self.issue(
                    ptr,
                    "an object with a `$expr` key must contain only that key",
                );
                None
            }
            None => {
                self.issue(
                    ptr,
                    format!("{what} must be a literal or an {{\"$expr\": …}} wrapper"),
                );
                None
            }
        }
    }

    /// A tool-call (or poll) argument value: an `$expr` wrapper, or
    /// literal JSON that is guaranteed free of nested `$expr` keys.
    fn arg_value(&mut self, v: &Value, ptr: &str, scope: Scope) -> Option<ArgValue> {
        if let Value::Object(obj) = v {
            if obj.contains_key("$expr") {
                return self
                    .expr_wrapper(obj, ptr, scope, "an argument")
                    .map(ArgValue::Expr);
            }
        }
        if self.reject_nested_expr(v, ptr) {
            Some(ArgValue::Literal(v.clone()))
        } else {
            None
        }
    }

    /// Rejects `$expr` keys buried inside a literal value. `$expr` is only
    /// recognized as a *direct* argument value; letting a nested one pass
    /// as data would silently send the wrapper object to the tool —
    /// exactly the kind of no-op misspelling the format forbids.
    fn reject_nested_expr(&mut self, v: &Value, ptr: &str) -> bool {
        match v {
            Value::Object(obj) => {
                let mut clean = if obj.contains_key("$expr") {
                    self.issue(
                        &child(ptr, "$expr"),
                        "`$expr` is only recognized as a direct argument value — it cannot \
                         appear inside a literal",
                    );
                    false
                } else {
                    true
                };
                for (k, inner) in obj {
                    if k != "$expr" {
                        clean &= self.reject_nested_expr(inner, &child(ptr, k));
                    }
                }
                clean
            }
            Value::Array(items) => {
                let mut clean = true;
                for (i, inner) in items.iter().enumerate() {
                    clean &= self.reject_nested_expr(inner, &element(ptr, i));
                }
                clean
            }
            _ => true,
        }
    }

    fn args(
        &mut self,
        v: Option<&Value>,
        ptr: &str,
        scope: Scope,
    ) -> Option<BTreeMap<String, ArgValue>> {
        let mut out = BTreeMap::new();
        let Some(v) = v else {
            return Some(out);
        };
        let Value::Object(obj) = v else {
            self.issue(ptr, "`args` must be a JSON object");
            return None;
        };
        let mut ok = true;
        for (k, inner) in obj {
            match self.arg_value(inner, &child(ptr, k), scope) {
                Some(a) => {
                    out.insert(k.clone(), a);
                }
                None => ok = false,
            }
        }
        ok.then_some(out)
    }

    /// An instruction array (`then`, `body`, `do`, …).
    fn block(
        &mut self,
        v: &Value,
        ptr: &str,
        scope: Scope,
        what: &str,
        min_items: usize,
    ) -> Option<Vec<Instruction>> {
        let Value::Array(items) = v else {
            self.issue(ptr, format!("{what} must be an array of instructions"));
            return None;
        };
        if items.len() < min_items {
            self.issue(ptr, format!("{what} must contain at least one instruction"));
            return None;
        }
        let mut out = Vec::with_capacity(items.len());
        let mut ok = true;
        for (i, item) in items.iter().enumerate() {
            match self.instruction(item, &element(ptr, i), scope) {
                Some(instr) => out.push(instr),
                None => ok = false,
            }
        }
        ok.then_some(out)
    }

    // ---- instructions ----------------------------------------------------

    fn instruction(&mut self, v: &Value, ptr: &str, scope: Scope) -> Option<Instruction> {
        let Value::Object(obj) = v else {
            self.issue(ptr, "an instruction must be a JSON object");
            return None;
        };

        const DISCRIMINANTS: [&str; 10] = [
            "tool", "sequence", "repeat", "if", "set", "try", "fail", "wait", "log", "script",
        ];
        let present: Vec<&str> = DISCRIMINANTS
            .iter()
            .copied()
            .filter(|d| obj.contains_key(*d))
            .collect();
        let disc = match present.as_slice() {
            [] => {
                let keys: Vec<&str> = obj.keys().map(String::as_str).collect();
                let found = if keys.is_empty() {
                    String::from("an empty object")
                } else {
                    format!("keys {}", quoted_list(&keys))
                };
                self.issue(
                    ptr,
                    format!(
                        "not an instruction: expected exactly one of `tool`, `sequence`, \
                         `repeat`, `if`, `set`, `try`, `fail`, `wait`, `log`; found {found}"
                    ),
                );
                return None;
            }
            ["script"] => {
                self.issue(
                    &child(ptr, "script"),
                    "`script` is reserved for a future format version — a version-1 document \
                     cannot use it",
                );
                return None;
            }
            [d] => *d,
            many => {
                self.issue(
                    ptr,
                    format!(
                        "an instruction has exactly one discriminant key; found {}",
                        quoted_list(many)
                    ),
                );
                return None;
            }
        };

        let companions: &[&str] = match disc {
            "tool" => &["args", "retry"],
            "repeat" => &["body"],
            "if" => &["then", "else"],
            "try" => &["catch", "finally"],
            _ => &[],
        };
        for k in obj.keys() {
            let k = k.as_str();
            if k != disc && !companions.contains(&k) && k != "id" && k != "once" {
                let mut allowed = vec![disc];
                allowed.extend_from_slice(companions);
                self.issue(
                    &child(ptr, k),
                    format!(
                        "unknown key `{k}` in {} `{disc}` instruction (allowed: {}, plus `id` \
                         and `once`)",
                        article(disc),
                        quoted_list(&allowed)
                    ),
                );
            }
        }

        let id = obj
            .get("id")
            .and_then(|v| self.string_field(v, &child(ptr, "id"), "`id`"));
        let once = obj
            .get("once")
            .and_then(|v| self.string_field(v, &child(ptr, "once"), "`once`"))
            .and_then(|key| self.record_once_key(key, &child(ptr, "once")));

        let kind = match disc {
            "tool" => self.tool_kind(obj, ptr, scope),
            "sequence" => self.sequence_kind(obj, ptr, scope),
            "repeat" => self.repeat_kind(obj, ptr, scope),
            "if" => self.if_kind(obj, ptr, scope),
            "set" => self.set_kind(obj, ptr, scope),
            "try" => self.try_kind(obj, ptr, scope),
            "fail" => self.fail_kind(obj, ptr, scope),
            "wait" => self.wait_kind(obj, ptr, scope),
            _ => self.log_kind(obj, ptr, scope),
        };

        Some(Instruction {
            id,
            once,
            kind: kind?,
        })
    }

    fn record_once_key(&mut self, key: String, ptr: &str) -> Option<String> {
        if let Some(first) = self.once_keys.get(&key) {
            self.issue(
                ptr,
                format!(
                    "duplicate `once` key `{key}` — `once` keys must be unique within a \
                     document (also used at {first})"
                ),
            );
            return None;
        }
        self.once_keys.insert(key.clone(), ptr.to_owned());
        Some(key)
    }

    fn missing(&mut self, ptr: &str, disc: &str, key: &str) {
        self.issue(
            ptr,
            format!("{} `{disc}` instruction requires a `{key}`", article(disc)),
        );
    }

    fn tool_kind(
        &mut self,
        obj: &Map<String, Value>,
        ptr: &str,
        scope: Scope,
    ) -> Option<InstructionKind> {
        let tool = obj
            .get("tool")
            .and_then(|v| self.string_field(v, &child(ptr, "tool"), "`tool`"));
        let args = self.args(obj.get("args"), &child(ptr, "args"), scope);
        let retry = match obj.get("retry") {
            None => Some(None),
            Some(v) => self.retry(v, &child(ptr, "retry")).map(Some),
        };
        Some(InstructionKind::Tool(ToolCall {
            tool: tool?,
            args: args?,
            retry: retry?,
        }))
    }

    fn retry(&mut self, v: &Value, ptr: &str) -> Option<Retry> {
        let Value::Object(obj) = v else {
            self.issue(
                ptr,
                "`retry` must be an object with `max_attempts` and `backoff`",
            );
            return None;
        };
        for k in obj.keys() {
            if !["max_attempts", "backoff"].contains(&k.as_str()) {
                self.issue(
                    &child(ptr, k),
                    format!("unknown key `{k}` in `retry` (allowed: `max_attempts`, `backoff`)"),
                );
            }
        }
        let max_attempts = if let Some(v) = obj.get("max_attempts") {
            match v.as_u64() {
                Some(n) if n >= 1 => Some(n),
                _ => {
                    self.issue(
                        &child(ptr, "max_attempts"),
                        "`max_attempts` must be a positive integer (total attempts, \
                     including the first)",
                    );
                    None
                }
            }
        } else {
            self.issue(ptr, "`retry` requires `max_attempts`");
            None
        };
        let backoff = if let Some(v) = obj.get("backoff") {
            self.duration_field(v, &child(ptr, "backoff"), "`backoff`")
        } else {
            self.issue(ptr, "`retry` requires a `backoff`");
            None
        };
        Some(Retry {
            max_attempts: max_attempts?,
            backoff: backoff?,
        })
    }

    fn sequence_kind(
        &mut self,
        obj: &Map<String, Value>,
        ptr: &str,
        scope: Scope,
    ) -> Option<InstructionKind> {
        let seq = obj.get("sequence")?;
        let Value::Array(items) = seq else {
            self.issue(
                &child(ptr, "sequence"),
                "`sequence` must be an array of instructions",
            );
            return None;
        };
        let mut out = Vec::with_capacity(items.len());
        let mut ok = true;
        let sptr = child(ptr, "sequence");
        for (i, item) in items.iter().enumerate() {
            match self.instruction(item, &element(&sptr, i), scope) {
                Some(instr) => out.push(instr),
                None => ok = false,
            }
        }
        ok.then_some(InstructionKind::Sequence(out))
    }

    fn repeat_kind(
        &mut self,
        obj: &Map<String, Value>,
        ptr: &str,
        scope: Scope,
    ) -> Option<InstructionKind> {
        let opts = obj.get("repeat")?;
        let rptr = child(ptr, "repeat");
        let Value::Object(opts) = opts else {
            self.issue(
                &rptr,
                "`repeat` must be an object with one of `until`/`while`/`count` \
                 (and `max_iterations`)",
            );
            return None;
        };
        for k in opts.keys() {
            if !["until", "while", "count", "max_iterations"].contains(&k.as_str()) {
                self.issue(
                    &child(&rptr, k),
                    format!(
                        "unknown key `{k}` in `repeat` (allowed: `until`, `while`, `count`, \
                         `max_iterations`)"
                    ),
                );
            }
        }

        let modes: Vec<&str> = ["until", "while", "count"]
            .into_iter()
            .filter(|m| opts.contains_key(*m))
            .collect();
        let mode_key = match modes.as_slice() {
            [m] => *m,
            [] => {
                self.issue(
                    &rptr,
                    "`repeat` requires exactly one of `until`, `while`, `count`",
                );
                return None;
            }
            many => {
                self.issue(
                    &rptr,
                    format!(
                        "`repeat` takes exactly one of `until`, `while`, `count`; found {}",
                        quoted_list(many)
                    ),
                );
                return None;
            }
        };

        let max_iterations = match opts.get("max_iterations") {
            Some(v) => self
                .bound(
                    v,
                    &child(&rptr, "max_iterations"),
                    scope,
                    1,
                    "`max_iterations`",
                )
                .map(Some),
            None => Some(None),
        };

        let mode = match mode_key {
            "count" => {
                let count = opts
                    .get("count")
                    .and_then(|v| self.bound(v, &child(&rptr, "count"), scope, 0, "`count`"));
                Some(RepeatMode::Count {
                    count: count?,
                    max_iterations: max_iterations?,
                })
            }
            cond_key => {
                let condition = opts
                    .get(cond_key)
                    .and_then(|v| self.expression(v, &child(&rptr, cond_key), scope));
                let max_iterations = if let Some(b) = max_iterations? {
                    Some(b)
                } else {
                    self.issue(
                        &rptr,
                        format!(
                            "`max_iterations` is required with `{cond_key}` — unbounded \
                             loops are a validation error"
                        ),
                    );
                    None
                };
                if cond_key == "until" {
                    Some(RepeatMode::Until {
                        condition: condition?,
                        max_iterations: max_iterations?,
                    })
                } else {
                    Some(RepeatMode::While {
                        condition: condition?,
                        max_iterations: max_iterations?,
                    })
                }
            }
        };

        let body = if let Some(v) = obj.get("body") {
            self.block(v, &child(ptr, "body"), scope, "`body`", 1)
        } else {
            self.missing(ptr, "repeat", "body");
            None
        };

        Some(InstructionKind::Repeat(Repeat {
            mode: mode?,
            body: body?,
        }))
    }

    fn if_kind(
        &mut self,
        obj: &Map<String, Value>,
        ptr: &str,
        scope: Scope,
    ) -> Option<InstructionKind> {
        let condition = obj
            .get("if")
            .and_then(|v| self.expression(v, &child(ptr, "if"), scope));
        let then = if let Some(v) = obj.get("then") {
            self.block(v, &child(ptr, "then"), scope, "`then`", 1)
        } else {
            self.missing(ptr, "if", "then");
            None
        };
        let otherwise = match obj.get("else") {
            None => Some(None),
            Some(v) => self
                .block(v, &child(ptr, "else"), scope, "`else`", 1)
                .map(Some),
        };
        Some(InstructionKind::If {
            condition: condition?,
            then: then?,
            otherwise: otherwise?,
        })
    }

    fn set_kind(
        &mut self,
        obj: &Map<String, Value>,
        ptr: &str,
        scope: Scope,
    ) -> Option<InstructionKind> {
        let entries = obj.get("set")?;
        let sptr = child(ptr, "set");
        let Value::Object(entries) = entries else {
            self.issue(&sptr, "`set` must be an object of session.* keys");
            return None;
        };
        if entries.is_empty() {
            self.issue(&sptr, "`set` must contain at least one entry");
            return None;
        }
        let mut out: Vec<SetEntry> = Vec::with_capacity(entries.len());
        let mut ok = true;
        for (key, v) in entries {
            let eptr = child(&sptr, key);
            let Some(path) = self.set_key(key, &eptr) else {
                ok = false;
                continue;
            };
            match self.expression(v, &eptr, scope) {
                Some(value) => out.push(SetEntry { path, value }),
                None => ok = false,
            }
        }
        // Overlapping paths within one `set` would make the write order
        // observable; all values are evaluated before any write, so the
        // format forbids the ambiguity outright. Detection is a sorted
        // prefix-stack scan rather than a pairwise loop: `/validate`
        // takes untrusted input, and in Vec-lexicographic order a prefix
        // sorts immediately before its extensions, so O(n log n) covers
        // what the pairwise scan did in O(n²). Each entry reports its
        // nearest prefix ancestor, bounding the issues at one per entry.
        let mut sorted: Vec<&SetEntry> = out.iter().collect();
        sorted.sort_by(|a, b| a.path.cmp(&b.path));
        let mut overlaps = Vec::new();
        let mut prefix_chain: Vec<&SetEntry> = Vec::new();
        for entry in sorted {
            while let Some(top) = prefix_chain.last() {
                if entry.path.starts_with(&top.path) {
                    break;
                }
                prefix_chain.pop();
            }
            if let Some(prefix) = prefix_chain.last() {
                overlaps.push((prefix.key(), entry.key()));
            }
            prefix_chain.push(entry);
        }
        for (prefix, extension) in overlaps {
            self.issue(
                &child(&sptr, &extension),
                format!(
                    "set keys `{prefix}` and `{extension}` overlap — one is a path prefix \
                     of the other, so the write order would be ambiguous"
                ),
            );
            ok = false;
        }
        ok.then_some(InstructionKind::Set(out))
    }

    fn set_key(&mut self, key: &str, ptr: &str) -> Option<Vec<String>> {
        let mut segments = key.split('.');
        let root_ok = segments.next() == Some("session");
        let mut path = Vec::new();
        let mut segs_ok = true;
        for seg in segments {
            let mut chars = seg.chars();
            let head_ok = chars.next().is_some_and(|c| c.is_ascii_alphabetic());
            let tail_ok = chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
            segs_ok &= head_ok && tail_ok;
            path.push(seg.to_owned());
        }
        if root_ok && key.starts_with("session._") {
            self.issue(
                ptr,
                format!(
                    "`{key}` writes reserved engine state — `session._*` keys \
                     (`session._once`, `session._triggers`) cannot be set by a document"
                ),
            );
            return None;
        }
        if !root_ok || path.is_empty() || !segs_ok {
            self.issue(
                ptr,
                format!(
                    "`{key}` is not a valid set key — keys are `session.*` paths of \
                     dot-separated segments, each a letter followed by letters, digits, \
                     or underscores"
                ),
            );
            return None;
        }
        Some(path)
    }

    fn try_kind(
        &mut self,
        obj: &Map<String, Value>,
        ptr: &str,
        scope: Scope,
    ) -> Option<InstructionKind> {
        let error_scope = Scope {
            error_ok: true,
            ..scope
        };
        let body = obj
            .get("try")
            .and_then(|v| self.block(v, &child(ptr, "try"), scope, "`try`", 1));
        let catch = match obj.get("catch") {
            None => Some(None),
            Some(v) => self
                .block(v, &child(ptr, "catch"), error_scope, "`catch`", 1)
                .map(Some),
        };
        let finally = match obj.get("finally") {
            None => Some(None),
            Some(v) => self
                .block(v, &child(ptr, "finally"), error_scope, "`finally`", 1)
                .map(Some),
        };
        Some(InstructionKind::Try {
            body: body?,
            catch: catch?,
            finally: finally?,
        })
    }

    fn fail_kind(
        &mut self,
        obj: &Map<String, Value>,
        ptr: &str,
        scope: Scope,
    ) -> Option<InstructionKind> {
        let f = obj.get("fail")?;
        let fptr = child(ptr, "fail");
        let Value::Object(f) = f else {
            self.issue(
                &fptr,
                "`fail` must be an object with a `message` expression",
            );
            return None;
        };
        for k in f.keys() {
            if k != "message" {
                self.issue(
                    &child(&fptr, k),
                    format!("unknown key `{k}` in `fail` (allowed: `message`)"),
                );
            }
        }
        let message = if let Some(v) = f.get("message") {
            self.expression(v, &child(&fptr, "message"), scope)
        } else {
            self.issue(
                &fptr,
                "`fail` requires a `message` expression — quote it (e.g. \
                 \"'exposure never converged'\") for a fixed string",
            );
            None
        };
        Some(InstructionKind::Fail { message: message? })
    }

    fn wait_kind(
        &mut self,
        obj: &Map<String, Value>,
        ptr: &str,
        scope: Scope,
    ) -> Option<InstructionKind> {
        let w = obj.get("wait")?;
        let wptr = child(ptr, "wait");
        let Value::Object(w) = w else {
            self.issue(
                &wptr,
                "`wait` must be an object with one of `duration`, `until_event`, `until`",
            );
            return None;
        };
        for k in w.keys() {
            if ![
                "duration",
                "until_event",
                "until",
                "timeout",
                "poll_interval",
            ]
            .contains(&k.as_str())
            {
                self.issue(
                    &child(&wptr, k),
                    format!(
                        "unknown key `{k}` in `wait` (allowed: `duration`, `until_event`, \
                         `until`, `timeout`, `poll_interval`)"
                    ),
                );
            }
        }
        let variants: Vec<&str> = ["duration", "until_event", "until"]
            .into_iter()
            .filter(|m| w.contains_key(*m))
            .collect();
        let variant = match variants.as_slice() {
            [v] => *v,
            [] => {
                self.issue(
                    &wptr,
                    "`wait` requires exactly one of `duration`, `until_event`, `until`",
                );
                return None;
            }
            many => {
                self.issue(
                    &wptr,
                    format!(
                        "`wait` takes exactly one of `duration`, `until_event`, `until`; \
                         found {}",
                        quoted_list(many)
                    ),
                );
                return None;
            }
        };

        let timeout = |b: &mut Self| {
            if let Some(v) = w.get("timeout") {
                b.duration_field(v, &child(&wptr, "timeout"), "`timeout`")
            } else {
                b.issue(
                    &wptr,
                    format!(
                        "a `wait` on `{variant}` requires a `timeout` — expiry raises a \
                     workflow error"
                    ),
                );
                None
            }
        };

        match variant {
            "duration" => {
                let stray_timeout = w.contains_key("timeout");
                if stray_timeout {
                    self.issue(
                        &child(&wptr, "timeout"),
                        "`timeout` is not used with a fixed-`duration` wait",
                    );
                }
                let stray_poll = w.contains_key("poll_interval");
                if stray_poll {
                    self.issue(
                        &child(&wptr, "poll_interval"),
                        "`poll_interval` only applies to an `until` wait",
                    );
                }
                let d = w
                    .get("duration")
                    .and_then(|v| self.duration_field(v, &child(&wptr, "duration"), "`duration`"));
                if stray_timeout || stray_poll {
                    return None;
                }
                Some(InstructionKind::Wait(Wait::Duration(d?)))
            }
            "until_event" => {
                let ok = if w.contains_key("poll_interval") {
                    self.issue(
                        &child(&wptr, "poll_interval"),
                        "`poll_interval` only applies to an `until` wait",
                    );
                    false
                } else {
                    true
                };
                let event = w.get("until_event").and_then(|v| {
                    self.string_field(v, &child(&wptr, "until_event"), "`until_event`")
                });
                let t = timeout(self);
                if !ok {
                    return None;
                }
                Some(InstructionKind::Wait(Wait::UntilEvent {
                    event: event?,
                    timeout: t?,
                }))
            }
            _ => {
                let condition = w
                    .get("until")
                    .and_then(|v| self.expression(v, &child(&wptr, "until"), scope));
                let poll_interval = match w.get("poll_interval") {
                    Some(v) => {
                        self.duration_field(v, &child(&wptr, "poll_interval"), "`poll_interval`")
                    }
                    None => Some(std::time::Duration::from_secs(10)),
                };
                let t = timeout(self);
                Some(InstructionKind::Wait(Wait::Until {
                    condition: condition?,
                    poll_interval: poll_interval?,
                    timeout: t?,
                }))
            }
        }
    }

    fn log_kind(
        &mut self,
        obj: &Map<String, Value>,
        ptr: &str,
        scope: Scope,
    ) -> Option<InstructionKind> {
        let l = obj.get("log")?;
        let lptr = child(ptr, "log");
        let Value::Object(l) = l else {
            self.issue(&lptr, "`log` must be an object with a `message`");
            return None;
        };
        for k in l.keys() {
            if !["level", "message", "values"].contains(&k.as_str()) {
                self.issue(
                    &child(&lptr, k),
                    format!("unknown key `{k}` in `log` (allowed: `level`, `message`, `values`)"),
                );
            }
        }
        let level = match l.get("level") {
            None => Some(LogLevel::Debug),
            Some(Value::String(s)) if s == "debug" => Some(LogLevel::Debug),
            Some(Value::String(s)) if s == "info" => Some(LogLevel::Info),
            Some(_) => {
                self.issue(
                    &child(&lptr, "level"),
                    "`level` must be \"debug\" or \"info\"",
                );
                None
            }
        };
        let message = if let Some(v) = l.get("message") {
            self.string_field(v, &child(&lptr, "message"), "`message`")
        } else {
            self.issue(&lptr, "`log` requires a `message` string");
            None
        };
        let values = match l.get("values") {
            None => Some(BTreeMap::new()),
            Some(Value::Object(m)) => {
                let vptr = child(&lptr, "values");
                let mut out = BTreeMap::new();
                let mut ok = true;
                for (k, v) in m {
                    match self.expression(v, &child(&vptr, k), scope) {
                        Some(e) => {
                            out.insert(k.clone(), e);
                        }
                        None => ok = false,
                    }
                }
                ok.then_some(out)
            }
            Some(_) => {
                self.issue(
                    &child(&lptr, "values"),
                    "`values` must be an object of expressions",
                );
                None
            }
        };
        Some(InstructionKind::Log(Log {
            level: level?,
            message: message?,
            values: values?,
        }))
    }

    // ---- triggers ---------------------------------------------------------

    fn triggers(&mut self, value: Option<&Value>) -> Option<Vec<Trigger>> {
        let Some(value) = value else {
            return Some(Vec::new());
        };
        let Value::Array(items) = value else {
            self.issue("/triggers", "`triggers` must be an array");
            return None;
        };
        let mut out = Vec::with_capacity(items.len());
        let mut ok = true;
        for (i, item) in items.iter().enumerate() {
            match self.trigger(item, &element("/triggers", i)) {
                Some(t) => out.push(t),
                None => ok = false,
            }
        }
        ok.then_some(out)
    }

    fn trigger(&mut self, v: &Value, ptr: &str) -> Option<Trigger> {
        let Value::Object(obj) = v else {
            self.issue(ptr, "a trigger must be a JSON object");
            return None;
        };
        for k in obj.keys() {
            if !["id", "on", "when", "while", "cooldown", "once", "do"].contains(&k.as_str()) {
                self.issue(
                    &child(ptr, k),
                    format!(
                        "unknown key `{k}` in a trigger (allowed: `id`, `on`, `when`, \
                         `while`, `cooldown`, `once`, `do`)"
                    ),
                );
            }
        }
        let scope = Scope {
            event_ok: true,
            error_ok: false,
        };

        let id = if let Some(v) = obj.get("id") {
            self.string_field(v, &child(ptr, "id"), "a trigger `id`")
                .and_then(|id| {
                    if let Some(first) = self.trigger_ids.get(&id) {
                        let first = first.clone();
                        self.issue(
                            &child(ptr, "id"),
                            format!(
                                "duplicate trigger id `{id}` — trigger ids must be unique \
                         within a document (also used at {first})"
                            ),
                        );
                        return None;
                    }
                    self.trigger_ids.insert(id.clone(), child(ptr, "id"));
                    Some(id)
                })
        } else {
            self.issue(ptr, "a trigger requires an `id`");
            None
        };

        let on = if let Some(v) = obj.get("on") {
            self.trigger_source(v, &child(ptr, "on"))
        } else {
            self.issue(ptr, "a trigger requires an `on` source");
            None
        };

        let when = match obj.get("when") {
            None => Some(None),
            Some(v) => self.expression(v, &child(ptr, "when"), scope).map(Some),
        };
        let while_gate = match obj.get("while") {
            None => Some(None),
            Some(v) => self.expression(v, &child(ptr, "while"), scope).map(Some),
        };
        let cooldown = match obj.get("cooldown") {
            None => Some(None),
            Some(v) => self
                .duration_field(v, &child(ptr, "cooldown"), "`cooldown`")
                .map(Some),
        };
        let once = match obj.get("once") {
            None => Some(false),
            Some(Value::Bool(b)) => Some(*b),
            Some(_) => {
                self.issue(
                    &child(ptr, "once"),
                    "a trigger's `once` is a boolean (fire at most once per session) — \
                     unlike an instruction's `once`, which is an idempotency-key string",
                );
                None
            }
        };
        let actions = if let Some(v) = obj.get("do") {
            self.block(v, &child(ptr, "do"), scope, "`do`", 1)
        } else {
            self.issue(ptr, "a trigger requires a `do` block");
            None
        };

        Some(Trigger {
            id: id?,
            on: on?,
            when: when?,
            while_gate: while_gate?,
            cooldown: cooldown?,
            once: once?,
            actions: actions?,
        })
    }

    fn trigger_source(&mut self, v: &Value, ptr: &str) -> Option<TriggerSource> {
        let Value::Object(obj) = v else {
            self.issue(
                ptr,
                "`on` must be an object: {\"event\": \"…\"} or {\"poll\": {…}}",
            );
            return None;
        };
        let keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        match keys.as_slice() {
            ["event"] => obj
                .get("event")
                .and_then(|v| self.string_field(v, &child(ptr, "event"), "`event`"))
                .map(TriggerSource::Event),
            ["poll"] => {
                let p = obj.get("poll")?;
                let pptr = child(ptr, "poll");
                let Value::Object(p) = p else {
                    self.issue(&pptr, "`poll` must be an object with `tool` and `interval`");
                    return None;
                };
                for k in p.keys() {
                    if !["tool", "args", "interval"].contains(&k.as_str()) {
                        self.issue(
                            &child(&pptr, k),
                            format!(
                                "unknown key `{k}` in `poll` (allowed: `tool`, `args`, \
                                 `interval`)"
                            ),
                        );
                    }
                }
                let tool = if let Some(v) = p.get("tool") {
                    self.string_field(v, &child(&pptr, "tool"), "`tool`")
                } else {
                    self.issue(&pptr, "`poll` requires a `tool`");
                    None
                };
                // Poll args are evaluated outside any instruction or
                // event context: only params/session/result are in scope.
                let args = self.args(p.get("args"), &child(&pptr, "args"), TREE);
                let interval = if let Some(v) = p.get("interval") {
                    match self.duration_field(v, &child(&pptr, "interval"), "`interval`") {
                        // A zero interval would make the poll due at every
                        // safe point — a busy loop against `rp`.
                        Some(d) if d.is_zero() => {
                            self.issue(
                                &child(&pptr, "interval"),
                                "a poll `interval` must be positive",
                            );
                            None
                        }
                        other => other,
                    }
                } else {
                    self.issue(&pptr, "`poll` requires an `interval`");
                    None
                };
                Some(TriggerSource::Poll {
                    tool: tool?,
                    args: args?,
                    interval: interval?,
                })
            }
            _ => {
                self.issue(ptr, "`on` takes exactly one of `event` or `poll`");
                None
            }
        }
    }
}
