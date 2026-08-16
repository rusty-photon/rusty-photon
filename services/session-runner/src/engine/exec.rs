//! The procedure-tree walk: executes a validated [`Instruction`] tree
//! against the blackboard, the tool client, and the clock — and pumps the
//! document's triggers at every safe point along the way.
//!
//! Semantics implemented here are pinned in
//! `docs/services/session-runner.md` — § Instructions, § `result` scoping,
//! § Triggers (the safe-point pump and its implementation pins),
//! § Re-entrancy Contract (`once` markers), and § Safety Behavior (the
//! terminated-session path). Two interrupts propagate outward: a workflow
//! error (catchable by `try`) and a session termination (never caught;
//! `finally` blocks still run best-effort).

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use serde_json::{json, Map, Value};
use tracing::{debug, info, warn};

use crate::blackboard::Blackboard;
use crate::document::{
    ArgValue, Bound, Instruction, InstructionKind, Log, LogLevel, Repeat, RepeatMode, SetEntry,
    ToolCall, Trigger, TriggerSource, Wait,
};
use crate::expr::num::ExactInt;
use crate::expr::{EvalContext, Expression};

use super::{Clock, EngineEvent, EventIntake, ToolCallError, ToolClient, WorkflowError};

/// Why execution stopped early.
#[derive(Debug)]
pub(super) enum Interrupt {
    /// A workflow error: propagates outward through enclosing `try`s.
    Error(WorkflowError),
    /// `rp` terminated the MCP session (safety): skips `catch` blocks,
    /// runs `finally` blocks best-effort, and ends the run without a
    /// completion.
    Terminated,
}

type ExecResult = Result<(), Interrupt>;

/// One run's execution state.
pub(super) struct Exec<'a, T, C> {
    params: &'a Value,
    blackboard: &'a mut Blackboard,
    tools: &'a T,
    clock: &'a C,
    /// The session's event intake, live from before the first instruction.
    events: EventIntake,
    /// The document's triggers, pumped at safe points.
    triggers: &'a [Trigger],
    /// The `result` namespace: the structured result of the most recent
    /// result-producing instruction on the current path (`null` at
    /// session start).
    result: Value,
    /// The `error.*` namespace value while a `catch` or an error-path
    /// `finally` runs; `None` elsewhere (expressions read `null`).
    error: Option<Value>,
    /// The `event.*` namespace value while a trigger action runs; `None`
    /// on the procedure tree.
    event: Option<Value>,
    /// Set while a trigger action runs: safe points inside it only move
    /// intake events into `pending` — trigger evaluation never re-enters.
    in_trigger_action: bool,
    /// Events awaiting evaluation ahead of the intake: synthetic
    /// `correction_requested` events, plus events received by a wait's
    /// select or drained during a trigger action.
    pending: VecDeque<EngineEvent>,
    /// Unconsumed occurrences per event name — what an `until_event`
    /// wait matches against (every event since run start, § `wait`),
    /// maintained because the pump consumes the events themselves.
    occurrences: BTreeMap<String, u64>,
    /// Per-trigger queued firing (the `event.*` payload); at most one
    /// each — a queued or running trigger does not queue again.
    queued: Vec<Option<Value>>,
    /// Per-trigger next poll due on the monotonic clock; `None` for
    /// event triggers. First due is one interval after run start.
    poll_due: Vec<Option<Duration>>,
}

impl<'a, T, C> Exec<'a, T, C>
where
    T: ToolClient + Sync,
    C: Clock + Sync,
{
    pub(super) fn new(
        params: &'a Value,
        blackboard: &'a mut Blackboard,
        tools: &'a T,
        clock: &'a C,
        events: EventIntake,
        triggers: &'a [Trigger],
    ) -> Self {
        // Document validation caps every duration at 24h, so the add
        // returns `None` only if the monotonic clock itself sits within
        // a day of `Duration::MAX`. Defense in depth for that case: a
        // `None` leaves the poll due at every safe point, which is safe
        // (polls only read).
        let poll_due = triggers
            .iter()
            .map(|t| match &t.on {
                TriggerSource::Poll { interval, .. } => clock.monotonic().checked_add(*interval),
                TriggerSource::Event(_) => None,
            })
            .collect();
        Self {
            params,
            blackboard,
            tools,
            clock,
            events,
            triggers,
            result: Value::Null,
            error: None,
            event: None,
            in_trigger_action: false,
            pending: VecDeque::new(),
            occurrences: BTreeMap::new(),
            queued: vec![None; triggers.len()],
            poll_due,
        }
    }

    fn ctx(&self) -> EvalContext<'_> {
        EvalContext {
            params: Some(self.params),
            session: Some(self.blackboard.value()),
            result: Some(&self.result),
            event: self.event.as_ref(),
            error: self.error.as_ref(),
            now: self.clock.now(),
        }
    }

    /// Run a block of instructions in order, with a safe point after each
    /// (§ Triggers: queued trigger actions run after the current
    /// instruction completes).
    pub(super) async fn exec_block(&mut self, block: &'a [Instruction]) -> ExecResult {
        for ins in block {
            self.exec_boxed(ins).await?;
            self.safe_point().await?;
        }
        Ok(())
    }

    /// The boxed recursion point: `exec_instruction`'s future embeds
    /// container execution, which re-enters here — the box breaks the
    /// otherwise-infinite future type. Depth is bounded by the document
    /// layer's 128-container nesting gate.
    fn exec_boxed<'s>(
        &'s mut self,
        ins: &'a Instruction,
    ) -> Pin<Box<dyn Future<Output = ExecResult> + Send + 's>> {
        Box::pin(self.exec_instruction(ins))
    }

    async fn exec_instruction(&mut self, ins: &'a Instruction) -> ExecResult {
        if let Some(key) = &ins.once {
            if self.blackboard.once_done(key) {
                // A skipped instruction produces nothing: `result` is left
                // unchanged, exactly as if the instruction were absent.
                debug!(once = %key, "skipping instruction: `once` marker already recorded");
                return Ok(());
            }
        }
        match &ins.kind {
            InstructionKind::Tool(call) => self.exec_tool(ins, call).await?,
            InstructionKind::Sequence(body) => self.exec_block(body).await?,
            InstructionKind::Repeat(repeat) => self.exec_repeat(ins, repeat).await?,
            InstructionKind::If {
                condition,
                then,
                otherwise,
            } => {
                if self.condition(ins, condition, "`if` condition")? {
                    self.exec_block(then).await?;
                } else if let Some(otherwise) = otherwise {
                    self.exec_block(otherwise).await?;
                }
            }
            InstructionKind::Set(entries) => self.exec_set(ins, entries).await?,
            InstructionKind::Try {
                body,
                catch,
                finally,
            } => {
                self.exec_try(body, catch.as_deref(), finally.as_deref())
                    .await?;
            }
            InstructionKind::Fail { message } => return Err(self.exec_fail(ins, message)),
            InstructionKind::Wait(wait) => self.exec_wait(ins, wait).await?,
            InstructionKind::Log(log) => self.exec_log(ins, log)?,
        }
        if let Some(key) = &ins.once {
            // Recorded only on successful completion — a failed
            // instruction re-runs on resume.
            debug!(once = %key, "recording `once` completion marker");
            self.blackboard
                .mark_once(key)
                .await
                .map_err(|e| Self::error_here(ins, e.to_string()))?;
        }
        Ok(())
    }

    /// Build a workflow error raised at this instruction.
    fn error_here(ins: &Instruction, message: String) -> Interrupt {
        Interrupt::Error(WorkflowError {
            message,
            instruction_id: ins.id.clone(),
            tool: None,
        })
    }

    /// Evaluate an expression; an evaluation error becomes a workflow
    /// error at this instruction, naming the expression's role.
    fn eval(&self, ins: &Instruction, expr: &Expression, role: &str) -> Result<Value, Interrupt> {
        expr.eval(&self.ctx())
            .map_err(|e| Self::error_here(ins, format!("{role} `{}` failed: {e}", expr.source())))
    }

    /// Evaluate an expression that must yield a boolean — no truthiness,
    /// matching the expression language's own logic operators.
    fn condition(
        &self,
        ins: &Instruction,
        expr: &Expression,
        role: &str,
    ) -> Result<bool, Interrupt> {
        match self.eval(ins, expr, role)? {
            Value::Bool(b) => Ok(b),
            other => Err(Self::error_here(
                ins,
                format!(
                    "{role} `{}` must yield a boolean, got {other}",
                    expr.source()
                ),
            )),
        }
    }

    async fn exec_tool(&mut self, ins: &'a Instruction, call: &'a ToolCall) -> ExecResult {
        let mut args = Map::new();
        for (name, arg) in &call.args {
            let value = match arg {
                ArgValue::Literal(v) => v.clone(),
                ArgValue::Expr(expr) => {
                    integral_arg(self.eval(ins, expr, &format!("tool argument `{name}`"))?)
                }
            };
            args.insert(name.clone(), value);
        }
        let max_attempts = call.retry.as_ref().map_or(1, |r| r.max_attempts);
        let mut attempt: u64 = 1;
        loop {
            debug!(tool = %call.tool, attempt, "calling tool");
            match self.tools.call(&call.tool, args.clone()).await {
                Ok(result) => {
                    if let Some(synthetic) = correction_event(&result) {
                        debug!(tool = %call.tool,
                               "tool result carries a correction; synthesizing \
                                correction_requested for the next safe point");
                        self.pending.push_back(synthetic);
                    }
                    self.result = result;
                    return Ok(());
                }
                Err(ToolCallError::SessionTerminated(message)) => {
                    debug!(tool = %call.tool, %message, "MCP session terminated during tool call");
                    return Err(Interrupt::Terminated);
                }
                Err(ToolCallError::Failed(message)) if attempt < max_attempts => {
                    debug!(
                        tool = %call.tool,
                        attempt,
                        max_attempts,
                        %message,
                        "tool call failed; retrying after backoff"
                    );
                    if let Some(retry) = &call.retry {
                        self.clock.sleep(retry.backoff).await;
                    }
                    attempt = attempt.saturating_add(1);
                }
                Err(ToolCallError::Failed(message)) => {
                    let message = if max_attempts > 1 {
                        format!(
                            "tool `{}` failed after {max_attempts} attempts: {message}",
                            call.tool
                        )
                    } else {
                        format!("tool `{}` failed: {message}", call.tool)
                    };
                    return Err(Interrupt::Error(WorkflowError {
                        message,
                        instruction_id: ins.id.clone(),
                        tool: Some(call.tool.clone()),
                    }));
                }
            }
        }
    }

    /// Evaluate a loop bound. Expressions must yield an integer-valued
    /// number; `minimum` is 1 for `max_iterations` (an exhausted budget
    /// must represent at least one pass) and 0 for `count`.
    fn bound(
        &self,
        ins: &Instruction,
        bound: &Bound,
        role: &str,
        minimum: u64,
    ) -> Result<u64, Interrupt> {
        let n = match bound {
            Bound::Literal(n) => *n,
            Bound::Expr(expr) => {
                let value = self.eval(ins, expr, role)?;
                integer_value(&value).ok_or_else(|| {
                    Self::error_here(
                        ins,
                        format!(
                            "{role} `{}` must yield a non-negative integer, got {value}",
                            expr.source()
                        ),
                    )
                })?
            }
        };
        if n < minimum {
            return Err(Self::error_here(
                ins,
                format!("{role} must be at least {minimum}, got {n}"),
            ));
        }
        Ok(n)
    }

    async fn exec_repeat(&mut self, ins: &'a Instruction, repeat: &'a Repeat) -> ExecResult {
        match &repeat.mode {
            RepeatMode::Until {
                condition,
                max_iterations,
            } => {
                let max = self.bound(ins, max_iterations, "`max_iterations`", 1)?;
                let mut iterations: u64 = 0;
                let mut converged = false;
                while iterations < max {
                    self.exec_block(&repeat.body).await?;
                    iterations = iterations.saturating_add(1);
                    // Checked after each pass, with the `result` that pass
                    // left in scope.
                    if self.condition(ins, condition, "`repeat` `until` condition")? {
                        converged = true;
                        break;
                    }
                }
                self.result = json!({ "iterations": iterations, "converged": converged });
            }
            RepeatMode::While {
                condition,
                max_iterations,
            } => {
                let max = self.bound(ins, max_iterations, "`max_iterations`", 1)?;
                let mut iterations: u64 = 0;
                let converged;
                loop {
                    // Checked before each potential pass — including after
                    // the final permitted one, so a condition that turns
                    // false exactly at the budget still converges.
                    if !self.condition(ins, condition, "`repeat` `while` condition")? {
                        converged = true;
                        break;
                    }
                    if iterations == max {
                        converged = false;
                        break;
                    }
                    self.exec_block(&repeat.body).await?;
                    iterations = iterations.saturating_add(1);
                }
                self.result = json!({ "iterations": iterations, "converged": converged });
            }
            RepeatMode::Count {
                count,
                max_iterations,
            } => {
                let n = self.bound(ins, count, "`count`", 0)?;
                if let Some(guard) = max_iterations {
                    let max = self.bound(ins, guard, "`max_iterations`", 1)?;
                    if n > max {
                        // The guard exists to bound a runaway `$expr`
                        // count: trip it loudly at loop entry rather than
                        // silently truncating the pass count.
                        return Err(Self::error_here(
                            ins,
                            format!("`count` ({n}) exceeds `max_iterations` ({max})"),
                        ));
                    }
                }
                for _ in 0..n {
                    self.exec_block(&repeat.body).await?;
                }
                // Count loops report no `converged` — there is no
                // condition to converge on.
                self.result = json!({ "iterations": n });
            }
        }
        Ok(())
    }

    async fn exec_set(&mut self, ins: &'a Instruction, entries: &'a [SetEntry]) -> ExecResult {
        // All values evaluate against the pre-write state — a `set`
        // cannot read its own writes.
        let mut writes = Vec::with_capacity(entries.len());
        for entry in entries {
            let role = format!("`set` value for `{}`", entry.key());
            writes.push(self.eval(ins, &entry.value, &role)?);
        }
        for (entry, value) in entries.iter().zip(writes) {
            self.blackboard
                .set_path(&entry.path, value)
                .map_err(|e| Self::error_here(ins, e.to_string()))?;
        }
        // One atomic persist per `set` instruction, before the next
        // instruction runs (the write-on-mutation invariant).
        self.blackboard
            .persist()
            .await
            .map_err(|e| Self::error_here(ins, e.to_string()))
    }

    async fn exec_try(
        &mut self,
        body: &'a [Instruction],
        catch: Option<&'a [Instruction]>,
        finally: Option<&'a [Instruction]>,
    ) -> ExecResult {
        let body_outcome = self.exec_block(body).await;
        let after_catch = match body_outcome {
            Err(Interrupt::Error(error)) => {
                if let Some(catch) = catch {
                    debug!(error = %error, "workflow error caught; running `catch`");
                    self.run_with_error_scope(catch, error.to_value()).await
                } else {
                    Err(Interrupt::Error(error))
                }
            }
            // A safety termination skips `catch` — the session is over
            // and `rp` has already secured the equipment.
            other => other,
        };
        let Some(finally) = finally else {
            return after_catch;
        };
        match after_catch {
            Ok(()) => {
                // Success path: the enclosing `error.*` scope (if any)
                // stays visible, and a `finally` failure is a real
                // workflow error.
                self.exec_block(finally).await
            }
            Err(interrupt) => {
                // Error path (or termination): best-effort — run, log
                // failures, never let them mask the original interrupt.
                let outcome = if let Interrupt::Error(error) = &interrupt {
                    self.run_with_error_scope(finally, error.to_value()).await
                } else {
                    self.exec_block(finally).await
                };
                match outcome {
                    Ok(()) => {}
                    // A termination mid-`finally` supersedes everything.
                    Err(Interrupt::Terminated) => return Err(Interrupt::Terminated),
                    Err(Interrupt::Error(finally_error)) => {
                        debug!(
                            error = %finally_error,
                            "`finally` block failed; propagating the original error"
                        );
                    }
                }
                Err(interrupt)
            }
        }
    }

    /// Run a `catch` or error-path `finally` block with `error.*` bound to
    /// the given value, restoring the enclosing scope's value afterwards.
    async fn run_with_error_scope(&mut self, block: &'a [Instruction], error: Value) -> ExecResult {
        let saved = self.error.replace(error);
        let outcome = self.exec_block(block).await;
        self.error = saved;
        outcome
    }

    fn exec_fail(&self, ins: &'a Instruction, message: &'a Expression) -> Interrupt {
        let message = match self.eval(ins, message, "`fail` message") {
            Ok(Value::String(s)) => s,
            // A non-string message is rendered as compact JSON — an error
            // message is terminal output, not data.
            Ok(other) => other.to_string(),
            Err(interrupt) => return interrupt,
        };
        Self::error_here(ins, message)
    }

    /// A `wait` is one long safe point (§ Triggers): every iteration pumps
    /// the triggers, then sleeps one segment — clamped to the next poll
    /// due and interrupted by an arriving event, which is buffered for the
    /// next iteration's pump. Budgets accumulate the monotonic time spent
    /// in the sleep segments alone, so a wall-clock step (NTP) can neither
    /// fire a timeout early nor extend a wait, and time spent in trigger
    /// actions (inside the pump) never counts.
    async fn exec_wait(&mut self, ins: &'a Instruction, wait: &'a Wait) -> ExecResult {
        match wait {
            Wait::Duration(duration) => {
                debug!(duration = %humantime::format_duration(*duration), "waiting");
                let mut remaining = *duration;
                loop {
                    self.safe_point().await?;
                    if remaining.is_zero() {
                        return Ok(());
                    }
                    remaining = remaining.saturating_sub(self.wait_segment(remaining).await);
                }
            }
            Wait::UntilEvent { event, timeout } => {
                debug!(
                    event = %event,
                    timeout = %humantime::format_duration(*timeout),
                    "waiting for event"
                );
                let mut elapsed = Duration::ZERO;
                loop {
                    // The pump counts every drained event's occurrence, so
                    // the consume below matches events from the whole run —
                    // including one emitted while an earlier instruction
                    // ran (design § `wait`). The pump runs once more when
                    // the budget expires, so an event that arrived exactly
                    // at expiry still satisfies the wait (mirroring
                    // `until`'s final evaluation at timeout).
                    self.safe_point().await?;
                    if self.consume_occurrence(event) {
                        return Ok(());
                    }
                    if elapsed >= *timeout {
                        return Err(Self::error_here(
                            ins,
                            format!(
                                "`wait` `until_event` `{event}` did not arrive within {}",
                                humantime::format_duration(*timeout)
                            ),
                        ));
                    }
                    elapsed = elapsed
                        .saturating_add(self.wait_segment(timeout.saturating_sub(elapsed)).await);
                }
            }
            Wait::Until {
                condition,
                poll_interval,
                timeout,
            } => {
                let mut elapsed = Duration::ZERO;
                loop {
                    // Evaluated on entry, after each interval (or sooner,
                    // when an event or trigger action ends a segment
                    // early), and once more when the timeout expires (the
                    // last sleep is clamped to the remaining budget).
                    self.safe_point().await?;
                    if self.condition(ins, condition, "`wait` `until` condition")? {
                        return Ok(());
                    }
                    if elapsed >= *timeout {
                        return Err(Self::error_here(
                            ins,
                            format!(
                                "`wait` `until` condition `{}` did not become true within {}",
                                condition.source(),
                                humantime::format_duration(*timeout)
                            ),
                        ));
                    }
                    let segment = (*poll_interval).min(timeout.saturating_sub(elapsed));
                    elapsed = elapsed.saturating_add(self.wait_segment(segment).await);
                }
            }
        }
    }

    /// One sleep segment of a wait: at most `remaining`, clamped to the
    /// next poll due so poll sources stay on schedule, and ended early by
    /// an arriving event (buffered into `pending` for the caller's next
    /// pump). Returns the monotonic time actually spent — the only time
    /// that counts against a wait's budget.
    async fn wait_segment(&mut self, remaining: Duration) -> Duration {
        let sleep_for = match self.next_poll_due_in() {
            Some(until_due) => remaining.min(until_due),
            None => remaining,
        };
        let waited_from = self.clock.monotonic();
        tokio::select! {
            // Biased so a just-arrived event beats an already-expired
            // sleep (the mock clock's sleeps resolve instantly).
            biased;
            received = self.events.next() => self.pending.push_back(received),
            () = self.clock.sleep(sleep_for) => {}
        }
        self.clock.monotonic().saturating_sub(waited_from)
    }

    /// Time until the earliest poll due, `None` when the document has no
    /// poll triggers (or inside a trigger action, where polls cannot run
    /// anyway — clamping there would spin on an already-due poll).
    fn next_poll_due_in(&self) -> Option<Duration> {
        if self.in_trigger_action {
            return None;
        }
        let now = self.clock.monotonic();
        self.poll_due
            .iter()
            .flatten()
            .map(|due| due.saturating_sub(now))
            .min()
    }

    /// Satisfy an `until_event`: decrement an unconsumed occurrence of
    /// `name`, or — inside a trigger action, where the pump does not
    /// count — consume a matching event straight from `pending`.
    fn consume_occurrence(&mut self, name: &str) -> bool {
        if let Some(n) = self.occurrences.get_mut(name) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                self.occurrences.remove(name);
            }
            return true;
        }
        if let Some(pos) = self.pending.iter().position(|e| e.event == name) {
            self.pending.remove(pos);
            return true;
        }
        false
    }

    // ---- the trigger pump (design § Triggers, implementation pins) -------

    /// A safe point on the procedure tree: feed buffered events (synthetic
    /// first, then the intake) to trigger evaluation, run due poll
    /// sources, then run queued trigger actions in document order.
    ///
    /// Inside a trigger action only the intake drain happens — evaluation
    /// never re-enters; everything drained there is evaluated at the next
    /// safe point on the tree.
    async fn safe_point(&mut self) -> ExecResult {
        while let Some(received) = self.events.try_next() {
            self.pending.push_back(received);
        }
        if self.in_trigger_action {
            return Ok(());
        }
        while let Some(received) = self.pending.pop_front() {
            let n = self.occurrences.entry(received.event.clone()).or_insert(0);
            *n = n.saturating_add(1);
            self.consider_event(&received)?;
        }
        self.run_due_polls().await?;
        self.run_queued().await
    }

    /// Queue every event trigger this event fires: name match, then the
    /// payload-independent gates, then the `when` gate over `event.*`.
    fn consider_event(&mut self, received: &EngineEvent) -> Result<(), Interrupt> {
        let triggers = self.triggers;
        for (idx, trigger) in triggers.iter().enumerate() {
            let TriggerSource::Event(name) = &trigger.on else {
                continue;
            };
            if *name != received.event || self.gated(idx, trigger) {
                continue;
            }
            if !self.gate_passes(trigger, trigger.when.as_ref(), &received.payload, "`when`")? {
                continue;
            }
            debug!(trigger = %trigger.id, event = %received.event, "trigger firing queued");
            if let Some(slot) = self.queued.get_mut(idx) {
                *slot = Some(received.payload.clone());
            } else {
                // `queued` is built with one slot per trigger; a miss is
                // an internal invariant break — loud, but no mid-session
                // panic.
                warn!(trigger = %trigger.id, idx, "trigger bookkeeping out of sync; dropping the firing");
            }
        }
        Ok(())
    }

    /// The payload-independent fire gates: a firing already queued, a
    /// spent `once`, an open cooldown.
    fn gated(&self, idx: usize, trigger: &Trigger) -> bool {
        if self.queued.get(idx).is_some_and(Option::is_some) {
            return true;
        }
        if trigger.once && self.blackboard.trigger_fired_once(&trigger.id) {
            debug!(trigger = %trigger.id, "trigger already fired this session (`once`)");
            return true;
        }
        if let Some(cooldown) = trigger.cooldown {
            if let Some(last) = self.blackboard.trigger_last_fired(&trigger.id) {
                // Wall-clock on purpose: cooldowns survive resume. A
                // negative elapsed (backwards clock step) extends the
                // cooldown rather than firing early, as does a cooldown
                // too large for chrono to represent.
                let since = self.clock.now().signed_duration_since(last);
                let cooldown =
                    chrono::Duration::from_std(cooldown).unwrap_or(chrono::Duration::MAX);
                if since < cooldown {
                    debug!(trigger = %trigger.id, "trigger inside its cooldown");
                    return true;
                }
            }
        }
        false
    }

    /// Evaluate a `when`/`while` gate with `event.*` bound to the
    /// firing's payload. Absent = passes. An evaluation error or
    /// non-boolean value is a workflow error attributed to the trigger —
    /// a gate that cannot be evaluated is an authoring bug, and silently
    /// never-firing would hide it (design § Triggers pins).
    fn gate_passes(
        &self,
        trigger: &Trigger,
        gate: Option<&Expression>,
        payload: &Value,
        role: &str,
    ) -> Result<bool, Interrupt> {
        let Some(gate) = gate else {
            return Ok(true);
        };
        let ctx = EvalContext {
            event: Some(payload),
            ..self.ctx()
        };
        let message = match gate.eval(&ctx) {
            Ok(Value::Bool(b)) => return Ok(b),
            Ok(other) => format!(
                "trigger `{}`: {role} gate `{}` must yield a boolean, got {other}",
                trigger.id,
                gate.source()
            ),
            Err(e) => format!(
                "trigger `{}`: {role} gate `{}` failed: {e}",
                trigger.id,
                gate.source()
            ),
        };
        Err(Interrupt::Error(WorkflowError {
            message,
            instruction_id: None,
            tool: None,
        }))
    }

    /// Call every due poll source and queue the firings its result
    /// gates through. Each handled due reschedules the next cycle at
    /// now + interval (missed cycles collapse). A gated trigger skips
    /// the tool call entirely; a failed call — argument evaluation
    /// included — skips the cycle at `debug!` (a flaky poll must not
    /// kill the session).
    async fn run_due_polls(&mut self) -> ExecResult {
        let triggers = self.triggers;
        for (idx, trigger) in triggers.iter().enumerate() {
            let TriggerSource::Poll {
                tool,
                args,
                interval,
            } = &trigger.on
            else {
                continue;
            };
            if self
                .poll_due
                .get(idx)
                .is_some_and(|due| due.is_some_and(|due| self.clock.monotonic() < due))
            {
                continue;
            }
            // Validation caps `interval` at 24h; a `None` (monotonic
            // clock within a day of its edge) is defense in depth — see
            // the construction of `poll_due`.
            let next_due = self.clock.monotonic().checked_add(*interval);
            if let Some(slot) = self.poll_due.get_mut(idx) {
                *slot = next_due;
            } else {
                // One slot per trigger by construction; a miss is an
                // internal invariant break — loud, but no panic.
                warn!(trigger = %trigger.id, idx, "poll schedule out of sync; next due not recorded");
            }
            if self.gated(idx, trigger) {
                continue;
            }
            let Some(call_args) = self.poll_args(trigger, args) else {
                continue;
            };
            debug!(trigger = %trigger.id, tool = %tool, "poll trigger due; calling its tool");
            match self.tools.call(tool, call_args).await {
                Ok(payload) => {
                    if let Some(synthetic) = correction_event(&payload) {
                        // Evaluated at the next safe point, like a
                        // correction on any other tool call.
                        self.pending.push_back(synthetic);
                    }
                    if self.gate_passes(trigger, trigger.when.as_ref(), &payload, "`when`")? {
                        debug!(trigger = %trigger.id, "poll trigger firing queued");
                        if let Some(slot) = self.queued.get_mut(idx) {
                            *slot = Some(payload);
                        } else {
                            // One slot per trigger by construction; a
                            // miss is an internal invariant break —
                            // loud, but no panic.
                            warn!(trigger = %trigger.id, idx, "trigger bookkeeping out of sync; dropping the firing");
                        }
                    }
                }
                Err(ToolCallError::SessionTerminated(message)) => {
                    debug!(trigger = %trigger.id, %message,
                           "MCP session terminated during a poll call");
                    return Err(Interrupt::Terminated);
                }
                Err(ToolCallError::Failed(message)) => {
                    debug!(trigger = %trigger.id, tool = %tool, %message,
                           "poll tool call failed; skipping this cycle");
                }
            }
        }
        Ok(())
    }

    /// Evaluate a poll's arguments (params/session/result in scope, per
    /// validation); `None` skips the cycle — poll args commonly read
    /// session state a later phase sets, and a poll must not kill the
    /// session before that phase arrives.
    fn poll_args(
        &self,
        trigger: &Trigger,
        args: &BTreeMap<String, ArgValue>,
    ) -> Option<Map<String, Value>> {
        let mut call_args = Map::new();
        for (name, arg) in args {
            let value = match arg {
                ArgValue::Literal(v) => v.clone(),
                ArgValue::Expr(expr) => match expr.eval(&self.ctx()) {
                    Ok(v) => integral_arg(v),
                    Err(e) => {
                        debug!(trigger = %trigger.id, argument = %name, error = %e,
                               "poll argument failed to evaluate; skipping this cycle");
                        return None;
                    }
                },
            };
            call_args.insert(name.clone(), value);
        }
        Some(call_args)
    }

    /// Run queued trigger actions in document order. The `while` gate is
    /// re-checked at fire time (an earlier trigger's action in the same
    /// batch can retract a later one); bookkeeping is recorded on
    /// successful completion only, mirroring instruction `once`
    /// semantics; an uncaught error fails the session, its message
    /// prefixed with the trigger id.
    async fn run_queued(&mut self) -> ExecResult {
        let triggers = self.triggers;
        for (idx, trigger) in triggers.iter().enumerate() {
            let Some(slot) = self.queued.get_mut(idx) else {
                // One slot per trigger by construction; a miss is an
                // internal invariant break — loud, but no panic.
                warn!(trigger = %trigger.id, idx, "trigger bookkeeping out of sync; cannot run its firing");
                continue;
            };
            let Some(payload) = slot.take() else {
                continue;
            };
            if !self.gate_passes(trigger, trigger.while_gate.as_ref(), &payload, "`while`")? {
                debug!(trigger = %trigger.id,
                       "`while` gate false at fire time; dropping the queued firing");
                continue;
            }
            debug!(trigger = %trigger.id, "trigger fired; running its `do` block");
            // A `do` block starts with `result` and `error.*` null and
            // sees the firing's payload as `event.*`; all three are
            // restored when the action ends (§ `result` scoping).
            let saved_result = std::mem::replace(&mut self.result, Value::Null);
            let saved_error = self.error.take();
            let saved_event = self.event.replace(payload);
            self.in_trigger_action = true;
            // Boxed: this re-entry into block execution would otherwise
            // make the safe-point future's type infinitely recursive.
            let outcome = Box::pin(self.exec_block(&trigger.actions)).await;
            self.in_trigger_action = false;
            self.event = saved_event;
            self.error = saved_error;
            self.result = saved_result;
            match outcome {
                Ok(()) => {}
                Err(Interrupt::Error(error)) => {
                    return Err(Interrupt::Error(WorkflowError {
                        message: format!("trigger `{}`: {}", trigger.id, error.message),
                        ..error
                    }));
                }
                Err(Interrupt::Terminated) => return Err(Interrupt::Terminated),
            }
            self.blackboard
                .mark_trigger_fired(&trigger.id, self.clock.now(), trigger.once)
                .await
                .map_err(|e| {
                    Interrupt::Error(WorkflowError {
                        message: format!("trigger `{}`: {e}", trigger.id),
                        instruction_id: None,
                        tool: None,
                    })
                })?;
        }
        Ok(())
    }

    fn exec_log(&self, ins: &'a Instruction, log: &'a Log) -> ExecResult {
        let mut rendered = Map::new();
        for (key, expr) in &log.values {
            let role = format!("`log` value `{key}`");
            rendered.insert(key.clone(), self.eval(ins, expr, &role)?);
        }
        let values = Value::Object(rendered);
        match log.level {
            LogLevel::Debug => {
                debug!(id = ins.id.as_deref(), values = %values, "{}", log.message);
            }
            LogLevel::Info => {
                info!(id = ins.id.as_deref(), values = %values, "{}", log.message);
            }
        }
        Ok(())
    }
}

/// The synthetic `correction_requested` event for a tool result that
/// carries a correction (design § Triggers pins; `rp.md` § Corrections):
/// `status: "aborted"` or `"blocked_by_correction"` with a `correction`
/// object → `event.delivery = "immediate"`; a `pending_correction`
/// object → `"after_current"`. A correction value that is not a JSON
/// object is logged and ignored.
fn correction_event(result: &Value) -> Option<EngineEvent> {
    let status = result.get("status").and_then(Value::as_str);
    let (correction, delivery) = if matches!(status, Some("aborted" | "blocked_by_correction")) {
        (result.get("correction")?, "immediate")
    } else {
        (result.get("pending_correction")?, "after_current")
    };
    let Value::Object(fields) = correction else {
        debug!(%correction, "ignoring a correction that is not a JSON object");
        return None;
    };
    let mut payload = fields.clone();
    payload.insert("delivery".to_owned(), Value::String(delivery.to_owned()));
    Some(EngineEvent {
        event: "correction_requested".to_owned(),
        payload: Value::Object(payload),
    })
}

/// An `$expr` tool argument that evaluates to a whole number within
/// f64's exact-integer range is serialized as a JSON integer —
/// expression arithmetic always produces f64, while tool parameters
/// are commonly integer-typed (a panel brightness, a camera gain), and
/// a `127.0` would fail their deserialization where `127` succeeds.
fn integral_arg(value: Value) -> Value {
    let Value::Number(ref n) = value else {
        return value;
    };
    if n.is_i64() || n.is_u64() {
        return value;
    }
    match n.as_f64().and_then(ExactInt::try_from_f64) {
        Some(exact) => Value::Number(serde_json::Number::from(exact.as_i64())),
        None => value,
    }
}

/// A `Value` as a `u64` loop bound: any JSON number whose value is a
/// non-negative integer within f64's exact-integer range.
fn integer_value(value: &Value) -> Option<u64> {
    ExactInt::try_from_f64(value.as_f64()?)?.to_u64()
}
