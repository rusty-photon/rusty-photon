//! Workflow execution: the interpreter that runs a validated
//! [`Document`]'s procedure tree against `rp`'s tool catalog and the
//! session blackboard.
//!
//! The normative execution contract — instruction semantics, `result`
//! scoping, error propagation, the re-entrancy contract, safety behavior —
//! is `docs/services/session-runner.md`; this module implements it against
//! two seams so unit tests need no `rp`: a [`ToolClient`] (the real MCP
//! client arrives with the Phase C service wiring) and a [`Clock`].
//!
//! Phase boundary (`docs/plans/archive/workflow-dsl.md`): the Phase C engine core
//! plus the Phase D event intake (`wait` `until_event` against the SSE
//! stream) and trigger engine — the safe-point pump, `when`/`while`
//! gates, `once`/`cooldown` bookkeeping, poll sources, and synthetic
//! `correction_requested` events (design § Triggers).

mod exec;
mod io;

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod exec_tests;

use serde_json::{json, Value};
use tracing::{debug, info};

pub use io::{Clock, EngineEvent, EventIntake, SystemClock, ToolCallError, ToolClient};

use crate::blackboard::Blackboard;
use crate::document::Document;

/// A workflow error: raised by a failed tool call (after retries), an
/// expression evaluation error, a `fail` instruction, a `wait` timeout,
/// or a blackboard write failure.
///
/// Propagates outward through enclosing `try` instructions.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct WorkflowError {
    pub message: String,
    /// The raising instruction's own `id`, when it declares one.
    pub instruction_id: Option<String>,
    /// The tool name when the error came from a tool call; `None`
    /// otherwise.
    pub tool: Option<String>,
}

impl WorkflowError {
    /// The `error.*` namespace value visible in `catch`/`finally`.
    fn to_value(&self) -> Value {
        json!({
            "message": self.message,
            "instruction_id": self.instruction_id,
            "tool": self.tool,
        })
    }
}

/// How a run ended.
#[derive(Debug, PartialEq, Eq)]
pub enum RunOutcome {
    /// The procedure tree ran to completion — post `outcome: "complete"`.
    Completed,
    /// An uncaught workflow error — post `outcome: "failed"` with the
    /// error message.
    Failed(WorkflowError),
    /// `rp` terminated the MCP session (safety). The blackboard is
    /// current (write-on-mutation invariant); the caller exits **without**
    /// posting a completion and awaits re-invocation with recovery
    /// context.
    Terminated,
}

/// Execute `doc`'s procedure tree to completion.
///
/// `params` is the bound parameter object from
/// [`crate::document::bind_parameters`]; `blackboard` is empty for a fresh
/// session or reloaded for a recovery invocation — re-execution from the
/// root against the persisted blackboard *is* the resume model (design
/// § Re-entrancy Contract). `events` is the session's event intake
/// (subscribed before the first instruction, so an event emitted while an
/// earlier instruction ran still satisfies a later `until_event` wait).
pub async fn run<T, C>(
    doc: &Document,
    params: &Value,
    blackboard: &mut Blackboard,
    tools: &T,
    clock: &C,
    events: EventIntake,
) -> RunOutcome
where
    T: ToolClient + Sync,
    C: Clock + Sync,
{
    let mut exec = exec::Exec::new(params, blackboard, tools, clock, events, &doc.triggers);
    match exec.exec_block(std::slice::from_ref(&doc.root)).await {
        Ok(()) => {
            debug!(document = %doc.name, "workflow completed");
            RunOutcome::Completed
        }
        Err(exec::Interrupt::Error(error)) => {
            debug!(document = %doc.name, %error, "workflow failed");
            RunOutcome::Failed(error)
        }
        Err(exec::Interrupt::Terminated) => {
            info!(
                document = %doc.name,
                "MCP session terminated by rp; exiting without completion and awaiting \
                 re-invocation"
            );
            RunOutcome::Terminated
        }
    }
}
