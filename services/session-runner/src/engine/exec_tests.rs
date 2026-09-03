//! Engine semantics tests — the design doc's § Testing Strategy battery,
//! driven against a scripted [`ToolClient`] and a mock [`Clock`]:
//! sequencing, `result` scoping, `set` ordering and persistence,
//! `try`/`catch`/`finally` paths (including finally-does-not-mask),
//! `retry`, loop bounds and `result.converged`, `once` bookkeeping,
//! waits, and the terminated-session (safety) path.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use serde_json::{json, Map, Value};

use super::{run, Clock, EngineEvent, EventIntake, RunOutcome, ToolCallError, ToolClient};
use crate::blackboard::Blackboard;
use crate::document::{bind_parameters, Document};

// --- test doubles ---------------------------------------------------------

type Responder =
    Box<dyn Fn(usize, &str, &Map<String, Value>) -> Result<Value, ToolCallError> + Send + Sync>;

/// A `ToolClient` driven by a closure; records every call in order.
struct MockTools {
    responder: Responder,
    calls: Mutex<Vec<(String, Value)>>,
}

impl MockTools {
    fn new(
        responder: impl Fn(usize, &str, &Map<String, Value>) -> Result<Value, ToolCallError>
            + Send
            + Sync
            + 'static,
    ) -> Self {
        Self {
            responder: Box::new(responder),
            calls: Mutex::new(Vec::new()),
        }
    }

    /// Every call succeeds with `result`.
    fn ok(result: Value) -> Self {
        Self::new(move |_, _, _| Ok(result.clone()))
    }

    /// Responses consumed in call order; panics when exhausted.
    fn scripted(results: Vec<Result<Value, ToolCallError>>) -> Self {
        let queue = Mutex::new(VecDeque::from(results));
        Self::new(move |_, tool, _| {
            queue
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| panic!("unexpected tool call `{tool}`: script exhausted"))
        })
    }

    /// Panics on any call.
    fn none() -> Self {
        Self::new(|_, tool, _| panic!("unexpected tool call `{tool}`"))
    }

    fn calls(&self) -> Vec<(String, Value)> {
        self.calls.lock().unwrap().clone()
    }

    fn call_names(&self) -> Vec<String> {
        self.calls().into_iter().map(|(name, _)| name).collect()
    }
}

impl ToolClient for MockTools {
    async fn call(&self, tool: &str, args: Map<String, Value>) -> Result<Value, ToolCallError> {
        let index = {
            let mut calls = self.calls.lock().unwrap();
            calls.push((tool.to_owned(), Value::Object(args.clone())));
            calls.len() - 1
        };
        (self.responder)(index, tool, &args)
    }
}

/// A deterministic clock: `sleep` advances `now` instantly and records the
/// requested duration.
struct MockClock {
    now: Mutex<DateTime<Utc>>,
    sleeps: Mutex<Vec<Duration>>,
}

impl MockClock {
    fn new() -> Self {
        Self {
            now: Mutex::new(Utc.with_ymd_and_hms(2026, 7, 1, 22, 0, 0).unwrap()),
            sleeps: Mutex::new(Vec::new()),
        }
    }

    fn sleeps(&self) -> Vec<Duration> {
        self.sleeps.lock().unwrap().clone()
    }
}

impl Clock for MockClock {
    fn now(&self) -> DateTime<Utc> {
        *self.now.lock().unwrap()
    }

    async fn sleep(&self, duration: Duration) {
        let delta = chrono::Duration::from_std(duration).unwrap();
        {
            let mut now = self.now.lock().unwrap();
            *now += delta;
        }
        self.sleeps.lock().unwrap().push(duration);
    }

    fn monotonic(&self) -> Duration {
        // Mock time passes only while sleeping.
        self.sleeps.lock().unwrap().iter().sum()
    }
}

// --- harness --------------------------------------------------------------

fn make_doc(document: Value) -> Document {
    Document::from_value(&document)
        .unwrap_or_else(|issues| panic!("test document invalid: {issues:#?}"))
}

fn doc_with_root(root: Value) -> Document {
    make_doc(json!({ "version": 1, "name": "test", "root": root }))
}

/// Run `doc` against a blackboard file in `dir` (loaded, so repeated runs
/// share persisted state) and return the outcome plus the final session
/// value. No event stream — see [`run_with_events`] for that.
async fn run_in(
    dir: &tempfile::TempDir,
    doc: &Document,
    params: &Value,
    tools: &MockTools,
    clock: &(impl Clock + Sync),
) -> (RunOutcome, Value) {
    let mut blackboard = Blackboard::load(dir.path().join("session.json"))
        .await
        .unwrap();
    let outcome = run(
        doc,
        params,
        &mut blackboard,
        tools,
        clock,
        EventIntake::disconnected(),
    )
    .await;
    let session = blackboard.value().clone();
    (outcome, session)
}

/// One-shot run of a parameterless document with a live event intake fed
/// by the returned sender's prior sends (buffer the events, then run —
/// the intake is live from run start, so pre-buffered events model events
/// emitted while earlier instructions ran).
async fn run_with_events(
    root: Value,
    tools: &MockTools,
    clock: &(impl Clock + Sync),
    events: EventIntake,
) -> RunOutcome {
    let dir = tempfile::tempdir().unwrap();
    let doc = doc_with_root(root);
    let mut blackboard = Blackboard::load(dir.path().join("session.json"))
        .await
        .unwrap();
    run(&doc, &json!({}), &mut blackboard, tools, clock, events).await
}

/// One-shot run of a parameterless document.
async fn run_root(root: Value, tools: &MockTools) -> (RunOutcome, Value) {
    let dir = tempfile::tempdir().unwrap();
    let doc = doc_with_root(root);
    run_in(&dir, &doc, &json!({}), tools, &MockClock::new()).await
}

#[track_caller]
fn failure(outcome: RunOutcome) -> super::WorkflowError {
    match outcome {
        RunOutcome::Failed(error) => error,
        other => panic!("expected RunOutcome::Failed, got {other:?}"),
    }
}

// --- sequencing and tool calls --------------------------------------------

#[tokio::test]
async fn test_sequence_calls_tools_in_order_with_literal_and_expr_args() {
    let tools = MockTools::ok(json!({}));
    let doc = make_doc(json!({
        "version": 1, "name": "t",
        "parameters": { "cam": { "type": "string", "required": true } },
        "root": { "sequence": [
            { "tool": "a", "args": { "x": 1, "s": "literal" } },
            { "tool": "b", "args": { "camera_id": { "$expr": "params.cam" } } },
            { "tool": "c" }
        ] }
    }));
    let params = bind_parameters(&doc.parameters, Some(&json!({"cam": "main"}))).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (outcome, _) = run_in(&dir, &doc, &params, &tools, &MockClock::new()).await;

    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(
        tools.calls(),
        vec![
            ("a".to_owned(), json!({"x": 1, "s": "literal"})),
            ("b".to_owned(), json!({"camera_id": "main"})),
            ("c".to_owned(), json!({})),
        ]
    );
}

#[tokio::test]
async fn test_result_scoping_across_instruction_kinds() {
    // `tool` produces a result; `set`, `log`, and `wait` leave it
    // unchanged; an `if` reads it without consuming it; after a container
    // the last inner result is still in scope.
    let tools = MockTools::new(|index, _, _| Ok(json!({ "v": index + 1 })));
    let root = json!({ "sequence": [
        { "tool": "a" },
        { "set": { "session.first": "result.v" } },
        { "log": { "message": "still scoped" } },
        { "wait": { "duration": "1s" } },
        { "set": { "session.after_noops": "result.v" } },
        { "tool": "b" },
        { "if": "result.v == 2",
          "then": [ { "set": { "session.in_branch": "result.v" } } ] },
        { "set": { "session.after_if": "result.v" } }
    ] });
    let (outcome, session) = run_root(root, &tools).await;

    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(session["first"], json!(1));
    assert_eq!(session["after_noops"], json!(1));
    assert_eq!(session["in_branch"], json!(2));
    assert_eq!(session["after_if"], json!(2));
}

#[tokio::test]
async fn test_result_is_null_at_session_start() {
    let tools = MockTools::none();
    let root = json!({ "set": { "session.initial": "result == null" } });
    let (outcome, session) = run_root(root, &tools).await;
    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(session["initial"], json!(true));
}

// --- set ------------------------------------------------------------------

#[tokio::test]
async fn test_set_evaluates_all_values_before_writing_any_key() {
    let tools = MockTools::none();
    let root = json!({ "sequence": [
        { "set": { "session.a": "1", "session.b": "2" } },
        // A swap only works if both reads happen before either write.
        { "set": { "session.a": "session.b", "session.b": "session.a" } }
    ] });
    let (outcome, session) = run_root(root, &tools).await;
    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(session["a"], json!(2.0));
    assert_eq!(session["b"], json!(1.0));
}

#[tokio::test]
async fn test_set_persists_before_the_next_instruction_runs() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.json");
    let probe_path = path.clone();
    // The tool responder reads the blackboard file from disk: whatever the
    // preceding `set` wrote must already be persisted.
    let tools = MockTools::new(move |_, _, _| {
        let bytes = std::fs::read(&probe_path).unwrap();
        Ok(json!({ "on_disk": serde_json::from_slice::<Value>(&bytes).unwrap() }))
    });
    let doc = doc_with_root(json!({ "sequence": [
        { "set": { "session.x": "42" } },
        { "tool": "probe" },
        { "set": { "session.probed": "result.on_disk.x" } }
    ] }));
    let (outcome, session) = run_in(&dir, &doc, &json!({}), &tools, &MockClock::new()).await;

    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(session["probed"], json!(42.0));
}

#[tokio::test]
async fn test_set_through_non_object_intermediate_fails_loud() {
    let tools = MockTools::none();
    let root = json!({ "sequence": [
        { "set": { "session.a": "1" } },
        { "id": "bad-write", "set": { "session.a.b": "2" } }
    ] });
    let (outcome, _) = run_root(root, &tools).await;
    let error = failure(outcome);
    assert_eq!(
        error.message,
        "cannot set `session.a.b`: `session.a` is not an object"
    );
    assert_eq!(error.instruction_id.as_deref(), Some("bad-write"));
}

#[tokio::test]
async fn test_set_value_evaluation_error_is_a_workflow_error() {
    let tools = MockTools::none();
    let root = json!({ "id": "s1", "set": { "session.x": "session.missing + 1" } });
    let (outcome, _) = run_root(root, &tools).await;
    let error = failure(outcome);
    assert!(
        error
            .message
            .starts_with("`set` value for `session.x` `session.missing + 1` failed:"),
        "{}",
        error.message
    );
    assert!(error.message.contains("(at "), "{}", error.message);
    assert_eq!(error.instruction_id.as_deref(), Some("s1"));
    assert_eq!(error.tool, None);
}

// --- try / catch / finally -------------------------------------------------

#[tokio::test]
async fn test_try_success_skips_catch_and_runs_finally() {
    let tools = MockTools::ok(json!({}));
    let root = json!({
        "try": [ { "tool": "work" } ],
        "catch": [ { "tool": "never" } ],
        "finally": [ { "tool": "cleanup" } ]
    });
    let (outcome, _) = run_root(root, &tools).await;
    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(tools.call_names(), vec!["work", "cleanup"]);
}

#[tokio::test]
async fn test_catch_sees_error_namespace_and_handles_the_error() {
    let tools = MockTools::new(|_, tool, _| match tool {
        "explode" => Err(ToolCallError::Failed("kaput".to_owned())),
        _ => Ok(json!({})),
    });
    let root = json!({
        "try": [ { "id": "boom", "tool": "explode" } ],
        "catch": [ { "set": {
            "session.msg": "error.message",
            "session.iid": "error.instruction_id",
            "session.tool": "error.tool"
        } } ],
        "finally": [ { "tool": "cleanup" } ]
    });
    let (outcome, session) = run_root(root, &tools).await;

    assert_eq!(outcome, RunOutcome::Completed, "catch handles the error");
    assert_eq!(session["msg"], json!("tool `explode` failed: kaput"));
    assert_eq!(session["iid"], json!("boom"));
    assert_eq!(session["tool"], json!("explode"));
    assert_eq!(tools.call_names(), vec!["explode", "cleanup"]);
}

#[tokio::test]
async fn test_error_namespace_for_a_fail_raised_error_has_null_tool() {
    let tools = MockTools::none();
    let root = json!({
        "try": [ { "fail": { "message": "'deliberate'" } } ],
        "catch": [ { "set": {
            "session.tool_is_null": "error.tool == null",
            "session.iid_is_null": "error.instruction_id == null"
        } } ]
    });
    let (outcome, session) = run_root(root, &tools).await;
    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(session["tool_is_null"], json!(true));
    assert_eq!(session["iid_is_null"], json!(true));
}

#[tokio::test]
async fn test_catch_reraise_via_fail_propagates_and_finally_still_runs() {
    let tools = MockTools::new(|_, tool, _| match tool {
        "explode" => Err(ToolCallError::Failed("kaput".to_owned())),
        _ => Ok(json!({})),
    });
    let root = json!({
        "try": [ { "tool": "explode" } ],
        "catch": [ { "fail": { "message": "error.message" } } ],
        "finally": [ { "tool": "cleanup" } ]
    });
    let (outcome, _) = run_root(root, &tools).await;
    let error = failure(outcome);
    assert_eq!(error.message, "tool `explode` failed: kaput");
    assert_eq!(tools.call_names(), vec!["explode", "cleanup"]);
}

#[tokio::test]
async fn test_finally_failure_on_the_success_path_is_a_workflow_error() {
    let tools = MockTools::new(|_, tool, _| match tool {
        "cleanup" => Err(ToolCallError::Failed("jammed".to_owned())),
        _ => Ok(json!({})),
    });
    let root = json!({
        "try": [ { "tool": "work" } ],
        "finally": [ { "tool": "cleanup" } ]
    });
    let (outcome, _) = run_root(root, &tools).await;
    assert_eq!(failure(outcome).message, "tool `cleanup` failed: jammed");
}

#[tokio::test]
async fn test_finally_failure_never_masks_the_original_error() {
    let tools = MockTools::new(|_, tool, _| Err(ToolCallError::Failed(format!("{tool} broke"))));
    let root = json!({
        "try": [ { "tool": "work" } ],
        "finally": [ { "tool": "cleanup" } ]
    });
    let (outcome, _) = run_root(root, &tools).await;
    assert_eq!(failure(outcome).message, "tool `work` failed: work broke");
    assert_eq!(tools.call_names(), vec!["work", "cleanup"]);
}

#[tokio::test]
async fn test_nested_try_restores_the_enclosing_error_scope() {
    let tools = MockTools::new(|_, tool, _| match tool {
        "outer_boom" => Err(ToolCallError::Failed("outer".to_owned())),
        "inner_boom" => Err(ToolCallError::Failed("inner".to_owned())),
        _ => Ok(json!({})),
    });
    let root = json!({
        "try": [ { "tool": "outer_boom" } ],
        "catch": [
            { "try": [ { "tool": "inner_boom" } ],
              "catch": [ { "set": { "session.inner": "error.message" } } ] },
            { "set": { "session.outer": "error.message" } }
        ]
    });
    let (outcome, session) = run_root(root, &tools).await;
    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(session["inner"], json!("tool `inner_boom` failed: inner"));
    assert_eq!(session["outer"], json!("tool `outer_boom` failed: outer"));
}

#[tokio::test]
async fn test_success_path_finally_keeps_the_enclosing_error_scope_visible() {
    let tools = MockTools::new(|_, tool, _| match tool {
        "explode" => Err(ToolCallError::Failed("kaput".to_owned())),
        _ => Ok(json!({})),
    });
    // The inner try succeeds, so its `finally` runs on the success path —
    // the outer catch's `error.*` (the error still being handled) stays
    // visible rather than being nulled out.
    let root = json!({
        "try": [ { "tool": "explode" } ],
        "catch": [
            { "try": [ { "log": { "message": "recovering" } } ],
              "finally": [ { "set": { "session.seen": "error.message" } } ] }
        ]
    });
    let (outcome, session) = run_root(root, &tools).await;
    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(session["seen"], json!("tool `explode` failed: kaput"));
}

#[tokio::test]
async fn test_error_namespace_is_null_in_a_top_level_success_finally() {
    let tools = MockTools::none();
    let root = json!({
        "try": [ { "set": { "session.x": "1" } } ],
        "finally": [ { "set": { "session.has_error": "has(error.message)" } } ]
    });
    let (outcome, session) = run_root(root, &tools).await;
    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(session["has_error"], json!(false));
}

// --- retry -------------------------------------------------------------------

#[tokio::test]
async fn test_retry_retries_with_backoff_until_success() {
    let tools = MockTools::scripted(vec![
        Err(ToolCallError::Failed("flake 1".to_owned())),
        Err(ToolCallError::Failed("flake 2".to_owned())),
        Ok(json!({"ok": true})),
    ]);
    let clock = MockClock::new();
    let dir = tempfile::tempdir().unwrap();
    let doc = doc_with_root(json!({ "sequence": [
        { "tool": "flaky", "retry": { "max_attempts": 3, "backoff": "10s" } },
        { "set": { "session.ok": "result.ok" } }
    ] }));
    let (outcome, session) = run_in(&dir, &doc, &json!({}), &tools, &clock).await;

    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(session["ok"], json!(true));
    assert_eq!(tools.call_names(), vec!["flaky", "flaky", "flaky"]);
    assert_eq!(
        clock.sleeps(),
        vec![Duration::from_secs(10), Duration::from_secs(10)]
    );
}

#[tokio::test]
async fn test_retry_exhaustion_raises_with_the_attempt_count() {
    let tools = MockTools::new(|_, _, _| Err(ToolCallError::Failed("nope".to_owned())));
    let root = json!({ "id": "t1",
        "tool": "flaky", "retry": { "max_attempts": 2, "backoff": "1s" } });
    let (outcome, _) = run_root(root, &tools).await;
    let error = failure(outcome);
    assert_eq!(error.message, "tool `flaky` failed after 2 attempts: nope");
    assert_eq!(error.instruction_id.as_deref(), Some("t1"));
    assert_eq!(error.tool.as_deref(), Some("flaky"));
    assert_eq!(tools.calls().len(), 2);
}

#[tokio::test]
async fn test_tool_failure_without_retry_names_no_attempt_count() {
    let tools = MockTools::new(|_, _, _| Err(ToolCallError::Failed("nope".to_owned())));
    let (outcome, _) = run_root(json!({ "tool": "x" }), &tools).await;
    assert_eq!(failure(outcome).message, "tool `x` failed: nope");
}

#[tokio::test]
async fn test_session_termination_is_never_retried() {
    let tools =
        MockTools::new(|_, _, _| Err(ToolCallError::SessionTerminated("safety".to_owned())));
    let clock = MockClock::new();
    let dir = tempfile::tempdir().unwrap();
    let doc = doc_with_root(
        json!({ "tool": "capture", "retry": { "max_attempts": 5, "backoff": "10s" } }),
    );
    let (outcome, _) = run_in(&dir, &doc, &json!({}), &tools, &clock).await;

    assert_eq!(outcome, RunOutcome::Terminated);
    assert_eq!(tools.calls().len(), 1);
    assert_eq!(clock.sleeps(), Vec::<Duration>::new());
}

// --- repeat ------------------------------------------------------------------

#[tokio::test]
async fn test_repeat_until_converges_and_reports_the_loop_summary() {
    let tools = MockTools::new(|index, _, _| Ok(json!({ "n": index + 1 })));
    let root = json!({ "sequence": [
        { "repeat": { "until": "result.n >= 3", "max_iterations": 10 },
          "body": [ { "tool": "step" } ] },
        { "set": { "session.iterations": "result.iterations",
                   "session.converged": "result.converged" } }
    ] });
    let (outcome, session) = run_root(root, &tools).await;
    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(tools.calls().len(), 3);
    assert_eq!(session["iterations"], json!(3));
    assert_eq!(session["converged"], json!(true));
}

#[tokio::test]
async fn test_repeat_until_exhaustion_completes_with_converged_false() {
    let tools = MockTools::ok(json!({ "n": 0 }));
    let root = json!({ "sequence": [
        { "repeat": { "until": "result.n >= 99", "max_iterations": 2 },
          "body": [ { "tool": "step" } ] },
        { "set": { "session.iterations": "result.iterations",
                   "session.converged": "result.converged" } }
    ] });
    let (outcome, session) = run_root(root, &tools).await;
    // Exhaustion is not an error — the loop completes and the document
    // decides what `converged == false` means.
    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(tools.calls().len(), 2);
    assert_eq!(session["iterations"], json!(2));
    assert_eq!(session["converged"], json!(false));
}

#[tokio::test]
async fn test_repeat_while_runs_while_the_condition_holds() {
    // The second `work` call says stop; the body's `if` flips the gate.
    let tools = MockTools::new(|index, _, _| Ok(json!({ "stop": index == 1 })));
    let root = json!({ "sequence": [
        { "set": { "session.go": "true" } },
        { "repeat": { "while": "session.go == true", "max_iterations": 5 },
          "body": [
            { "tool": "work" },
            { "if": "result.stop == true",
              "then": [ { "set": { "session.go": "false" } } ] }
          ] },
        { "set": { "session.iterations": "result.iterations",
                   "session.converged": "result.converged" } }
    ] });
    let (outcome, session) = run_root(root, &tools).await;
    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(tools.calls().len(), 2);
    assert_eq!(session["iterations"], json!(2));
    assert_eq!(session["converged"], json!(true));
}

#[tokio::test]
async fn test_repeat_while_false_at_entry_runs_zero_passes() {
    let tools = MockTools::none();
    let root = json!({ "sequence": [
        { "repeat": { "while": "session.go == true", "max_iterations": 5 },
          "body": [ { "tool": "work" } ] },
        { "set": { "session.iterations": "result.iterations",
                   "session.converged": "result.converged" } }
    ] });
    let (outcome, session) = run_root(root, &tools).await;
    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(session["iterations"], json!(0));
    assert_eq!(session["converged"], json!(true));
}

#[tokio::test]
async fn test_repeat_while_exhaustion_with_the_condition_still_true() {
    let tools = MockTools::ok(json!({}));
    let root = json!({ "sequence": [
        { "repeat": { "while": "true", "max_iterations": 2 },
          "body": [ { "tool": "work" } ] },
        { "set": { "session.iterations": "result.iterations",
                   "session.converged": "result.converged" } }
    ] });
    let (outcome, session) = run_root(root, &tools).await;
    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(tools.calls().len(), 2);
    assert_eq!(session["iterations"], json!(2));
    assert_eq!(session["converged"], json!(false));
}

#[tokio::test]
async fn test_repeat_while_condition_turning_false_at_the_budget_still_converges() {
    // The gate flips after the second (final permitted) pass: the engine
    // evaluates the condition once more before declaring exhaustion, so
    // this loop converged.
    let tools = MockTools::new(|index, _, _| Ok(json!({ "stop": index == 1 })));
    let root = json!({ "sequence": [
        { "set": { "session.go": "true" } },
        { "repeat": { "while": "session.go == true", "max_iterations": 2 },
          "body": [
            { "tool": "work" },
            { "if": "result.stop == true",
              "then": [ { "set": { "session.go": "false" } } ] }
          ] },
        { "set": { "session.converged": "result.converged" } }
    ] });
    let (outcome, session) = run_root(root, &tools).await;
    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(session["converged"], json!(true));
}

#[tokio::test]
async fn test_repeat_count_runs_exactly_count_passes() {
    let tools = MockTools::ok(json!({}));
    let root = json!({ "sequence": [
        { "repeat": { "count": 4 }, "body": [ { "tool": "capture" } ] },
        { "set": { "session.iterations": "result.iterations",
                   "session.has_converged": "has(result.converged)" } }
    ] });
    let (outcome, session) = run_root(root, &tools).await;
    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(tools.calls().len(), 4);
    assert_eq!(session["iterations"], json!(4));
    // Count loops have no condition, so no `converged` in the summary.
    assert_eq!(session["has_converged"], json!(false));
}

#[tokio::test]
async fn test_repeat_count_zero_runs_no_passes() {
    let tools = MockTools::none();
    let root = json!({ "sequence": [
        { "repeat": { "count": 0 }, "body": [ { "tool": "capture" } ] },
        { "set": { "session.iterations": "result.iterations" } }
    ] });
    let (outcome, session) = run_root(root, &tools).await;
    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(session["iterations"], json!(0));
}

#[tokio::test]
async fn test_repeat_count_from_a_parameter_expression() {
    let tools = MockTools::ok(json!({}));
    let doc = make_doc(json!({
        "version": 1, "name": "t",
        "parameters": { "count": { "type": "integer", "required": true } },
        "root": { "repeat": { "count": { "$expr": "params.count" } },
                  "body": [ { "tool": "capture" } ] }
    }));
    let params = bind_parameters(&doc.parameters, Some(&json!({"count": 3}))).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (outcome, _) = run_in(&dir, &doc, &params, &tools, &MockClock::new()).await;
    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(tools.calls().len(), 3);
}

#[tokio::test]
async fn test_repeat_count_exceeding_its_max_iterations_guard_fails_at_entry() {
    let tools = MockTools::none();
    let root = json!({ "id": "guarded",
        "repeat": { "count": { "$expr": "2 + 3" }, "max_iterations": 2 },
        "body": [ { "tool": "capture" } ] });
    let (outcome, _) = run_root(root, &tools).await;
    let error = failure(outcome);
    assert_eq!(error.message, "`count` (5) exceeds `max_iterations` (2)");
    assert_eq!(error.instruction_id.as_deref(), Some("guarded"));
    assert!(tools.calls().is_empty(), "no pass may run");
}

#[tokio::test]
async fn test_repeat_bound_expressions_must_yield_usable_integers() {
    let cases = [
        (
            json!({ "repeat": { "until": "false", "max_iterations": { "$expr": "0 - 1" } },
                    "body": [ { "log": { "message": "x" } } ] }),
            "`max_iterations` `0 - 1` must yield a non-negative integer, got -1.0",
        ),
        (
            json!({ "repeat": { "until": "false", "max_iterations": { "$expr": "2.5" } },
                    "body": [ { "log": { "message": "x" } } ] }),
            "`max_iterations` `2.5` must yield a non-negative integer, got 2.5",
        ),
        (
            json!({ "repeat": { "until": "false", "max_iterations": { "$expr": "0" } },
                    "body": [ { "log": { "message": "x" } } ] }),
            "`max_iterations` must be at least 1, got 0",
        ),
        (
            json!({ "repeat": { "count": { "$expr": "'three'" } },
                    "body": [ { "log": { "message": "x" } } ] }),
            "`count` `'three'` must yield a non-negative integer, got \"three\"",
        ),
    ];
    for (root, expected) in cases {
        let tools = MockTools::none();
        let (outcome, _) = run_root(root, &tools).await;
        assert_eq!(failure(outcome).message, expected);
    }
}

#[tokio::test]
async fn test_repeat_body_error_propagates_out_of_the_loop() {
    let tools = MockTools::new(|index, _, _| {
        if index == 1 {
            Err(ToolCallError::Failed("pass 2 broke".to_owned()))
        } else {
            Ok(json!({}))
        }
    });
    let root = json!({ "repeat": { "count": 5 }, "body": [ { "tool": "step" } ] });
    let (outcome, _) = run_root(root, &tools).await;
    assert_eq!(failure(outcome).message, "tool `step` failed: pass 2 broke");
    assert_eq!(tools.calls().len(), 2);
}

// --- once markers ------------------------------------------------------------

#[tokio::test]
async fn test_once_skips_on_reexecution_and_leaves_result_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let doc = doc_with_root(json!({ "sequence": [
        { "tool": "arm", "once": "armed" },
        { "set": { "session.result_v": "result.v" } }
    ] }));
    let tools = MockTools::ok(json!({ "v": 1 }));

    let (outcome, session) = run_in(&dir, &doc, &json!({}), &tools, &MockClock::new()).await;
    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(session["result_v"], json!(1));
    assert_eq!(session["_once"], json!({ "armed": true }));
    assert_eq!(tools.calls().len(), 1);

    // Re-execution (the resume model): the marked instruction is skipped
    // and, having produced nothing, leaves `result` at its session-start
    // null — so the following `set` writes null.
    let (outcome, session) = run_in(&dir, &doc, &json!({}), &tools, &MockClock::new()).await;
    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(session["result_v"], Value::Null);
    assert_eq!(tools.calls().len(), 1, "the armed tool must not run again");
}

#[tokio::test]
async fn test_once_marker_is_not_recorded_when_the_instruction_fails() {
    let dir = tempfile::tempdir().unwrap();
    let doc = doc_with_root(json!({ "tool": "arm", "once": "armed" }));

    let failing = MockTools::new(|_, _, _| Err(ToolCallError::Failed("no".to_owned())));
    let (outcome, session) = run_in(&dir, &doc, &json!({}), &failing, &MockClock::new()).await;
    failure(outcome);
    assert_eq!(session.get("_once"), None);

    // The instruction re-runs on the next invocation and completes.
    let working = MockTools::ok(json!({}));
    let (outcome, session) = run_in(&dir, &doc, &json!({}), &working, &MockClock::new()).await;
    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(session["_once"], json!({ "armed": true }));
    assert_eq!(working.calls().len(), 1);
}

// --- wait --------------------------------------------------------------------

#[tokio::test]
async fn test_wait_duration_sleeps_once() {
    let tools = MockTools::none();
    let clock = MockClock::new();
    let dir = tempfile::tempdir().unwrap();
    let doc = doc_with_root(json!({ "wait": { "duration": "30s" } }));
    let (outcome, _) = run_in(&dir, &doc, &json!({}), &tools, &clock).await;
    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(clock.sleeps(), vec![Duration::from_secs(30)]);
}

#[tokio::test]
async fn test_wait_until_polls_until_the_condition_turns_true() {
    // The mock clock starts at 2026-07-01T22:00:00Z; the condition turns
    // true one minute later. Six 10 s polls get there.
    let tools = MockTools::none();
    let clock = MockClock::new();
    let dir = tempfile::tempdir().unwrap();
    let doc = doc_with_root(json!({ "wait": {
        "until": "seconds_until('2026-07-01T22:01:00Z') <= 0",
        "poll_interval": "10s",
        "timeout": "5m"
    } }));
    let (outcome, _) = run_in(&dir, &doc, &json!({}), &tools, &clock).await;
    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(clock.sleeps(), vec![Duration::from_secs(10); 6]);
}

#[tokio::test]
async fn test_wait_until_timeout_raises_after_a_final_check() {
    let tools = MockTools::none();
    let clock = MockClock::new();
    let dir = tempfile::tempdir().unwrap();
    let doc = doc_with_root(json!({ "id": "w1", "wait": {
        "until": "false",
        "poll_interval": "40s",
        "timeout": "1m 30s"
    } }));
    let (outcome, _) = run_in(&dir, &doc, &json!({}), &tools, &clock).await;
    let error = failure(outcome);
    assert_eq!(
        error.message,
        "`wait` `until` condition `false` did not become true within 1m 30s"
    );
    assert_eq!(error.instruction_id.as_deref(), Some("w1"));
    // The last sleep is clamped to the remaining budget, so the condition
    // gets a final evaluation exactly at the timeout.
    assert_eq!(
        clock.sleeps(),
        vec![
            Duration::from_secs(40),
            Duration::from_secs(40),
            Duration::from_secs(10)
        ]
    );
}

/// A clock whose wall time leaps an hour forward across every sleep — the
/// NTP-step scenario a wall-clock-based timeout would misread.
struct SteppingClock {
    now: Mutex<DateTime<Utc>>,
    sleeps: Mutex<Vec<Duration>>,
}

impl SteppingClock {
    fn new() -> Self {
        Self {
            now: Mutex::new(Utc.with_ymd_and_hms(2026, 7, 1, 22, 0, 0).unwrap()),
            sleeps: Mutex::new(Vec::new()),
        }
    }
}

impl Clock for SteppingClock {
    fn now(&self) -> DateTime<Utc> {
        *self.now.lock().unwrap()
    }

    async fn sleep(&self, duration: Duration) {
        let step = chrono::Duration::from_std(duration).unwrap() + chrono::Duration::hours(1);
        {
            let mut now = self.now.lock().unwrap();
            *now += step;
        }
        self.sleeps.lock().unwrap().push(duration);
    }

    fn monotonic(&self) -> Duration {
        // The wall clock steps; the monotonic reading, by contract,
        // advances only by the time actually slept.
        self.sleeps.lock().unwrap().iter().sum()
    }
}

#[tokio::test]
async fn test_wait_until_timeout_is_immune_to_wall_clock_steps() {
    // The timeout budget is accumulated sleep time, not wall-clock
    // differences — an NTP step (here +1 h per sleep) must neither fire
    // the timeout early nor extend the wait.
    let tools = MockTools::none();
    let clock = SteppingClock::new();
    let dir = tempfile::tempdir().unwrap();
    let doc = doc_with_root(json!({ "wait": {
        "until": "false",
        "poll_interval": "10s",
        "timeout": "30s"
    } }));
    let (outcome, _) = run_in(&dir, &doc, &json!({}), &tools, &clock).await;

    assert_eq!(
        failure(outcome).message,
        "`wait` `until` condition `false` did not become true within 30s"
    );
    // The full poll schedule ran despite `now()` racing three hours ahead.
    assert_eq!(
        clock.sleeps.lock().unwrap().clone(),
        vec![Duration::from_secs(10); 3]
    );
}

// --- wait: until_event ------------------------------------------------------

/// An intake pre-loaded with `events` (name, payload) — models events that
/// arrived (and were buffered) while earlier instructions ran.
fn buffered_events(events: &[(&str, Value)]) -> EventIntake {
    let (tx, rx) = tokio::sync::mpsc::channel(events.len().max(1));
    for (event, payload) in events {
        tx.try_send(EngineEvent {
            event: (*event).to_owned(),
            payload: payload.clone(),
        })
        .unwrap();
    }
    // The sender drops here: the stream ends after the buffered events,
    // exactly like an rp that emitted these and then went quiet.
    EventIntake::new(rx)
}

#[tokio::test]
async fn test_wait_until_event_is_satisfied_by_a_buffered_event() {
    // The intake runs from before the first instruction, so an event that
    // arrived during an earlier instruction still satisfies the wait — no
    // sleeping needed.
    let tools = MockTools::none();
    let clock = MockClock::new();
    let root = json!({ "wait": { "until_event": "guide_settled", "timeout": "5m" } });
    let events = buffered_events(&[("guide_settled", json!({ "rms": 0.4 }))]);
    let outcome = run_with_events(root, &tools, &clock, events).await;
    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(clock.sleeps(), Vec::<Duration>::new());
}

#[tokio::test]
async fn test_wait_until_event_drains_past_non_matching_events() {
    let tools = MockTools::none();
    let clock = MockClock::new();
    let root = json!({ "wait": { "until_event": "guide_settled", "timeout": "5m" } });
    let events = buffered_events(&[
        ("exposure_complete", json!({ "document_id": "doc-1" })),
        ("filter_switch", json!({ "filter_name": "Ha" })),
        ("guide_settled", Value::Null),
    ]);
    let outcome = run_with_events(root, &tools, &clock, events).await;
    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(clock.sleeps(), Vec::<Duration>::new());
}

#[tokio::test]
async fn test_wait_until_event_times_out_when_the_event_never_arrives() {
    // A closed, empty intake: the select's event arm pends forever, so
    // the full remaining budget is slept away and expiry raises.
    let tools = MockTools::none();
    let clock = MockClock::new();
    let root = json!({ "id": "settle",
                       "wait": { "until_event": "guide_settled", "timeout": "5m" } });
    let outcome = run_with_events(root, &tools, &clock, EventIntake::disconnected()).await;
    let error = failure(outcome);
    assert_eq!(
        error.message,
        "`wait` `until_event` `guide_settled` did not arrive within 5m"
    );
    assert_eq!(error.instruction_id.as_deref(), Some("settle"));
    assert_eq!(clock.sleeps(), vec![Duration::from_mins(5)]);
}

#[tokio::test]
async fn test_wait_until_event_times_out_past_non_matching_events() {
    // Non-matching buffered events are consumed but must not satisfy —
    // or extend — the wait.
    let tools = MockTools::none();
    let clock = MockClock::new();
    let root = json!({ "wait": { "until_event": "guide_settled", "timeout": "1m" } });
    let events = buffered_events(&[("exposure_complete", Value::Null)]);
    let outcome = run_with_events(root, &tools, &clock, events).await;
    assert_eq!(
        failure(outcome).message,
        "`wait` `until_event` `guide_settled` did not arrive within 1m"
    );
    assert_eq!(clock.sleeps(), vec![Duration::from_mins(1)]);
}

// --- fail, if, log -------------------------------------------------------------

#[tokio::test]
async fn test_fail_raises_with_the_evaluated_message() {
    let tools = MockTools::none();
    let root = json!({ "id": "give-up", "fail": { "message": "'exposure never converged'" } });
    let (outcome, _) = run_root(root, &tools).await;
    let error = failure(outcome);
    assert_eq!(error.message, "exposure never converged");
    assert_eq!(error.instruction_id.as_deref(), Some("give-up"));
    assert_eq!(error.tool, None);
}

#[tokio::test]
async fn test_fail_renders_a_non_string_message_as_json() {
    let tools = MockTools::none();
    let root = json!({ "fail": { "message": "1 + 1" } });
    let (outcome, _) = run_root(root, &tools).await;
    assert_eq!(failure(outcome).message, "2.0");
}

#[tokio::test]
async fn test_if_takes_the_else_branch_and_tolerates_a_missing_else() {
    let tools = MockTools::none();
    let root = json!({ "sequence": [
        { "if": "1 > 2",
          "then": [ { "set": { "session.then": "true" } } ],
          "else": [ { "set": { "session.else": "true" } } ] },
        { "if": "1 > 2",
          "then": [ { "set": { "session.skipped": "true" } } ] }
    ] });
    let (outcome, session) = run_root(root, &tools).await;
    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(session.get("then"), None);
    assert_eq!(session["else"], json!(true));
    assert_eq!(session.get("skipped"), None);
}

#[tokio::test]
async fn test_if_condition_must_be_a_boolean() {
    let tools = MockTools::none();
    let root = json!({ "if": "1 + 1", "then": [ { "log": { "message": "x" } } ] });
    let (outcome, _) = run_root(root, &tools).await;
    assert_eq!(
        failure(outcome).message,
        "`if` condition `1 + 1` must yield a boolean, got 2.0"
    );
}

#[tokio::test]
async fn test_log_value_evaluation_errors_are_workflow_errors() {
    let tools = MockTools::none();
    let root = json!({ "log": { "message": "m", "values": { "v": "session.x + 1" } } });
    let (outcome, _) = run_root(root, &tools).await;
    let error = failure(outcome);
    assert!(
        error
            .message
            .starts_with("`log` value `v` `session.x + 1` failed:"),
        "{}",
        error.message
    );
}

#[tokio::test]
async fn test_log_renders_values_and_continues() {
    let tools = MockTools::none();
    let root = json!({ "sequence": [
        { "log": { "level": "info", "message": "hello", "values": { "n": "1 + 1" } } },
        { "set": { "session.after": "true" } }
    ] });
    let (outcome, session) = run_root(root, &tools).await;
    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(session["after"], json!(true));
}

// --- safety termination --------------------------------------------------------

#[tokio::test]
async fn test_termination_skips_catch_runs_finally_best_effort_and_ends_the_run() {
    let tools = MockTools::new(|_, tool, _| match tool {
        "a" => Ok(json!({})),
        // Everything after the safety termination fails too — rp has torn
        // the MCP session down.
        _ => Err(ToolCallError::SessionTerminated("unsafe".to_owned())),
    });
    let root = json!({ "sequence": [
        { "try": [ { "tool": "a" }, { "tool": "b" }, { "tool": "never_reached" } ],
          "catch": [ { "tool": "catch_never_runs" } ],
          "finally": [ { "tool": "cleanup" } ] },
        { "tool": "after_try_never_runs" }
    ] });
    let (outcome, _) = run_root(root, &tools).await;

    assert_eq!(outcome, RunOutcome::Terminated);
    assert_eq!(tools.call_names(), vec!["a", "b", "cleanup"]);
}

#[tokio::test]
async fn test_termination_propagates_even_when_finally_merely_fails() {
    let tools = MockTools::new(|_, tool, _| match tool {
        "b" => Err(ToolCallError::SessionTerminated("unsafe".to_owned())),
        "cleanup" => Err(ToolCallError::Failed("session gone".to_owned())),
        _ => Ok(json!({})),
    });
    let root = json!({ "try": [ { "tool": "b" } ],
                       "finally": [ { "tool": "cleanup" } ] });
    let (outcome, _) = run_root(root, &tools).await;
    assert_eq!(outcome, RunOutcome::Terminated);
    assert_eq!(tools.call_names(), vec!["b", "cleanup"]);
}

#[tokio::test]
async fn test_blackboard_reflects_every_completed_set_at_termination() {
    let dir = tempfile::tempdir().unwrap();
    let tools = MockTools::new(|_, tool, _| match tool {
        "boom" => Err(ToolCallError::SessionTerminated("unsafe".to_owned())),
        _ => Ok(json!({})),
    });
    let doc = doc_with_root(json!({ "sequence": [
        { "set": { "session.progress": "7" } },
        { "tool": "boom" }
    ] }));
    let (outcome, _) = run_in(&dir, &doc, &json!({}), &tools, &MockClock::new()).await;
    assert_eq!(outcome, RunOutcome::Terminated);

    let on_disk: Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("session.json")).unwrap()).unwrap();
    assert_eq!(on_disk["progress"], json!(7.0));
}

// --- triggers: the safe-point pump ------------------------------------------

/// A live event channel: the sender side is handed to tool responders so a
/// tool call can emit an event mid-run (modelling `rp` emitting on the SSE
/// stream while an instruction is in flight).
fn live_events() -> (tokio::sync::mpsc::Sender<EngineEvent>, EventIntake) {
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    (tx, EventIntake::new(rx))
}

fn ev(event: &str, payload: Value) -> EngineEvent {
    EngineEvent {
        event: event.to_owned(),
        payload,
    }
}

/// Run a full document (triggers included) against a fresh blackboard and
/// the given intake; returns the outcome and the final session value.
async fn run_doc_with_events(
    doc: &Document,
    tools: &MockTools,
    clock: &(impl Clock + Sync),
    events: EventIntake,
) -> (RunOutcome, Value) {
    let dir = tempfile::tempdir().unwrap();
    let mut blackboard = Blackboard::load(dir.path().join("session.json"))
        .await
        .unwrap();
    let outcome = run(doc, &json!({}), &mut blackboard, tools, clock, events).await;
    let session = blackboard.value().clone();
    (outcome, session)
}

#[tokio::test]
async fn test_event_trigger_action_runs_at_the_next_safe_point_not_mid_instruction() {
    let (tx, events) = live_events();
    // The event is emitted while `a` is in flight; the trigger action must
    // run after `a` completes and before `b` starts.
    let tools = MockTools::new(move |_, tool, _| {
        if tool == "a" {
            tx.try_send(ev("hfr_degraded", json!({}))).unwrap();
        }
        Ok(json!({}))
    });
    let doc = make_doc(json!({
        "version": 1, "name": "t",
        "triggers": [ { "id": "correct", "on": { "event": "hfr_degraded" },
                        "do": [ { "tool": "refocus" } ] } ],
        "root": { "sequence": [ { "tool": "a" }, { "tool": "b" } ] }
    }));
    let (outcome, _) = run_doc_with_events(&doc, &tools, &MockClock::new(), events).await;

    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(tools.call_names(), vec!["a", "refocus", "b"]);
}

#[tokio::test]
async fn test_queued_trigger_actions_run_in_document_order() {
    let (tx, events) = live_events();
    let tools = MockTools::new(move |_, tool, _| {
        if tool == "a" {
            tx.try_send(ev("e", json!({}))).unwrap();
        }
        Ok(json!({}))
    });
    let doc = make_doc(json!({
        "version": 1, "name": "t",
        "triggers": [
            { "id": "t1", "on": { "event": "e" }, "do": [ { "tool": "first" } ] },
            { "id": "t2", "on": { "event": "e" }, "do": [ { "tool": "second" } ] }
        ],
        "root": { "sequence": [ { "tool": "a" }, { "tool": "b" } ] }
    }));
    let (outcome, _) = run_doc_with_events(&doc, &tools, &MockClock::new(), events).await;

    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(tools.call_names(), vec!["a", "first", "second", "b"]);
}

#[tokio::test]
async fn test_a_queued_trigger_does_not_queue_again() {
    let (tx, events) = live_events();
    // Two occurrences drain in the same batch; the trigger queues once.
    let tools = MockTools::new(move |_, tool, _| {
        if tool == "a" {
            tx.try_send(ev("e", json!({}))).unwrap();
            tx.try_send(ev("e", json!({}))).unwrap();
        }
        Ok(json!({}))
    });
    let doc = make_doc(json!({
        "version": 1, "name": "t",
        "triggers": [ { "id": "t", "on": { "event": "e" }, "do": [ { "tool": "act" } ] } ],
        "root": { "sequence": [ { "tool": "a" }, { "tool": "b" } ] }
    }));
    let (outcome, _) = run_doc_with_events(&doc, &tools, &MockClock::new(), events).await;

    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(tools.call_names(), vec!["a", "act", "b"]);
}

#[tokio::test]
async fn test_when_gate_filters_on_the_event_payload() {
    let (tx, events) = live_events();
    let tools = MockTools::new(move |_, tool, _| {
        match tool {
            "a" => tx
                .try_send(ev("exposure_complete", json!({ "hfr": 2.0 })))
                .unwrap(),
            "b" => tx
                .try_send(ev("exposure_complete", json!({ "hfr": 4.0 })))
                .unwrap(),
            _ => {}
        }
        Ok(json!({}))
    });
    let doc = make_doc(json!({
        "version": 1, "name": "t",
        "triggers": [ { "id": "refocus-on-degradation",
                        "on": { "event": "exposure_complete" },
                        "when": "event.hfr > 3.0",
                        "do": [ { "tool": "refocus" } ] } ],
        "root": { "sequence": [ { "tool": "a" }, { "tool": "b" }, { "tool": "c" } ] }
    }));
    let (outcome, _) = run_doc_with_events(&doc, &tools, &MockClock::new(), events).await;

    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(tools.call_names(), vec!["a", "b", "refocus", "c"]);
}

#[tokio::test]
async fn test_while_gate_is_evaluated_at_fire_time() {
    let (tx, events) = live_events();
    let tools = MockTools::new(move |_, tool, _| {
        if tool == "a" {
            tx.try_send(ev("e", json!({}))).unwrap();
        }
        Ok(json!({}))
    });
    // Both triggers queue from the same event; t1's action flips the flag
    // t2's `while` reads, so t2's queued firing is dropped at fire time.
    let doc = make_doc(json!({
        "version": 1, "name": "t",
        "triggers": [
            { "id": "t1", "on": { "event": "e" },
              "do": [ { "set": { "session.stop": "true" } } ] },
            { "id": "t2", "on": { "event": "e" }, "while": "session.stop != true",
              "do": [ { "tool": "never" } ] }
        ],
        "root": { "sequence": [ { "tool": "a" }, { "tool": "b" } ] }
    }));
    let (outcome, session) = run_doc_with_events(&doc, &tools, &MockClock::new(), events).await;

    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(tools.call_names(), vec!["a", "b"]);
    assert_eq!(session["stop"], json!(true));
    // t1 fired (bookkeeping recorded); t2's dropped firing did not.
    assert!(session["_triggers"]["t1"]["last_fired"].is_string());
    assert_eq!(session["_triggers"].get("t2"), None);
}

#[tokio::test]
async fn test_once_trigger_fires_at_most_once_per_session_including_resume() {
    let doc = make_doc(json!({
        "version": 1, "name": "t",
        "triggers": [ { "id": "t", "on": { "event": "e" }, "once": true,
                        "do": [ { "tool": "act" } ] } ],
        "root": { "sequence": [ { "tool": "a" }, { "tool": "b" } ] }
    }));
    let dir = tempfile::tempdir().unwrap();

    // First run: fires once; the second occurrence (after `b`) is gated.
    let (tx, events) = live_events();
    let tools = MockTools::new(move |_, tool, _| {
        if tool == "a" || tool == "b" {
            tx.try_send(ev("e", json!({}))).unwrap();
        }
        Ok(json!({}))
    });
    let mut blackboard = Blackboard::load(dir.path().join("session.json"))
        .await
        .unwrap();
    let outcome = run(
        &doc,
        &json!({}),
        &mut blackboard,
        &tools,
        &MockClock::new(),
        events,
    )
    .await;
    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(tools.call_names(), vec!["a", "act", "b"]);
    assert_eq!(
        blackboard.value()["_triggers"]["t"]["fired_once"],
        json!(true)
    );

    // Resume (same blackboard file): `once` is per session, so a fresh
    // occurrence must not fire it again.
    let (tx2, events2) = live_events();
    let tools2 = MockTools::new(move |_, tool, _| {
        if tool == "a" {
            tx2.try_send(ev("e", json!({}))).unwrap();
        }
        Ok(json!({}))
    });
    let mut reloaded = Blackboard::load(dir.path().join("session.json"))
        .await
        .unwrap();
    let outcome = run(
        &doc,
        &json!({}),
        &mut reloaded,
        &tools2,
        &MockClock::new(),
        events2,
    )
    .await;
    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(tools2.call_names(), vec!["a", "b"]);
}

#[tokio::test]
async fn test_cooldown_gates_by_wall_clock_and_reopens() {
    let (tx, events) = live_events();
    let tools = MockTools::new(move |_, tool, _| {
        if matches!(tool, "a" | "b" | "c") {
            tx.try_send(ev("e", json!({}))).unwrap();
        }
        Ok(json!({}))
    });
    // `a` fires the trigger; `b`'s occurrence lands inside the 10m
    // cooldown (no time has passed); the 15m wait reopens it for `c`'s.
    let doc = make_doc(json!({
        "version": 1, "name": "t",
        "triggers": [ { "id": "t", "on": { "event": "e" }, "cooldown": "10m",
                        "do": [ { "tool": "act" } ] } ],
        "root": { "sequence": [
            { "tool": "a" },
            { "tool": "b" },
            { "wait": { "duration": "15m" } },
            { "tool": "c" },
            { "tool": "d" }
        ] }
    }));
    let (outcome, _) = run_doc_with_events(&doc, &tools, &MockClock::new(), events).await;

    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(tools.call_names(), vec!["a", "act", "b", "c", "act", "d"]);
}

#[tokio::test]
async fn test_uncaught_trigger_action_error_fails_the_session_naming_the_trigger() {
    let (tx, events) = live_events();
    let tools = MockTools::new(move |_, tool, _| {
        if tool == "a" {
            tx.try_send(ev("e", json!({}))).unwrap();
        }
        if tool == "boom" {
            return Err(ToolCallError::Failed("dew heater offline".to_owned()));
        }
        Ok(json!({}))
    });
    let doc = make_doc(json!({
        "version": 1, "name": "t",
        "triggers": [ { "id": "t", "on": { "event": "e" }, "do": [ { "tool": "boom" } ] } ],
        "root": { "sequence": [ { "tool": "a" }, { "tool": "never" } ] }
    }));
    let (outcome, _) = run_doc_with_events(&doc, &tools, &MockClock::new(), events).await;

    let error = failure(outcome);
    assert_eq!(
        error.message,
        "trigger `t`: tool `boom` failed: dew heater offline"
    );
    assert_eq!(tools.call_names(), vec!["a", "boom"]);
}

#[tokio::test]
async fn test_a_try_inside_the_do_block_makes_a_trigger_resilient() {
    let (tx, events) = live_events();
    let tools = MockTools::new(move |_, tool, _| {
        if tool == "a" {
            tx.try_send(ev("e", json!({}))).unwrap();
        }
        if tool == "boom" {
            return Err(ToolCallError::Failed("still offline".to_owned()));
        }
        Ok(json!({}))
    });
    let doc = make_doc(json!({
        "version": 1, "name": "t",
        "triggers": [ { "id": "t", "on": { "event": "e" },
                        "do": [ { "try": [ { "tool": "boom" } ],
                                  "catch": [ { "tool": "cleanup" } ] } ] } ],
        "root": { "sequence": [ { "tool": "a" }, { "tool": "b" } ] }
    }));
    let (outcome, session) = run_doc_with_events(&doc, &tools, &MockClock::new(), events).await;

    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(tools.call_names(), vec!["a", "boom", "cleanup", "b"]);
    // The caught firing still counts as fired.
    assert!(session["_triggers"]["t"]["last_fired"].is_string());
}

#[tokio::test]
async fn test_trigger_action_scoping_result_error_and_event() {
    let (tx, events) = live_events();
    let tools = MockTools::new(move |_, tool, _| {
        if tool == "a" {
            tx.try_send(ev("e", json!({ "reason": "drift" }))).unwrap();
            return Ok(json!({ "v": "root" }));
        }
        Ok(json!({ "v": "trigger" }))
    });
    // The `do` block starts with `result` null and sees `event.*`; the
    // tree's `result` is restored after the action (the trigger's own
    // tool result must not leak into the `set` that follows `a`).
    let doc = make_doc(json!({
        "version": 1, "name": "t",
        "triggers": [ { "id": "t", "on": { "event": "e" },
                        "do": [
                            { "set": { "session.result_was_null": "result == null",
                                       "session.reason": "event.reason" } },
                            { "tool": "inner" }
                        ] } ],
        "root": { "sequence": [
            { "tool": "a" },
            { "set": { "session.root_result": "result.v" } }
        ] }
    }));
    let (outcome, session) = run_doc_with_events(&doc, &tools, &MockClock::new(), events).await;

    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(session["result_was_null"], json!(true));
    assert_eq!(session["reason"], json!("drift"));
    assert_eq!(session["root_result"], json!("root"));
}

#[tokio::test]
async fn test_when_gate_yielding_a_non_boolean_is_a_workflow_error() {
    let events = buffered_events(&[("e", json!({ "x": 5 }))]);
    let doc = make_doc(json!({
        "version": 1, "name": "t",
        "triggers": [ { "id": "t", "on": { "event": "e" }, "when": "event.x",
                        "do": [ { "tool": "never" } ] } ],
        "root": { "wait": { "duration": "1s" } }
    }));
    let (outcome, _) =
        run_doc_with_events(&doc, &MockTools::none(), &MockClock::new(), events).await;

    let error = failure(outcome);
    assert_eq!(
        error.message,
        "trigger `t`: `when` gate `event.x` must yield a boolean, got 5"
    );
}

// --- triggers: waits as long safe points -------------------------------------

#[tokio::test]
async fn test_trigger_fires_during_a_duration_wait_without_consuming_its_budget() {
    // The action's own 3s wait must not count against the outer 30s wait:
    // the sleep ledger shows both in full.
    let events = buffered_events(&[("e", json!({}))]);
    let clock = MockClock::new();
    let doc = make_doc(json!({
        "version": 1, "name": "t",
        "triggers": [ { "id": "t", "on": { "event": "e" },
                        "do": [ { "wait": { "duration": "3s" } } ] } ],
        "root": { "wait": { "duration": "30s" } }
    }));
    let (outcome, session) = run_doc_with_events(&doc, &MockTools::none(), &clock, events).await;

    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(
        clock.sleeps(),
        vec![Duration::from_secs(3), Duration::from_secs(30)]
    );
    assert!(session["_triggers"]["t"]["last_fired"].is_string());
}

#[tokio::test]
async fn test_until_event_timeout_budget_excludes_trigger_action_time() {
    let events = buffered_events(&[("progress", json!({}))]);
    let clock = MockClock::new();
    let doc = make_doc(json!({
        "version": 1, "name": "t",
        "triggers": [ { "id": "t", "on": { "event": "progress" },
                        "do": [ { "wait": { "duration": "3s" } } ] } ],
        "root": { "id": "settle",
                  "wait": { "until_event": "never_comes", "timeout": "10s" } }
    }));
    let (outcome, _) = run_doc_with_events(&doc, &MockTools::none(), &clock, events).await;

    let error = failure(outcome);
    assert_eq!(
        error.message,
        "`wait` `until_event` `never_comes` did not arrive within 10s"
    );
    // 3s of action time, then the full 10s budget — not 7s.
    assert_eq!(
        clock.sleeps(),
        vec![Duration::from_secs(3), Duration::from_secs(10)]
    );
}

#[tokio::test]
async fn test_the_awaited_event_also_feeds_trigger_evaluation() {
    let (tx, events) = live_events();
    let tools = MockTools::new(move |_, tool, _| {
        if tool == "a" {
            tx.try_send(ev("done", json!({}))).unwrap();
        }
        Ok(json!({}))
    });
    let clock = MockClock::new();
    let doc = make_doc(json!({
        "version": 1, "name": "t",
        "triggers": [ { "id": "t", "on": { "event": "done" }, "do": [ { "tool": "act" } ] } ],
        "root": { "sequence": [
            { "tool": "a" },
            { "wait": { "until_event": "done", "timeout": "5m" } },
            { "tool": "b" }
        ] }
    }));
    let (outcome, _) = run_doc_with_events(&doc, &tools, &clock, events).await;

    assert_eq!(outcome, RunOutcome::Completed);
    // One `done`: the trigger fires at the safe point after `a`, and the
    // wait is satisfied by the same occurrence — with no sleeping.
    assert_eq!(tools.call_names(), vec!["a", "act", "b"]);
    assert_eq!(clock.sleeps(), Vec::<Duration>::new());
}

// --- triggers: poll sources ---------------------------------------------------

#[tokio::test]
async fn test_poll_trigger_polls_on_schedule_and_fires_through_its_when_gate() {
    let clock = MockClock::new();
    // First cycle gated by `when`, second passes.
    let responses = Mutex::new(VecDeque::from([
        json!({ "ok": false }),
        json!({ "ok": true }),
    ]));
    let tools = MockTools::new(move |_, tool, _| {
        Ok(match tool {
            "check" => responses.lock().unwrap().pop_front().unwrap(),
            _ => json!({}),
        })
    });
    let doc = make_doc(json!({
        "version": 1, "name": "t",
        "triggers": [ { "id": "t",
                        "on": { "poll": { "tool": "check", "interval": "10s" } },
                        "when": "event.ok == true",
                        "do": [ { "tool": "act" } ] } ],
        "root": { "wait": { "duration": "25s" } }
    }));
    let (outcome, _) = run_doc_with_events(&doc, &tools, &clock, EventIntake::disconnected()).await;

    assert_eq!(outcome, RunOutcome::Completed);
    // First due one interval after run start; sleep segments clamp to the
    // schedule: 10s, 10s, then the remaining 5s.
    assert_eq!(tools.call_names(), vec!["check", "check", "act"]);
    assert_eq!(
        clock.sleeps(),
        vec![
            Duration::from_secs(10),
            Duration::from_secs(10),
            Duration::from_secs(5)
        ]
    );
}

#[tokio::test]
async fn test_poll_failure_skips_the_cycle_without_failing_the_session() {
    let clock = MockClock::new();
    let responses = Mutex::new(VecDeque::from([
        Err(ToolCallError::Failed("flaky".to_owned())),
        Ok(json!({})),
    ]));
    let tools = MockTools::new(move |_, tool, _| match tool {
        "check" => responses.lock().unwrap().pop_front().unwrap(),
        _ => Ok(json!({})),
    });
    let doc = make_doc(json!({
        "version": 1, "name": "t",
        "triggers": [ { "id": "t",
                        "on": { "poll": { "tool": "check", "interval": "10s" } },
                        "do": [ { "tool": "act" } ] } ],
        "root": { "wait": { "duration": "25s" } }
    }));
    let (outcome, _) = run_doc_with_events(&doc, &tools, &clock, EventIntake::disconnected()).await;

    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(tools.call_names(), vec!["check", "check", "act"]);
}

#[tokio::test]
async fn test_a_due_poll_whose_trigger_cannot_fire_skips_the_tool_call() {
    let clock = MockClock::new();
    let tools = MockTools::ok(json!({}));
    let doc = make_doc(json!({
        "version": 1, "name": "t",
        "triggers": [ { "id": "t", "once": true,
                        "on": { "poll": { "tool": "check", "interval": "10s" } },
                        "do": [ { "tool": "act" } ] } ],
        "root": { "wait": { "duration": "35s" } }
    }));
    let (outcome, _) = run_doc_with_events(&doc, &tools, &clock, EventIntake::disconnected()).await;

    assert_eq!(outcome, RunOutcome::Completed);
    // Fires at 10s; the dues at 20s and 30s skip the call (`once` spent).
    assert_eq!(tools.call_names(), vec!["check", "act"]);
}

#[tokio::test]
async fn test_a_poll_argument_that_fails_to_evaluate_skips_the_cycle() {
    let clock = MockClock::new();
    let tools = MockTools::none();
    let doc = make_doc(json!({
        "version": 1, "name": "t",
        "triggers": [ { "id": "t",
                        "on": { "poll": { "tool": "check", "interval": "10s",
                                          "args": { "pos": { "$expr": "1 / 0" } } } },
                        "do": [ { "tool": "act" } ] } ],
        "root": { "wait": { "duration": "15s" } }
    }));
    let (outcome, _) = run_doc_with_events(&doc, &tools, &clock, EventIntake::disconnected()).await;

    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(tools.calls(), Vec::<(String, serde_json::Value)>::new());
}

// --- triggers: synthetic corrections ------------------------------------------

#[tokio::test]
async fn test_an_aborted_tool_result_synthesizes_an_immediate_correction() {
    let tools = MockTools::new(|_, tool, _| {
        Ok(match tool {
            "capture" => json!({
                "status": "aborted",
                "correction": { "action": "focus", "reason": "HFR 4.8", "source": "analyzer" }
            }),
            _ => json!({}),
        })
    });
    let doc = make_doc(json!({
        "version": 1, "name": "t",
        "triggers": [ { "id": "on-correction", "on": { "event": "correction_requested" },
                        "do": [ { "set": {
                            "session.action": "event.action",
                            "session.delivery": "event.delivery",
                            "session.source": "event.source" } } ] } ],
        "root": { "sequence": [ { "tool": "capture" }, { "tool": "b" } ] }
    }));
    let (outcome, session) =
        run_doc_with_events(&doc, &tools, &MockClock::new(), EventIntake::disconnected()).await;

    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(session["action"], json!("focus"));
    assert_eq!(session["delivery"], json!("immediate"));
    assert_eq!(session["source"], json!("analyzer"));
}

#[tokio::test]
async fn test_a_pending_correction_synthesizes_an_after_current_correction() {
    let tools = MockTools::new(|_, tool, _| {
        Ok(match tool {
            "capture" => json!({
                "image_path": "/data/f.fits",
                "pending_correction": { "action": "focus", "reason": "trending" }
            }),
            _ => json!({}),
        })
    });
    let doc = make_doc(json!({
        "version": 1, "name": "t",
        "triggers": [ { "id": "on-correction", "on": { "event": "correction_requested" },
                        "do": [ { "set": { "session.delivery": "event.delivery" } } ] } ],
        "root": { "sequence": [
            { "tool": "capture" },
            // The carrying result stays in scope for the tree.
            { "set": { "session.path": "result.image_path" } }
        ] }
    }));
    let (outcome, session) =
        run_doc_with_events(&doc, &tools, &MockClock::new(), EventIntake::disconnected()).await;

    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(session["delivery"], json!("after_current"));
    assert_eq!(session["path"], json!("/data/f.fits"));
}

#[tokio::test]
async fn test_a_blocked_by_correction_result_synthesizes_an_immediate_correction() {
    let tools = MockTools::new(|_, tool, _| {
        Ok(match tool {
            "slew" => json!({
                "status": "blocked_by_correction",
                "correction": { "action": "focus", "reason": "frame bad", "source": "analyzer" }
            }),
            _ => json!({}),
        })
    });
    let doc = make_doc(json!({
        "version": 1, "name": "t",
        "triggers": [ { "id": "on-correction", "on": { "event": "correction_requested" },
                        "do": [ { "set": { "session.delivery": "event.delivery" } } ] } ],
        "root": { "tool": "slew" }
    }));
    let (outcome, session) =
        run_doc_with_events(&doc, &tools, &MockClock::new(), EventIntake::disconnected()).await;

    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(session["delivery"], json!("immediate"));
}

// --- the golden document end-to-end ----------------------------------------------

/// Responder for the shipped `calibrator_flats.json` golden document,
/// faithful to `rp`'s actual tool results: `get_camera_info` reports the
/// exposure limits as humantime strings, `capture` returns
/// `image_path`/`document_id`, `compute_image_stats` serves the scripted
/// medians, and the cover/calibrator/filter tools return their status
/// objects.
fn flats_tools(medians: Vec<u32>) -> MockTools {
    flats_tools_with_cover(medians, "Open")
}

fn flats_tools_with_cover(medians: Vec<u32>, cover_state: &'static str) -> MockTools {
    let medians = Mutex::new(VecDeque::from(medians));
    MockTools::new(move |_, tool, args| match tool {
        "get_camera_info" => Ok(json!({
            "camera_id": "cam",
            "max_adu": 65535,
            "exposure_min": "1ms",
            "exposure_max": "30s"
        })),
        "get_cover_state" => Ok(json!({ "cover_state": cover_state })),
        "capture" => Ok(json!({ "image_path": "/tmp/flat.fits", "document_id": "doc-1" })),
        "compute_image_stats" => Ok(json!({
            "median_adu": medians
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected compute_image_stats call")
        })),
        "set_filter" => Ok(json!({ "filter_wheel_id": "fw", "position": 0 })),
        "calibrator_on" => Ok(json!({
            "status": "ready",
            "brightness": args.get("brightness").cloned().unwrap_or_else(|| json!(255))
        })),
        _ => Ok(json!({ "status": "ok" })),
    })
}

fn flats_params(doc: &Document, filters: Value) -> Value {
    bind_parameters(
        &doc.parameters,
        Some(&json!({
            "camera_id": "cam",
            "filter_wheel_id": "fw",
            "calibrator_id": "panel",
            "filters": filters
        })),
    )
    .unwrap()
}

#[tokio::test]
async fn test_golden_calibrator_flats_document_runs_the_full_algorithm() {
    let doc = make_doc(crate::document::corpus::golden_calibrator_flats());
    let params = flats_params(
        &doc,
        json!([{ "name": "L", "count": 2 }, { "name": "R", "count": 1 }]),
    );
    // target_adu = 65535 * 0.5 = 32767.5. For L the first median (16000)
    // is outside the 5 % tolerance and the exposure is rescaled; the
    // second (32000) is inside it — two passes. For R the very first
    // median converges.
    let tools = flats_tools(vec![16000, 32000, 32000]);
    let dir = tempfile::tempdir().unwrap();
    let (outcome, session) = run_in(&dir, &doc, &params, &tools, &MockClock::new()).await;

    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(
        tools.call_names(),
        vec![
            "get_camera_info",
            "get_cover_state", // initial state, restored by the finally
            "close_cover",
            "start_cooldown",
            "calibrator_on",
            "set_filter",          // L
            "capture",             // find-exposure pass 1
            "compute_image_stats", // 16000 → rescale
            "capture",             // find-exposure pass 2
            "compute_image_stats", // 32000 → converged
            "capture",             // L flat 1
            "capture",             // L flat 2
            "set_filter",          // R
            "capture",             // find-exposure pass 1
            "compute_image_stats", // 32000 → converged
            "capture",             // R flat 1
            "calibrator_off",
            "open_cover",   // the cover started Open
            "start_warmup", // the finally's last act
        ]
    );
    assert_eq!(session["target_adu"], json!(32767.5));
    // `set` copies `result.median_adu` verbatim — the mock's JSON integer
    // survives untouched (arithmetic-produced values below are f64).
    assert_eq!(session["median_adu"], json!(32000));
    assert_eq!(session["filter_index"], json!(2.0));
    assert_eq!(session["report"]["total_frames"], json!(3.0));

    let captures: Vec<Value> = tools
        .calls()
        .into_iter()
        .filter(|(name, _)| name == "capture")
        .map(|(_, args)| args["duration"].clone())
        .collect();
    // L's search starts at the 1 s initial exposure, rescales once
    // (32767.5 / 16000 ≈ 2.048 s), and — matching the Rust orchestrator —
    // the converging pass does NOT rescale again: both L flats reuse the
    // duration that converged. R's search resets to the initial exposure
    // and converges immediately, so R's flat uses 1 s.
    assert_eq!(captures[0], json!("1s"));
    assert_eq!(captures[1], captures[2], "converged duration was rescaled");
    assert_eq!(captures[2], captures[3]);
    assert_ne!(captures[1], json!("1s"));
    assert_eq!(captures[4], json!("1s"));
    assert_eq!(captures[5], json!("1s"));
}

#[tokio::test]
async fn test_golden_calibrator_flats_skips_set_filter_on_a_filterless_rig() {
    let doc = make_doc(crate::document::corpus::golden_calibrator_flats());
    // No `filter_wheel_id` supplied — the parameter defaults to `""`,
    // the document's no-filter-wheel sentinel.
    let params = bind_parameters(
        &doc.parameters,
        Some(&json!({
            "camera_id": "cam",
            "calibrator_id": "panel",
            "filters": [{ "name": "OSC", "count": 2 }]
        })),
    )
    .unwrap();
    let tools = flats_tools(vec![32000]);
    let dir = tempfile::tempdir().unwrap();
    let (outcome, session) = run_in(&dir, &doc, &params, &tools, &MockClock::new()).await;

    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(
        tools.call_names(),
        vec![
            "get_camera_info",
            "get_cover_state",
            "close_cover",
            "start_cooldown",
            "calibrator_on",
            "capture",             // find-exposure pass 1
            "compute_image_stats", // 32000 → converged
            "capture",             // OSC flat 1
            "capture",             // OSC flat 2
            "calibrator_off",
            "open_cover",
            "start_warmup",
        ],
        "a filterless plan must never call set_filter"
    );
    assert_eq!(session["report"]["total_frames"], json!(2.0));
}

#[tokio::test]
async fn test_golden_calibrator_flats_leaves_a_closed_cover_closed() {
    let doc = make_doc(crate::document::corpus::golden_calibrator_flats());
    let params = flats_params(&doc, json!([{ "name": "L", "count": 1 }]));
    let tools = flats_tools_with_cover(vec![32000], "Closed");
    let dir = tempfile::tempdir().unwrap();
    let (outcome, _) = run_in(&dir, &doc, &params, &tools, &MockClock::new()).await;

    assert_eq!(outcome, RunOutcome::Completed);
    let names = tools.call_names();
    assert!(
        !names.contains(&"open_cover".to_owned()),
        "a cover that started Closed must stay closed: {names:?}"
    );
    assert_eq!(
        &names[names.len() - 2..],
        &["calibrator_off".to_owned(), "start_warmup".to_owned()]
    );
}

#[tokio::test]
async fn test_golden_calibrator_flats_halves_brightness_when_pinned_over_bright() {
    let doc = make_doc(crate::document::corpus::golden_calibrator_flats());
    // max_iterations 2 keeps the saturated pass short: two over-target
    // medians exhaust the first search, the ladder halves 255 → 127 and
    // re-lights, and the dimmer search converges immediately.
    let params = bind_parameters(
        &doc.parameters,
        Some(&json!({
            "camera_id": "cam",
            "filter_wheel_id": "fw",
            "calibrator_id": "panel",
            "max_iterations": 2,
            "filters": [{ "name": "L", "count": 1 }]
        })),
    )
    .unwrap();
    let tools = flats_tools(vec![60000, 60000, 32000]);
    let dir = tempfile::tempdir().unwrap();
    let (outcome, session) = run_in(&dir, &doc, &params, &tools, &MockClock::new()).await;

    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(
        tools.call_names(),
        vec![
            "get_camera_info",
            "get_cover_state",
            "close_cover",
            "start_cooldown",
            "calibrator_on", // full brightness (255)
            "set_filter",    // L
            "capture",       // pass 1: 60000, over target
            "compute_image_stats",
            "capture", // pass 2: 60000 — search exhausted
            "compute_image_stats",
            "calibrator_on", // ladder: re-lit at 127
            "capture",       // pass 1 at 127: 32000 → converged
            "compute_image_stats",
            "capture", // L flat 1
            "calibrator_off",
            "open_cover",
            "start_warmup", // the finally's last act
        ]
    );
    // The halved brightness reaches rp as a JSON *integer* (rp's
    // brightness parameter is a u32; a 127.0 would bounce).
    let ladder_on = tools
        .calls()
        .into_iter()
        .filter(|(name, _)| name == "calibrator_on")
        .nth(1)
        .expect("a second calibrator_on");
    assert_eq!(ladder_on.1["brightness"], json!(127));
    // The blackboard keeps the expression's f64; only tool arguments
    // get the integral normalization.
    assert_eq!(session["brightness"], json!(127.0));
}

#[tokio::test]
async fn test_golden_calibrator_flats_cleans_up_when_the_loop_fails() {
    let doc = make_doc(crate::document::corpus::golden_calibrator_flats());
    let params = flats_params(&doc, json!([{ "name": "L", "count": 2 }]));
    let tools = MockTools::new(|_, tool, _| match tool {
        "get_camera_info" => Ok(json!({
            "max_adu": 65535,
            "exposure_min": "1ms",
            "exposure_max": "30s"
        })),
        "get_cover_state" => Ok(json!({ "cover_state": "Open" })),
        "calibrator_on" => Ok(json!({ "status": "ready", "brightness": 255 })),
        "compute_image_stats" => Err(ToolCallError::Failed("stats broke".to_owned())),
        _ => Ok(json!({ "status": "ok" })),
    });
    let dir = tempfile::tempdir().unwrap();
    let (outcome, _) = run_in(&dir, &doc, &params, &tools, &MockClock::new()).await;

    // The error propagates out of the find-exposure loop, but the
    // `finally` block still turns the panel off and reopens the cover —
    // the calibrator-flats cleanup-on-failure contract.
    let error = failure(outcome);
    assert_eq!(
        error.message,
        "tool `compute_image_stats` failed: stats broke"
    );
    let names = tools.call_names();
    assert_eq!(
        &names[names.len() - 3..],
        &[
            "calibrator_off".to_owned(),
            "open_cover".to_owned(),
            "start_warmup".to_owned()
        ]
    );
}

#[tokio::test]
async fn test_golden_calibrator_flats_rejects_a_zero_target_adu_before_moving_anything() {
    // The Rust oracle errors on `target_adu == 0` inside its exposure
    // search; the document's fail-fast guard does one better and raises
    // before the `try` — the cover never closes and no frame is wasted
    // on a division-by-zero mid-loop.
    let doc = make_doc(crate::document::corpus::golden_calibrator_flats());
    let params = flats_params(&doc, json!([{ "name": "L", "count": 2 }]));
    let tools = MockTools::new(|_, tool, _| match tool {
        "get_camera_info" => Ok(json!({
            "max_adu": 0,
            "exposure_min": "1ms",
            "exposure_max": "30s"
        })),
        other => panic!("unexpected tool call `{other}` after a zero target_adu"),
    });
    let dir = tempfile::tempdir().unwrap();
    let (outcome, _) = run_in(&dir, &doc, &params, &tools, &MockClock::new()).await;

    let error = failure(outcome);
    assert!(
        error.message.contains("target_adu is not positive"),
        "{}",
        error.message
    );
    assert_eq!(tools.call_names(), vec!["get_camera_info"]);
}

/// `rp`-faithful mock tool surface for the sky-flat document: scripted
/// per-frame medians, an LST reading, and status objects for the mount
/// and wheel tools.
fn sky_flat_tools(medians: Vec<u32>) -> MockTools {
    let medians = Mutex::new(VecDeque::from(medians));
    MockTools::new(move |_, tool, _| match tool {
        "get_camera_info" => Ok(json!({
            "camera_id": "cam",
            "max_adu": 65535,
            "exposure_min": "1ms",
            "exposure_max": "60s"
        })),
        "get_local_sidereal_time" => Ok(json!({ "lst_hours": 23.75 })),
        "capture" => Ok(json!({ "image_path": "/tmp/flat.fits", "document_id": "doc-1" })),
        "compute_image_stats" => Ok(json!({
            "median_adu": medians
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected compute_image_stats call")
        })),
        _ => Ok(json!({ "status": "ok" })),
    })
}

fn sky_flat_params(doc: &Document, overrides: Value) -> Value {
    let mut supplied = json!({
        "camera_id": "cam",
        "filter_wheel_id": "fw",
        "filters": [{ "name": "L", "count": 2 }, { "name": "R", "count": 1 }],
        "site_latitude_degrees": 47.5,
        "ra_offset_hours": 0.5
    });
    if let (Some(base), Some(extra)) = (supplied.as_object_mut(), overrides.as_object()) {
        for (key, value) in extra {
            base.insert(key.clone(), value.clone());
        }
    }
    bind_parameters(&doc.parameters, Some(&supplied)).unwrap()
}

#[track_caller]
fn capture_durations(tools: &MockTools) -> Vec<Value> {
    tools
        .calls()
        .into_iter()
        .filter(|(name, _)| name == "capture")
        .map(|(_, args)| args["duration"].clone())
        .collect()
}

#[tokio::test]
async fn test_golden_sky_flat_points_at_the_zenith_and_rescales_every_frame() {
    let doc = make_doc(crate::document::corpus::golden_sky_flat());
    let params = sky_flat_params(&doc, json!({}));
    // target_adu = 32767.5, tolerance 0.1 → band [29490.75, 36044.25].
    // All three medians are in-band, so every frame counts — and unlike
    // the panel flats, EVERY frame rescales (the sky keeps changing).
    let tools = sky_flat_tools(vec![32000, 33000, 32767]);
    let dir = tempfile::tempdir().unwrap();
    let (outcome, session) = run_in(&dir, &doc, &params, &tools, &MockClock::new()).await;

    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(
        tools.call_names(),
        vec![
            "get_camera_info",
            "unpark",
            "set_tracking", // on — slew requires it
            "start_cooldown",
            "get_local_sidereal_time",
            "slew",         // to (LST + offset, site latitude)
            "set_tracking", // off — flats trail deliberately
            "set_filter",   // L
            "capture",      // L flat 1
            "compute_image_stats",
            "capture", // L flat 2
            "compute_image_stats",
            "set_filter", // R
            "capture",    // R flat 1
            "compute_image_stats",
            "park",
            "start_warmup", // the finally's last act
        ]
    );

    // Zenith pointing: RA = (23.75 + 0.5) mod 24, dec = the latitude.
    let slew_args = &tools.calls()[5].1;
    assert_eq!(*slew_args, json!({ "ra": 0.25, "dec": 47.5 }));
    let tracking: Vec<Value> = tools
        .calls()
        .into_iter()
        .filter(|(name, _)| name == "set_tracking")
        .map(|(_, args)| args["enabled"].clone())
        .collect();
    assert_eq!(tracking, vec![json!(true), json!(false)]);

    // Rescale-always: the second frame's duration differs from the
    // first's, and R inherits L's last rescaled duration rather than
    // resetting to initial_duration — the sky at hand, not a constant,
    // is the best predictor.
    let durations = capture_durations(&tools);
    assert_eq!(durations[0], json!("1s"));
    assert_ne!(durations[1], durations[0], "frame 2 must be rescaled");
    assert_ne!(durations[2], json!("1s"), "R must inherit L's duration");

    assert_eq!(session["report"]["total_frames"], json!(3.0));
    assert_eq!(session["report"]["window_over"], json!(false));
    assert_eq!(session["filter_index"], json!(2.0));
}

#[tokio::test]
async fn test_golden_sky_flat_discards_an_out_of_band_frame_and_recaptures() {
    let doc = make_doc(crate::document::corpus::golden_sky_flat());
    let params = sky_flat_params(&doc, json!({ "filters": [{ "name": "L", "count": 1 }] }));
    // 16000 is far below the band → discarded, but still drives the
    // rescale that brings the retry in-band.
    let tools = sky_flat_tools(vec![16000, 32767]);
    let dir = tempfile::tempdir().unwrap();
    let (outcome, session) = run_in(&dir, &doc, &params, &tools, &MockClock::new()).await;

    assert_eq!(outcome, RunOutcome::Completed);
    let durations = capture_durations(&tools);
    assert_eq!(durations.len(), 2, "one discard, one counted frame");
    assert_ne!(durations[1], durations[0]);
    assert_eq!(session["report"]["total_frames"], json!(1.0));
    assert_eq!(session["report"]["window_over"], json!(false));
}

#[tokio::test]
async fn test_golden_sky_flat_dusk_ends_the_run_when_a_ceiling_frame_is_still_dark() {
    let doc = make_doc(crate::document::corpus::golden_sky_flat());
    let params = sky_flat_params(&doc, json!({ "max_exposure_duration": "2s" }));
    // A very dark 1 s frame is NOT enough to close the window — the
    // 2 s operator ceiling has only been pointed at by the rescale's
    // clamp, not tested. The retry at the ceiling reads dark too: now
    // the window is over — the run completes with a partial report and
    // the remaining filters are skipped (the sky only gets darker).
    let tools = sky_flat_tools(vec![1000, 1000]);
    let dir = tempfile::tempdir().unwrap();
    let (outcome, session) = run_in(&dir, &doc, &params, &tools, &MockClock::new()).await;

    assert_eq!(outcome, RunOutcome::Completed);
    let names = tools.call_names();
    assert_eq!(
        names.iter().filter(|n| *n == "capture").count(),
        2,
        "one frame to reach the ceiling, one to test it — then no more"
    );
    let durations = capture_durations(&tools);
    assert_eq!(durations[1], json!("2s"), "the ceiling itself was tested");
    assert_eq!(
        names.iter().filter(|n| *n == "set_filter").count(),
        1,
        "the R filter is never mounted"
    );
    assert!(names.contains(&"park".to_owned()), "shutdown still parks");
    assert_eq!(session["report"]["total_frames"], json!(0.0));
    assert_eq!(session["report"]["window_over"], json!(true));
}

#[tokio::test]
async fn test_golden_sky_flat_dusk_waits_for_a_bright_sky_to_dim_at_the_floor() {
    let doc = make_doc(crate::document::corpus::golden_sky_flat());
    let params = sky_flat_params(
        &doc,
        json!({
            "filters": [{ "name": "L", "count": 1 }],
            "min_exposure_duration": "500ms",
            "initial_duration": "500ms"
        }),
    );
    // A saturated frame captured AT the 500 ms floor: at dusk the sky
    // is still dimming toward the window, so the document waits 30 s
    // and re-tests.
    let tools = sky_flat_tools(vec![65535, 32767]);
    let clock = MockClock::new();
    let dir = tempfile::tempdir().unwrap();
    let (outcome, session) = run_in(&dir, &doc, &params, &tools, &clock).await;

    assert_eq!(outcome, RunOutcome::Completed);
    assert!(
        clock.sleeps().contains(&Duration::from_secs(30)),
        "the bright-sky wait never slept: {:?}",
        clock.sleeps()
    );
    let durations = capture_durations(&tools);
    assert_eq!(durations.len(), 2);
    assert_eq!(durations[1], json!("500ms"), "retry at the clamped floor");
    assert_eq!(session["report"]["total_frames"], json!(1.0));
}

#[tokio::test]
async fn test_golden_sky_flat_dawn_ends_the_run_when_a_floor_frame_is_still_bright() {
    let doc = make_doc(crate::document::corpus::golden_sky_flat());
    let params = sky_flat_params(
        &doc,
        json!({
            "filters": [{ "name": "L", "count": 1 }],
            "dawn": true,
            "min_exposure_duration": "500ms",
            "initial_duration": "500ms"
        }),
    );
    // The same saturated-at-the-floor frame that dusk waits out means
    // the window is OVER at dawn — the sky only gets brighter.
    let tools = sky_flat_tools(vec![65535]);
    let clock = MockClock::new();
    let dir = tempfile::tempdir().unwrap();
    let (outcome, session) = run_in(&dir, &doc, &params, &tools, &clock).await;

    assert_eq!(outcome, RunOutcome::Completed);
    assert!(
        !clock.sleeps().contains(&Duration::from_secs(30)),
        "dawn must not wait for a brightening sky"
    );
    assert_eq!(capture_durations(&tools).len(), 1);
    assert_eq!(session["report"]["total_frames"], json!(0.0));
    assert_eq!(session["report"]["window_over"], json!(true));
}

#[tokio::test]
async fn test_golden_sky_flat_dawn_waits_for_a_dark_sky_to_brighten_at_the_ceiling() {
    let doc = make_doc(crate::document::corpus::golden_sky_flat());
    let params = sky_flat_params(
        &doc,
        json!({
            "filters": [{ "name": "L", "count": 1 }],
            "dawn": true,
            "max_exposure_duration": "2s",
            "initial_duration": "2s"
        }),
    );
    // A dark frame captured AT the 2 s ceiling: at dawn the sky is
    // still brightening toward the window, so the document waits 30 s
    // and re-tests — the mirror of the dusk floor wait.
    let tools = sky_flat_tools(vec![1000, 32767]);
    let clock = MockClock::new();
    let dir = tempfile::tempdir().unwrap();
    let (outcome, session) = run_in(&dir, &doc, &params, &tools, &clock).await;

    assert_eq!(outcome, RunOutcome::Completed);
    assert!(
        clock.sleeps().contains(&Duration::from_secs(30)),
        "the dark-sky wait never slept: {:?}",
        clock.sleeps()
    );
    assert_eq!(capture_durations(&tools).len(), 2);
    assert_eq!(session["report"]["total_frames"], json!(1.0));
    assert_eq!(session["report"]["window_over"], json!(false));
}

#[tokio::test]
async fn test_golden_sky_flat_treats_an_exhausted_attempt_budget_as_a_closed_window() {
    let doc = make_doc(crate::document::corpus::golden_sky_flat());
    let params = sky_flat_params(
        &doc,
        json!({ "filters": [{ "name": "L", "count": 1 }], "max_extra_attempts": 1 }),
    );
    // Two out-of-band frames away from either rail exhaust the
    // 1 + 1 attempt budget without a counted flat: the run ends with a
    // partial report instead of failing — partial flats are usable.
    let tools = sky_flat_tools(vec![20000, 20000]);
    let dir = tempfile::tempdir().unwrap();
    let (outcome, session) = run_in(&dir, &doc, &params, &tools, &MockClock::new()).await;

    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(capture_durations(&tools).len(), 2);
    assert_eq!(session["report"]["total_frames"], json!(0.0));
    assert_eq!(session["report"]["window_over"], json!(true));
}

#[tokio::test]
async fn test_golden_sky_flat_resumes_the_current_filter_without_recapturing() {
    let doc = make_doc(crate::document::corpus::golden_sky_flat());
    let params = sky_flat_params(&doc, json!({ "filters": [{ "name": "L", "count": 2 }] }));
    let dir = tempfile::tempdir().unwrap();

    // First life: one counted frame, then the next capture dies with
    // the run (an rp-outage-shaped failure). The blackboard keeps
    // flat_count = 1 under the L index marker.
    let first = MockTools::scripted(vec![
        Ok(json!({ "max_adu": 65535, "exposure_min": "1ms", "exposure_max": "60s" })),
        Ok(json!({ "status": "ok" })),                         // unpark
        Ok(json!({ "status": "ok" })),                         // set_tracking on
        Ok(json!({ "cameras": ["cam"] })),                     // start_cooldown
        Ok(json!({ "lst_hours": 23.75 })),                     // get_local_sidereal_time
        Ok(json!({ "status": "ok" })),                         // slew
        Ok(json!({ "status": "ok" })),                         // set_tracking off
        Ok(json!({ "status": "ok" })),                         // set_filter L
        Ok(json!({ "document_id": "doc-1" })),                 // capture 1
        Ok(json!({ "median_adu": 32767 })),                    // in-band → counted
        Err(ToolCallError::Failed("rp went away".to_owned())), // capture 2
        Ok(json!({ "cameras": ["cam"] })),                     // start_warmup (finally)
    ]);
    let (outcome, session) = run_in(&dir, &doc, &params, &first, &MockClock::new()).await;
    assert!(matches!(outcome, RunOutcome::Failed(_)));
    assert_eq!(session["flat_count"], json!(1.0));

    // Second life, same blackboard: the pointing re-runs idempotently,
    // and the index marker keeps the count — exactly one more frame is
    // captured, not two.
    let second = sky_flat_tools(vec![32767]);
    let (outcome, session) = run_in(&dir, &doc, &params, &second, &MockClock::new()).await;
    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(
        capture_durations(&second).len(),
        1,
        "the resumed run must not repeat the counted frame"
    );
    assert_eq!(session["report"]["total_frames"], json!(2.0));
}

fn deep_sky_params(doc: &Document, overrides: Value) -> Value {
    let mut supplied = json!({ "train_id": "main" });
    if let (Some(base), Some(extra)) = (supplied.as_object_mut(), overrides.as_object()) {
        for (k, v) in extra {
            base.insert(k.clone(), v.clone());
        }
    }
    bind_parameters(&doc.parameters, Some(&supplied)).unwrap()
}

/// `get_next_target` result carrying an exposure plan, in rp's current
/// wire shape: the target's coordinate nests as `coord`, the plan
/// entry to shoot next is a nested `exposure` object (`{filter,
/// duration_secs}`), or `null` when the target defines no plan, and
/// `position_angle_degrees` is the effective framing angle (rp.md §
/// Target Store → Position angle). A null `duration_secs` argument is
/// the "no plan entry" signal — it yields a null `exposure`, matching
/// the workflow's `result.exposure != null` fallback guard.
fn planned_recommendation(filter: Value, duration_secs: Value) -> Value {
    let exposure = if duration_secs.is_null() {
        Value::Null
    } else {
        json!({ "filter": filter, "duration_secs": duration_secs })
    };
    json!({
        "target": {
            "name": "M31",
            "coord": { "ra_hours": 0.7, "dec_degrees": 41.0 },
            "min_altitude_degrees": null
        },
        "reason": "best_transiting_candidate",
        "exposure": exposure,
        "position_angle_degrees": 121.25
    })
}

#[tokio::test]
async fn test_golden_deep_sky_captures_at_the_planner_duration_and_filter() {
    // The planner recommends Red at 120 s; the `exposure_duration` parameter
    // stays at its 300s default and `filter` at "". The acquisition
    // must set the plan's filter and the capture must run at the
    // plan's duration (120 s → "2m" through the humantime() builtin).
    let doc = make_doc(crate::document::corpus::golden_deep_sky());
    let params = deep_sky_params(
        &doc,
        json!({
            "focus": false,
            "centering": false,
            "max_frames": 1,
            "park_on_finish": false
        }),
    );
    let tools = MockTools::new(|_, tool, _| match tool {
        "unpark" | "set_tracking" | "start_cooldown" | "start_warmup" | "slew" | "set_filter"
        | "record_exposure" => Ok(json!({})),
        "get_next_target" => Ok(planned_recommendation(json!("Red"), json!(120))),
        "capture" => Ok(json!({ "image_path": "/tmp/light.fits", "document_id": "doc-1" })),
        other => panic!("unexpected tool call `{other}`"),
    });
    let dir = tempfile::tempdir().unwrap();
    let (outcome, _) = run_in(&dir, &doc, &params, &tools, &MockClock::new()).await;

    assert_eq!(outcome, RunOutcome::Completed);
    let calls = tools.calls();
    let set_filter = calls
        .iter()
        .find(|(name, _)| name == "set_filter")
        .expect("set_filter must be called for a plan that names a filter");
    assert_eq!(
        set_filter.1["train_id"], "main",
        "the filter change must be train-addressed"
    );
    assert_eq!(set_filter.1["filter_name"], "Red");
    let capture = calls
        .iter()
        .find(|(name, _)| name == "capture")
        .expect("capture must be called");
    assert_eq!(
        capture.1["duration"], "2m",
        "the capture must run at the plan's 120 s, not params.exposure_duration"
    );
}

#[tokio::test]
async fn test_golden_deep_sky_rotates_to_the_planner_angle_when_enabled() {
    // With rotate: true the acquisition issues a train-addressed
    // move_rotator to the recommendation's effective position angle,
    // after the slew and before the capture; get_next_target itself is
    // train-addressed so rp can resolve the per-train default layer
    // (rp.md § Target Store → Position angle).
    let doc = make_doc(crate::document::corpus::golden_deep_sky());
    let params = deep_sky_params(
        &doc,
        json!({
            "rotate": true,
            "focus": false,
            "centering": false,
            "max_frames": 1,
            "park_on_finish": false
        }),
    );
    let tools = MockTools::new(|_, tool, _| match tool {
        "unpark" | "set_tracking" | "start_cooldown" | "start_warmup" | "slew" | "set_filter"
        | "move_rotator" | "record_exposure" => Ok(json!({})),
        "get_next_target" => Ok(planned_recommendation(json!("Red"), json!(120))),
        "capture" => Ok(json!({ "image_path": "/tmp/light.fits", "document_id": "doc-1" })),
        other => panic!("unexpected tool call `{other}`"),
    });
    let dir = tempfile::tempdir().unwrap();
    let (outcome, _) = run_in(&dir, &doc, &params, &tools, &MockClock::new()).await;

    assert_eq!(outcome, RunOutcome::Completed);
    let calls = tools.calls();
    let next_target = calls
        .iter()
        .find(|(name, _)| name == "get_next_target")
        .expect("get_next_target must be called");
    assert_eq!(
        next_target.1["train_id"], "main",
        "get_next_target must be train-addressed for the per-train angle layer"
    );
    let rotate_idx = calls
        .iter()
        .position(|(name, _)| name == "move_rotator")
        .expect("move_rotator must be called when rotate is enabled");
    assert_eq!(calls[rotate_idx].1["train_id"], "main");
    assert_eq!(calls[rotate_idx].1["angle"], 121.25);
    let slew_idx = calls
        .iter()
        .position(|(name, _)| name == "slew")
        .expect("slew must be called");
    let capture_idx = calls
        .iter()
        .position(|(name, _)| name == "capture")
        .expect("capture must be called");
    assert!(
        slew_idx < rotate_idx && rotate_idx < capture_idx,
        "the rotator must move after the slew and before the capture; got slew {slew_idx}, \
         move_rotator {rotate_idx}, capture {capture_idx}"
    );
}

#[tokio::test]
async fn test_golden_deep_sky_does_not_rotate_by_default() {
    // rotate defaults to false (rotator-less rigs): the train default
    // angle is config-documented reality and nothing moves. The mock
    // panics on any unexpected tool, so a stray move_rotator would
    // fail the run outright; the explicit assertion documents intent.
    let doc = make_doc(crate::document::corpus::golden_deep_sky());
    let params = deep_sky_params(
        &doc,
        json!({
            "focus": false,
            "centering": false,
            "max_frames": 1,
            "park_on_finish": false
        }),
    );
    let tools = MockTools::new(|_, tool, _| match tool {
        "unpark" | "set_tracking" | "start_cooldown" | "start_warmup" | "slew" | "set_filter"
        | "record_exposure" => Ok(json!({})),
        "get_next_target" => Ok(planned_recommendation(json!("Red"), json!(120))),
        "capture" => Ok(json!({ "image_path": "/tmp/light.fits", "document_id": "doc-1" })),
        other => panic!("unexpected tool call `{other}`"),
    });
    let dir = tempfile::tempdir().unwrap();
    let (outcome, _) = run_in(&dir, &doc, &params, &tools, &MockClock::new()).await;

    assert_eq!(outcome, RunOutcome::Completed);
    assert!(
        !tools.calls().iter().any(|(name, _)| name == "move_rotator"),
        "rotate defaults to off — no rotator motion"
    );
}

#[tokio::test]
async fn test_golden_deep_sky_continues_when_the_rotator_move_fails() {
    // A failed rotator move degrades framing, not the night: the
    // try/catch logs and the capture still happens (tenet: robustness).
    let doc = make_doc(crate::document::corpus::golden_deep_sky());
    let params = deep_sky_params(
        &doc,
        json!({
            "rotate": true,
            "focus": false,
            "centering": false,
            "max_frames": 1,
            "park_on_finish": false
        }),
    );
    let tools = MockTools::new(|_, tool, _| match tool {
        "unpark" | "set_tracking" | "start_cooldown" | "start_warmup" | "slew" | "set_filter"
        | "record_exposure" => Ok(json!({})),
        "move_rotator" => Err(ToolCallError::Failed("rotator jammed".to_owned())),
        "get_next_target" => Ok(planned_recommendation(json!("Red"), json!(120))),
        "capture" => Ok(json!({ "image_path": "/tmp/light.fits", "document_id": "doc-1" })),
        other => panic!("unexpected tool call `{other}`"),
    });
    let dir = tempfile::tempdir().unwrap();
    let (outcome, _) = run_in(&dir, &doc, &params, &tools, &MockClock::new()).await;

    assert_eq!(outcome, RunOutcome::Completed);
    let calls = tools.calls();
    assert!(calls.iter().any(|(name, _)| name == "move_rotator"));
    assert!(
        calls.iter().any(|(name, _)| name == "capture"),
        "the capture must still happen after a failed rotator move"
    );
}

#[tokio::test]
async fn test_golden_deep_sky_falls_back_to_the_exposure_duration_parameter_without_a_plan() {
    // A target without `exposures[]` recommends with a null plan; the
    // capture falls back to `params.exposure_duration` (default 300s) and no
    // filter change happens.
    let doc = make_doc(crate::document::corpus::golden_deep_sky());
    let params = deep_sky_params(
        &doc,
        json!({
            "focus": false,
            "centering": false,
            "max_frames": 1,
            "park_on_finish": false
        }),
    );
    let tools = MockTools::new(|_, tool, _| match tool {
        "unpark" | "set_tracking" | "start_cooldown" | "start_warmup" | "slew"
        | "record_exposure" => Ok(json!({})),
        "get_next_target" => Ok(planned_recommendation(Value::Null, Value::Null)),
        "capture" => Ok(json!({ "image_path": "/tmp/light.fits", "document_id": "doc-1" })),
        other => panic!("unexpected tool call `{other}`"),
    });
    let dir = tempfile::tempdir().unwrap();
    let (outcome, _) = run_in(&dir, &doc, &params, &tools, &MockClock::new()).await;

    assert_eq!(outcome, RunOutcome::Completed);
    let calls = tools.calls();
    assert!(
        !calls.iter().any(|(name, _)| name == "set_filter"),
        "no plan filter and no filter parameter ⇒ no set_filter call"
    );
    let capture = calls
        .iter()
        .find(|(name, _)| name == "capture")
        .expect("capture must be called");
    assert_eq!(capture.1["duration"], "300s");
}

#[tokio::test]
async fn test_golden_deep_sky_respects_an_unfiltered_plan_over_the_filter_parameter() {
    // A plan whose first entry is explicitly unfiltered (filter null,
    // duration_secs set) must image unfiltered — the `filter`
    // parameter is a fallback for a *missing* plan, not a default the
    // plan merges with. Falling back here would force a filter change
    // the plan deliberately avoided.
    let doc = make_doc(crate::document::corpus::golden_deep_sky());
    let params = deep_sky_params(
        &doc,
        json!({
            "focus": false,
            "centering": false,
            "filter": "Red",
            "max_frames": 1,
            "park_on_finish": false
        }),
    );
    let tools = MockTools::new(|_, tool, _| match tool {
        "unpark" | "set_tracking" | "start_cooldown" | "start_warmup" | "slew"
        | "record_exposure" => Ok(json!({})),
        "get_next_target" => Ok(planned_recommendation(Value::Null, json!(60))),
        "capture" => Ok(json!({ "image_path": "/tmp/light.fits", "document_id": "doc-1" })),
        other => panic!("unexpected tool call `{other}`"),
    });
    let dir = tempfile::tempdir().unwrap();
    let (outcome, _) = run_in(&dir, &doc, &params, &tools, &MockClock::new()).await;

    assert_eq!(outcome, RunOutcome::Completed);
    let calls = tools.calls();
    assert!(
        !calls.iter().any(|(name, _)| name == "set_filter"),
        "an explicitly unfiltered plan must not fall back to params.filter"
    );
    let capture = calls
        .iter()
        .find(|(name, _)| name == "capture")
        .expect("capture must be called");
    assert_eq!(capture.1["duration"], "1m");
}

#[tokio::test]
async fn test_golden_deep_sky_fails_before_the_slew_when_the_train_has_no_filter_wheel() {
    // A filter arriving in the planner's exposure plan for a wheelless
    // train must fail the session loudly instead of silently imaging
    // unfiltered: rp's train-addressed set_filter errors, the call is
    // not try-wrapped, and the filter change precedes acquisition.
    let doc = make_doc(crate::document::corpus::golden_deep_sky());
    let params = deep_sky_params(
        &doc,
        json!({
            "focus": false,
            "centering": false,
            "max_frames": 1,
            "park_on_finish": false
        }),
    );
    let tools = MockTools::new(|_, tool, _| match tool {
        "unpark" | "set_tracking" | "start_cooldown" | "start_warmup" => Ok(json!({})),
        "get_next_target" => Ok(planned_recommendation(json!("Red"), json!(120))),
        "set_filter" => Err(ToolCallError::Failed(
            "train 'main' has no filter wheel".to_owned(),
        )),
        other => panic!("unexpected tool call `{other}` after the failed filter change"),
    });
    let dir = tempfile::tempdir().unwrap();
    let (outcome, _) = run_in(&dir, &doc, &params, &tools, &MockClock::new()).await;

    let error = failure(outcome);
    assert!(
        error.message.contains("train 'main' has no filter wheel"),
        "{}",
        error.message
    );
    assert_eq!(
        tools.call_names(),
        vec![
            "unpark",
            "set_tracking",
            "start_cooldown",
            "get_next_target",
            "set_filter",
            "start_warmup"
        ],
        "the failure must land before the slew — a target the rig \
         cannot filter for must not move the mount; the finally still \
         warms the camera"
    );
}

#[tokio::test]
async fn test_golden_deep_sky_ends_the_session_when_the_planner_says_end_of_session() {
    // The planner owns dawn (rp #465): an `end_of_session` reason ends
    // the session on the spot — no frames-captured heuristic, no slew,
    // no capture, no 5-minute wait.
    let doc = make_doc(crate::document::corpus::golden_deep_sky());
    let params = deep_sky_params(
        &doc,
        json!({
            "focus": false,
            "centering": false,
            "max_frames": 1,
            "park_on_finish": false
        }),
    );
    let tools = MockTools::new(|_, tool, _| match tool {
        "unpark" | "set_tracking" | "start_cooldown" | "start_warmup" => Ok(json!({})),
        "get_next_target" => Ok(json!({
            "target": null,
            "reason": "end_of_session",
            "exposure": null
        })),
        other => panic!("unexpected tool call `{other}` after end_of_session"),
    });
    let dir = tempfile::tempdir().unwrap();
    let (outcome, _) = run_in(&dir, &doc, &params, &tools, &MockClock::new()).await;

    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(
        tools.call_names(),
        vec![
            "unpark",
            "set_tracking",
            "start_cooldown",
            "get_next_target",
            "start_warmup"
        ],
        "end_of_session must terminate the dispatch loop immediately"
    );
}

#[tokio::test]
async fn test_golden_deep_sky_follows_plan_rotation_and_records_each_frame() {
    // rp #463: the planner rotates within a target's plan as recorded
    // goals complete. Pass 1 recommends Red@120s, pass 2 Blue@60s on
    // the SAME target, pass 3 end_of_session. The document must
    // re-derive filter/duration on every pass — the filter changes
    // mid-target with no re-slew — and record each light frame with
    // the filter it was actually taken through.
    let doc = make_doc(crate::document::corpus::golden_deep_sky());
    let params = deep_sky_params(
        &doc,
        json!({
            "focus": false,
            "centering": false,
            "max_frames": 0,
            "park_on_finish": false
        }),
    );
    let planner_calls = std::sync::Arc::new(Mutex::new(0u32));
    let counter = planner_calls.clone();
    let tools = MockTools::new(move |_, tool, _| match tool {
        "unpark" | "set_tracking" | "start_cooldown" | "start_warmup" | "slew" | "set_filter"
        | "record_exposure" => Ok(json!({})),
        "capture" => Ok(json!({ "image_path": "/tmp/light.fits", "document_id": "doc-1" })),
        "get_next_target" => {
            let mut n = counter.lock().unwrap();
            *n += 1;
            match *n {
                1 => Ok(planned_recommendation(json!("Red"), json!(120))),
                2 => Ok(planned_recommendation(json!("Blue"), json!(60))),
                _ => Ok(json!({
                    "target": null,
                    "reason": "end_of_session",
                    "exposure": null
                })),
            }
        }
        other => panic!("unexpected tool call `{other}`"),
    });
    let dir = tempfile::tempdir().unwrap();
    let (outcome, _) = run_in(&dir, &doc, &params, &tools, &MockClock::new()).await;

    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(
        tools.call_names(),
        vec![
            "unpark",
            "set_tracking",
            "start_cooldown",
            // Pass 1: new filter + new target.
            "get_next_target",
            "set_filter",
            "slew",
            "capture",
            "record_exposure",
            // Pass 2: the plan rotated — filter change only, no slew.
            "get_next_target",
            "set_filter",
            "capture",
            "record_exposure",
            // Pass 3: every goal met.
            "get_next_target",
            // The finally warms the camera however the run ended.
            "start_warmup",
        ],
        "plan rotation must change the filter without re-acquiring the target"
    );
    let calls = tools.calls();
    let set_filters: Vec<&Value> = calls
        .iter()
        .filter(|(name, _)| name == "set_filter")
        .map(|(_, args)| &args["filter_name"])
        .collect();
    assert_eq!(set_filters, [&json!("Red"), &json!("Blue")]);
    let recorded: Vec<(&Value, &Value)> = calls
        .iter()
        .filter(|(name, _)| name == "record_exposure")
        .map(|(_, args)| (&args["target"], &args["filter"]))
        .collect();
    assert_eq!(
        recorded,
        [
            (&json!("M31"), &json!("Red")),
            (&json!("M31"), &json!("Blue"))
        ],
        "each frame must be recorded with the filter it was taken through"
    );
}

/// `run_in` with a pre-fed event intake, for the golden document's
/// trigger-wiring tests (the parameterless `run_doc_with_events`
/// cannot carry the required `train_id`).
async fn run_params_with_events(
    doc: &Document,
    params: &Value,
    tools: &MockTools,
    events: EventIntake,
) -> (RunOutcome, Value) {
    let dir = tempfile::tempdir().unwrap();
    let mut blackboard = Blackboard::load(dir.path().join("session.json"))
        .await
        .unwrap();
    let outcome = run(
        doc,
        params,
        &mut blackboard,
        tools,
        &MockClock::new(),
        events,
    )
    .await;
    let session = blackboard.value().clone();
    (outcome, session)
}

#[tokio::test]
async fn test_golden_deep_sky_guided_session_runs_the_guide_lifecycle() {
    // guide: true — guiding starts after acquisition (post-slew) and
    // before the first frame; dither_every: 2 dithers exactly once,
    // after the second recorded frame; shutdown stops guiding BEFORE
    // the park.
    let doc = make_doc(crate::document::corpus::golden_deep_sky());
    let params = deep_sky_params(
        &doc,
        json!({
            "focus": false,
            "centering": false,
            "guide": true,
            "dither_every": 2,
            "max_frames": 3,
            "park_on_finish": true
        }),
    );
    let tools = MockTools::new(|_, tool, _| match tool {
        "unpark" | "set_tracking" | "start_cooldown" | "start_warmup" | "slew"
        | "record_exposure" | "start_guiding" | "stop_guiding" | "dither" | "park" => Ok(json!({})),
        "get_next_target" => Ok(planned_recommendation(Value::Null, Value::Null)),
        "capture" => Ok(json!({ "image_path": "/tmp/light.fits", "document_id": "doc-1" })),
        other => panic!("unexpected tool call `{other}`"),
    });
    let dir = tempfile::tempdir().unwrap();
    let (outcome, _) = run_in(&dir, &doc, &params, &tools, &MockClock::new()).await;

    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(
        tools.call_names(),
        vec![
            "unpark",
            "set_tracking",
            "start_cooldown",
            // Pass 1: acquisition ends with the guide loop starting.
            "get_next_target",
            "slew",
            "start_guiding",
            "capture",
            "record_exposure",
            // Pass 2: the dither lands after the second recorded frame.
            "get_next_target",
            "capture",
            "record_exposure",
            "dither",
            // Pass 3: frame budget reached; only one frame since the
            // dither, so no second dither.
            "get_next_target",
            "capture",
            "record_exposure",
            // Shutdown: guiding stops before the mount parks.
            "stop_guiding",
            "park",
            "start_warmup",
        ],
        "the guided cadence must be start-after-acquisition, \
         dither-every-2, stop-before-park"
    );
}

#[tokio::test]
async fn test_golden_deep_sky_start_guiding_failure_retries_then_fails_the_session() {
    // A guided session that cannot guide must fail loudly after the
    // 3-attempt retry instead of silently capturing trailed frames
    // all night.
    let doc = make_doc(crate::document::corpus::golden_deep_sky());
    let params = deep_sky_params(
        &doc,
        json!({
            "focus": false,
            "centering": false,
            "guide": true,
            "max_frames": 1,
            "park_on_finish": false
        }),
    );
    let tools = MockTools::new(|_, tool, _| match tool {
        "unpark" | "set_tracking" | "start_cooldown" | "start_warmup" | "slew" => Ok(json!({})),
        "get_next_target" => Ok(planned_recommendation(Value::Null, Value::Null)),
        "start_guiding" => Err(ToolCallError::Failed("PHD2 unreachable".to_owned())),
        // The retry back-off advances the mock clock past the
        // meridian poll's interval; the poll lands at the finally's
        // safe point (the warm-up) and must be answered.
        "get_meridian_status" => Ok(json!({ "time_to_flip_seconds": null })),
        other => panic!("unexpected tool call `{other}` after guiding failed to start"),
    });
    let dir = tempfile::tempdir().unwrap();
    let (outcome, _) = run_in(&dir, &doc, &params, &tools, &MockClock::new()).await;

    let error = failure(outcome);
    assert_eq!(
        error.message,
        "tool `start_guiding` failed after 3 attempts: PHD2 unreachable"
    );
    assert_eq!(
        tools
            .call_names()
            .iter()
            .filter(|name| *name == "start_guiding")
            .count(),
        3,
        "start_guiding must be retried exactly 3 times before failing"
    );
}

#[tokio::test]
async fn test_golden_deep_sky_degraded_event_runs_a_guide_only_auto_focus() {
    // guide_focus_degraded names the guiding train; the trigger runs
    // the guide-only metric sweep on exactly that train — the document
    // needs no guide-train parameter of its own.
    let doc = make_doc(crate::document::corpus::golden_deep_sky());
    let params = deep_sky_params(
        &doc,
        json!({
            "focus": false,
            "centering": false,
            "guide": true,
            "max_frames": 1,
            "park_on_finish": false
        }),
    );
    let tools = MockTools::new(|_, tool, _| match tool {
        "unpark" | "set_tracking" | "start_cooldown" | "start_warmup" | "slew"
        | "record_exposure" | "start_guiding" | "stop_guiding" => Ok(json!({})),
        "get_next_target" => Ok(planned_recommendation(Value::Null, Value::Null)),
        "capture" => Ok(json!({ "image_path": "/tmp/light.fits", "document_id": "doc-1" })),
        "auto_focus" => Ok(json!({ "best_hfd": 2.1 })),
        other => panic!("unexpected tool call `{other}`"),
    });
    let events = buffered_events(&[(
        "guide_focus_degraded",
        json!({ "train_id": "guide", "baseline_hfd": 2.0, "current_hfd": 3.0, "window": 10 }),
    )]);
    let (outcome, _) = run_params_with_events(&doc, &params, &tools, events).await;

    assert_eq!(outcome, RunOutcome::Completed);
    let calls = tools.calls();
    let auto_focus = calls
        .iter()
        .find(|(name, _)| name == "auto_focus")
        .expect("guide_focus_degraded must fire the guide-only auto_focus");
    assert_eq!(
        auto_focus.1,
        json!({ "train_id": "guide" }),
        "the sweep must address the event's guiding train, nothing else"
    );
}

#[tokio::test]
async fn test_golden_deep_sky_escalation_event_runs_the_full_refocus_train() {
    let doc = make_doc(crate::document::corpus::golden_deep_sky());
    let params = deep_sky_params(
        &doc,
        json!({
            "focus": false,
            "centering": false,
            "guide": true,
            "max_frames": 1,
            "park_on_finish": false
        }),
    );
    let tools = MockTools::new(|_, tool, _| match tool {
        "unpark" | "set_tracking" | "start_cooldown" | "start_warmup" | "slew"
        | "record_exposure" | "start_guiding" | "stop_guiding" => Ok(json!({})),
        "get_next_target" => Ok(planned_recommendation(Value::Null, Value::Null)),
        "capture" => Ok(json!({ "image_path": "/tmp/light.fits", "document_id": "doc-1" })),
        "refocus_train" => Ok(json!({ "steps": [] })),
        other => panic!("unexpected tool call `{other}`"),
    });
    let events = buffered_events(&[(
        "guide_focus_escalation",
        json!({ "train_id": "guide", "baseline_hfd": 2.0, "current_hfd": 3.0 }),
    )]);
    let (outcome, _) = run_params_with_events(&doc, &params, &tools, events).await;

    assert_eq!(outcome, RunOutcome::Completed);
    let calls = tools.calls();
    let refocus = calls
        .iter()
        .find(|(name, _)| name == "refocus_train")
        .expect("guide_focus_escalation must fire refocus_train");
    assert_eq!(
        refocus.1,
        json!({ "train_id": "guide", "reason": "guide_focus_escalation" })
    );
}

#[tokio::test]
async fn test_golden_deep_sky_shutdown_stop_failure_keeps_the_flag_and_still_parks() {
    // A failed stop_guiding is logged, not fatal — but it must NOT
    // clear session.guiding: the blackboard would otherwise claim a
    // stopped loop the guider still runs, and later stop attempts
    // would be skipped. The park still happens.
    let doc = make_doc(crate::document::corpus::golden_deep_sky());
    let params = deep_sky_params(
        &doc,
        json!({
            "focus": false,
            "centering": false,
            "guide": true,
            "max_frames": 1,
            "park_on_finish": true
        }),
    );
    let tools = MockTools::new(|_, tool, _| match tool {
        "unpark" | "set_tracking" | "start_cooldown" | "start_warmup" | "slew"
        | "record_exposure" | "start_guiding" | "park" => Ok(json!({})),
        "get_next_target" => Ok(planned_recommendation(Value::Null, Value::Null)),
        "capture" => Ok(json!({ "image_path": "/tmp/light.fits", "document_id": "doc-1" })),
        "stop_guiding" => Err(ToolCallError::Failed("PHD2 went away".to_owned())),
        other => panic!("unexpected tool call `{other}`"),
    });
    let dir = tempfile::tempdir().unwrap();
    let (outcome, session) = run_in(&dir, &doc, &params, &tools, &MockClock::new()).await;

    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(
        session["guiding"],
        json!(true),
        "a failed stop must not pretend the loop stopped"
    );
    assert_eq!(
        tools.call_names().iter().rev().nth(1).map(String::as_str),
        Some("park"),
        "the park still happens after the logged stop failure (before \
         the finally's warm-up)"
    );
}

#[tokio::test]
async fn test_golden_deep_sky_recovery_clears_the_flip_restart_flag() {
    // A crash between the flip trigger's stop-guiding and its restart
    // leaves session.flip_restart_guiding persisted as true; the
    // recovery branch must clear it, or a later flip on the resumed
    // run would restart guiding the invocation never asked for.
    let doc = make_doc(crate::document::corpus::golden_deep_sky());
    let mut params = deep_sky_params(
        &doc,
        json!({
            "focus": false,
            "centering": false,
            "max_frames": 1,
            "park_on_finish": false
        }),
    );
    // The engine injects `_recovery` on a recovery invocation
    // (routes.rs); the exec harness emulates that injection.
    params["_recovery"] = json!({ "reason": "safety_interruption" });
    let tools = MockTools::new(|_, tool, _| match tool {
        "unpark" | "set_tracking" | "start_cooldown" | "start_warmup" | "slew"
        | "record_exposure" => Ok(json!({})),
        "get_next_target" => Ok(planned_recommendation(Value::Null, Value::Null)),
        "capture" => Ok(json!({ "image_path": "/tmp/light.fits", "document_id": "doc-1" })),
        other => panic!("unexpected tool call `{other}`"),
    });
    let dir = tempfile::tempdir().unwrap();
    {
        let mut blackboard = Blackboard::load(dir.path().join("session.json"))
            .await
            .unwrap();
        blackboard
            .set_path(&["flip_restart_guiding".to_owned()], json!(true))
            .unwrap();
        blackboard.persist().await.unwrap();
    }
    let (outcome, session) = run_in(&dir, &doc, &params, &tools, &MockClock::new()).await;
    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(
        session["flip_restart_guiding"],
        json!(false),
        "recovery must clear the crashed flip's restart flag"
    );
}

#[tokio::test]
async fn test_golden_deep_sky_watch_triggers_stay_silent_for_a_null_train_id() {
    // The watch legally emits train_id: null when rp has no guiding
    // train configured; the triggers must stay silent rather than
    // spam a doomed null-addressed sweep into the catch-log once per
    // event.
    let doc = make_doc(crate::document::corpus::golden_deep_sky());
    let params = deep_sky_params(
        &doc,
        json!({
            "focus": false,
            "centering": false,
            "guide": true,
            "max_frames": 1,
            "park_on_finish": false
        }),
    );
    let tools = MockTools::new(|_, tool, _| match tool {
        "unpark" | "set_tracking" | "start_cooldown" | "start_warmup" | "slew"
        | "record_exposure" | "start_guiding" | "stop_guiding" => Ok(json!({})),
        "get_next_target" => Ok(planned_recommendation(Value::Null, Value::Null)),
        "capture" => Ok(json!({ "image_path": "/tmp/light.fits", "document_id": "doc-1" })),
        other => panic!("unexpected tool call `{other}` — a null train_id must not fire a sweep"),
    });
    let events = buffered_events(&[
        (
            "guide_focus_degraded",
            json!({ "train_id": null, "baseline_hfd": 2.0, "current_hfd": 3.0, "window": 10 }),
        ),
        (
            "guide_focus_escalation",
            json!({ "train_id": null, "baseline_hfd": 2.0, "current_hfd": 3.0 }),
        ),
    ]);
    let (outcome, _) = run_params_with_events(&doc, &params, &tools, events).await;
    assert_eq!(outcome, RunOutcome::Completed);
}

#[tokio::test]
async fn test_golden_deep_sky_watch_triggers_stay_silent_without_guide() {
    // guide defaults to false: the watch events must not fire the
    // triggers — an unguided document did not start the loop the
    // events describe, so reacting to them is out of its scope.
    let doc = make_doc(crate::document::corpus::golden_deep_sky());
    let params = deep_sky_params(
        &doc,
        json!({
            "focus": false,
            "centering": false,
            "max_frames": 1,
            "park_on_finish": false
        }),
    );
    let tools = MockTools::new(|_, tool, _| match tool {
        "unpark" | "set_tracking" | "start_cooldown" | "start_warmup" | "slew"
        | "record_exposure" => Ok(json!({})),
        "get_next_target" => Ok(planned_recommendation(Value::Null, Value::Null)),
        "capture" => Ok(json!({ "image_path": "/tmp/light.fits", "document_id": "doc-1" })),
        other => panic!("unexpected tool call `{other}` — the watch triggers must stay silent"),
    });
    let events = buffered_events(&[
        (
            "guide_focus_degraded",
            json!({ "train_id": "guide", "baseline_hfd": 2.0, "current_hfd": 3.0, "window": 10 }),
        ),
        (
            "guide_focus_escalation",
            json!({ "train_id": "guide", "baseline_hfd": 2.0, "current_hfd": 3.0 }),
        ),
    ]);
    let (outcome, _) = run_params_with_events(&doc, &params, &tools, events).await;
    assert_eq!(outcome, RunOutcome::Completed);
}
