//! Workflow-document loading and validation.
//!
//! The normative contract is `docs/services/session-runner.md`
//! (§ Workflow Documents, § Validation); the published external contract
//! is `schema/workflow-v1.schema.json`. This module implements
//! validation **layer 1** (document shape) and **layer 3** (invocation
//! parameters); layer 2 (catalog validation against `rp`'s `tools/list`)
//! arrives with the MCP client.
//!
//! Entry points:
//!
//! - [`Document::parse`] / [`Document::from_value`] validate raw JSON and
//!   build the typed model, reporting **all** findings (not just the
//!   first) as [`ValidationIssue`]s with RFC 6901 JSON Pointers — the
//!   payload `/validate` returns and `/invoke` fails loudly with.
//! - [`bind_parameters`] checks invocation parameters against the
//!   document's declarations and materializes the `params.*` namespace.
//! - [`resolve_workflow_path`] maps a `config.workflow` name to a path
//!   under `workflows_dir`.

mod catalog;
mod duration;
mod locate;
mod model;
mod params;
mod validate;

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod catalog_tests;

// `pub(crate)`: the engine's tests execute the corpus's golden documents.
#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
pub(crate) mod corpus;
#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod golden_tests;
#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod params_tests;
#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod schema_agreement_tests;
#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod validate_tests;

use serde::Serialize;
use serde_json::Value;

pub use catalog::{validate_against_catalog, ToolSpec};
pub use locate::resolve_workflow_path;
pub use model::{
    ArgValue, Bound, Document, Instruction, InstructionKind, Log, LogLevel, ParameterDecl,
    ParameterType, Repeat, RepeatMode, Retry, SetEntry, ToolCall, Trigger, TriggerSource, Wait,
};
pub use params::bind_parameters;

use crate::expr::Span;

/// One validation finding: an RFC 6901 JSON Pointer into the offending
/// document location plus a message. Expression errors additionally
/// carry the byte span within the expression string at that location.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, thiserror::Error)]
#[error("{pointer}: {message}")]
pub struct ValidationIssue {
    pub pointer: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expr_span: Option<Span>,
}

impl Document {
    /// Parses and validates a workflow document from JSON text,
    /// reporting every finding.
    ///
    /// # Errors
    ///
    /// Returns a single issue if `src` is not valid JSON, otherwise
    /// every [`ValidationIssue`] the document validator finds.
    pub fn parse(src: &str) -> Result<Self, Vec<ValidationIssue>> {
        match serde_json::from_str::<Value>(src) {
            Ok(value) => Self::from_value(&value),
            Err(e) => Err(vec![ValidationIssue {
                pointer: String::new(),
                message: format!("not valid JSON: {e}"),
                expr_span: None,
            }]),
        }
    }

    /// Validates an already-parsed JSON value (the `/validate` route
    /// receives the document embedded in its request body).
    ///
    /// # Errors
    ///
    /// Returns every [`ValidationIssue`] the document validator finds.
    pub fn from_value(value: &Value) -> Result<Self, Vec<ValidationIssue>> {
        validate::build(value)
    }
}
