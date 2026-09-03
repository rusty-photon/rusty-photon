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
use tokio_util::sync::CancellationToken;
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

/// Why a run paused (design § Safety Behavior).
///
/// The blackboard is current (write-on-mutation invariant), no `finally`
/// block ran, and the caller waits for the condition to clear before
/// re-executing the document from the root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PauseReason {
    /// `rp` cancelled or refused a call for safety; wait for safe
    /// conditions.
    Safety(String),
    /// `rp` is unreachable; wait for it to come back.
    RpOutage(String),
}

impl PauseReason {
    /// The `paused_reason` value reported on the run record.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Safety(_) => "safety",
            Self::RpOutage(_) => "rp_outage",
        }
    }

    /// The error text behind the pause.
    #[must_use]
    pub fn detail(&self) -> &str {
        match self {
            Self::Safety(message) | Self::RpOutage(message) => message,
        }
    }
}

/// How a run ended.
#[derive(Debug, PartialEq, Eq)]
pub enum RunOutcome {
    /// The procedure tree ran to completion — `outcome: "complete"`.
    Completed,
    /// An uncaught workflow error — `outcome: "failed"` with the error
    /// message.
    Failed(WorkflowError),
    /// The run is paused, not over: the caller waits out the reason and
    /// re-executes from the root (design § Safety Behavior).
    Paused(PauseReason),
    /// The stop token fired: the run ended at a safe point after its
    /// best-effort `finally` blocks — `outcome: "stopped"`.
    Stopped,
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
    run_with_stop(
        doc,
        params,
        blackboard,
        tools,
        clock,
        events,
        &CancellationToken::new(),
    )
    .await
}

/// [`run`] with a stop token.
///
/// Once `stop` is cancelled the run ends at its next safe point (the
/// in-flight tool call completes first), enclosing `finally` blocks run
/// best-effort, and the outcome is [`RunOutcome::Stopped`] (design
/// § Runs → `POST /runs/{id}/stop`).
pub async fn run_with_stop<T, C>(
    doc: &Document,
    params: &Value,
    blackboard: &mut Blackboard,
    tools: &T,
    clock: &C,
    events: EventIntake,
    stop: &CancellationToken,
) -> RunOutcome
where
    T: ToolClient + Sync,
    C: Clock + Sync,
{
    let mut exec = exec::Exec::new(
        params,
        blackboard,
        tools,
        clock,
        events,
        &doc.triggers,
        stop,
    );
    match exec.exec_block(std::slice::from_ref(&doc.root)).await {
        Ok(()) => {
            debug!(document = %doc.name, "workflow completed");
            RunOutcome::Completed
        }
        Err(exec::Interrupt::Error(error)) => {
            debug!(document = %doc.name, %error, "workflow failed");
            RunOutcome::Failed(error)
        }
        Err(exec::Interrupt::Paused(reason)) => {
            info!(
                document = %doc.name,
                reason = reason.name(),
                detail = reason.detail(),
                "run paused; waiting to resume from the root"
            );
            RunOutcome::Paused(reason)
        }
        Err(exec::Interrupt::Stopped) => {
            info!(document = %doc.name, "run stopped on request");
            RunOutcome::Stopped
        }
    }
}
