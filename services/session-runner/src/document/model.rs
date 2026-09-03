//! The typed workflow-document model. Instances are produced only by the
//! validation walk in [`super::validate`] (parse-don't-validate): every
//! invariant the format pins — exactly one discriminant per instruction,
//! loop bounds present, `wait` variants complete, expressions parsed — is
//! encoded in these types, so an engine holding a [`Document`] never
//! re-checks document shape.

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::Value;

use crate::expr::Expression;

/// A validated workflow document (format version 1).
#[derive(Clone, Debug, PartialEq)]
pub struct Document {
    /// Format version; always `1` — the version gate rejects everything
    /// else before the model is built.
    pub version: u64,
    /// Identification for logs, events, and the run's outcome payload.
    pub name: String,
    pub description: Option<String>,
    /// Declared run parameters, keyed by name.
    pub parameters: BTreeMap<String, ParameterDecl>,
    /// Advisory run-length estimates for the document's readers —
    /// parsed and validated, enforced by nothing (the legacy `/invoke`
    /// acknowledgment that returned them went with mcp-sessionless
    /// slice 7).
    pub estimated_duration: Option<Duration>,
    pub max_duration: Option<Duration>,
    /// Document-global reactive rules.
    pub triggers: Vec<Trigger>,
    /// The procedure tree.
    pub root: Instruction,
}

/// A declared run parameter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParameterDecl {
    pub ty: ParameterType,
    /// `None` = the parameter is required; `Some` = optional with this
    /// default. A `duration` default is kept as its humantime string —
    /// expressions read duration parameters as strings (`seconds()`
    /// converts).
    pub default: Option<Value>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParameterType {
    String,
    Integer,
    Number,
    Boolean,
    Duration,
    /// An opaque JSON array. v1 declares no element shape — element
    /// errors surface as loud expression errors at run time (typed
    /// element declarations are the deferred "array-parameter
    /// ergonomics" in the design doc's MVP boundary).
    Array,
}

impl ParameterType {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Integer => "integer",
            Self::Number => "number",
            Self::Boolean => "boolean",
            Self::Duration => "duration",
            Self::Array => "array",
        }
    }
}

/// One instruction node: a discriminant-specific payload plus the common
/// `id` (log label) and `once` (re-entrancy idempotency key) fields.
#[derive(Clone, Debug, PartialEq)]
pub struct Instruction {
    pub id: Option<String>,
    pub once: Option<String>,
    pub kind: InstructionKind,
}

#[derive(Clone, Debug, PartialEq)]
pub enum InstructionKind {
    Tool(ToolCall),
    Sequence(Vec<Instruction>),
    Repeat(Repeat),
    If {
        condition: Expression,
        then: Vec<Instruction>,
        otherwise: Option<Vec<Instruction>>,
    },
    /// Blackboard writes. Entries are guaranteed non-overlapping (no
    /// entry's path is a prefix of another's), so their write order
    /// cannot matter.
    Set(Vec<SetEntry>),
    Try {
        body: Vec<Instruction>,
        catch: Option<Vec<Instruction>>,
        finally: Option<Vec<Instruction>>,
    },
    Fail {
        message: Expression,
    },
    Wait(Wait),
    Log(Log),
}

/// An MCP tool call.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolCall {
    pub tool: String,
    pub args: BTreeMap<String, ArgValue>,
    pub retry: Option<Retry>,
}

/// A tool-call argument value: literal JSON by default, or a computed
/// expression wrapped as `{ "$expr": "…" }` in the document.
#[derive(Clone, Debug, PartialEq)]
pub enum ArgValue {
    /// Literal JSON, guaranteed free of nested `$expr` wrappers (the
    /// validator rejects them so a misplaced wrapper cannot silently
    /// travel to the tool as data).
    Literal(Value),
    Expr(Expression),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Retry {
    /// Total attempts (≥ 1), not retries-after-the-first.
    pub max_attempts: u64,
    /// Fixed delay between attempts.
    pub backoff: Duration,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Repeat {
    pub mode: RepeatMode,
    pub body: Vec<Instruction>,
}

/// The loop mode. `until`/`while` structurally require `max_iterations`
/// (unbounded loops are a validation error); `count` may carry one as an
/// extra guard.
#[derive(Clone, Debug, PartialEq)]
pub enum RepeatMode {
    /// Condition checked **after** each pass.
    Until {
        condition: Expression,
        max_iterations: Bound,
    },
    /// Condition checked **before** each pass.
    While {
        condition: Expression,
        max_iterations: Bound,
    },
    Count {
        count: Bound,
        max_iterations: Option<Bound>,
    },
}

/// A loop bound: a literal integer, or an expression evaluated once at
/// loop entry.
#[derive(Clone, Debug, PartialEq)]
pub enum Bound {
    Literal(u64),
    Expr(Expression),
}

/// One `set` entry: the `session.*` path segments after the `session`
/// root, and the value expression.
#[derive(Clone, Debug, PartialEq)]
pub struct SetEntry {
    pub path: Vec<String>,
    pub value: Expression,
}

impl SetEntry {
    /// The document-form key (`session.a.b`), for logs and errors.
    #[must_use]
    pub fn key(&self) -> String {
        let mut key = String::from("session");
        for seg in &self.path {
            key.push('.');
            key.push_str(seg);
        }
        key
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Wait {
    Duration(Duration),
    UntilEvent {
        /// An `rp` event name.
        event: String,
        timeout: Duration,
    },
    Until {
        condition: Expression,
        /// Re-evaluation interval; the document default is 10 s.
        poll_interval: Duration,
        timeout: Duration,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Log {
    pub level: LogLevel,
    pub message: String,
    /// Expression values rendered into the structured log record, in
    /// key order.
    pub values: BTreeMap<String, Expression>,
}

/// `debug` (the document default) or `info`, matching the workspace
/// logging rule.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LogLevel {
    #[default]
    Debug,
    Info,
}

/// A document-global trigger.
#[derive(Clone, Debug, PartialEq)]
pub struct Trigger {
    /// Unique within the document.
    pub id: String,
    pub on: TriggerSource,
    /// Fire gate over `event.*` + the usual namespaces; absent = always.
    pub when: Option<Expression>,
    /// Phase gate evaluated at fire time (the document's `while` field).
    pub while_gate: Option<Expression>,
    /// Minimum interval between firings.
    pub cooldown: Option<Duration>,
    /// Fire at most once per session.
    pub once: bool,
    /// The trigger's instruction block (the document's `do` field).
    pub actions: Vec<Instruction>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TriggerSource {
    /// An `rp` SSE event name, or the synthetic `correction_requested`.
    Event(String),
    /// The engine calls the tool on the interval; the result becomes
    /// `event.*`.
    Poll {
        tool: String,
        args: BTreeMap<String, ArgValue>,
        interval: Duration,
    },
}
