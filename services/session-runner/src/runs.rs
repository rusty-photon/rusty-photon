//! Runs — the `POST /runs` lifecycle (design § Runs, § Safety Behavior).
//!
//! Three pieces: the [`RunRegistry`] the routes read and stop through;
//! the [`RunManifest`] written next to the blackboard, which a restarted
//! service resumes from (§ Self-resume on startup); and the supervisor
//! ([`supervise`]) that owns one run end to end — connect and validate,
//! execute, pause on a safety stop or an `rp` outage, wait the pause
//! out *in this process*, re-execute from the root, and record the
//! outcome where the run started. Nothing exits to await a
//! re-invocation, because nothing re-invokes (mcp-sessionless D6/D9).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::blackboard::Blackboard;
use crate::config::{Config, RpConnection};
use crate::document::{bind_parameters, resolve_workflow_path, validate_against_catalog, Document};
use crate::engine::{
    run_with_stop, PauseReason, RunOutcome, SystemClock, ToolCallError, ToolClient,
};
use crate::events;
use crate::mcp_client::McpClient;

/// The service state behind every route: the configuration and the run
/// registry.
pub struct AppState {
    pub config: Config,
    pub runs: RunRegistry,
}

impl AppState {
    #[must_use]
    pub fn new(config: Config) -> Self {
        Self {
            config,
            runs: RunRegistry::default(),
        }
    }
}

// --- the registry -----------------------------------------------------------

/// Where a run is in its lifecycle (`GET /runs/{id}`'s `state`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RunState {
    Running,
    Paused,
    Complete,
    Failed,
    Stopped,
}

impl RunState {
    /// Running or paused: the run still owns the rig.
    #[must_use]
    pub const fn is_active(self) -> bool {
        matches!(self, Self::Running | Self::Paused)
    }
}

/// One run as the routes report it.
#[derive(Clone, Debug, Serialize)]
pub struct RunRecord {
    pub run_id: String,
    pub session_id: String,
    pub workflow: String,
    pub state: RunState,
    /// `safety` | `rp_outage` while paused.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paused_reason: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paused_detail: Option<String>,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    /// The completion payload once the run has ended.
    pub outcome: Option<Value>,
    #[serde(skip)]
    stop: CancellationToken,
}

impl RunRecord {
    /// The run's stop token.
    #[must_use]
    pub fn stop_token(&self) -> CancellationToken {
        self.stop.clone()
    }
}

/// Why a stop request was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum StopError {
    NotFound,
    AlreadyEnded(RunState),
}

/// Every run this process knows, in start order. One run at a time
/// (design § Runs): [`RunRegistry::start`] refuses a second active run.
#[derive(Default)]
pub struct RunRegistry {
    runs: Mutex<Vec<RunRecord>>,
}

impl RunRegistry {
    /// Register a new running run and hand back its stop token.
    ///
    /// # Errors
    ///
    /// The id of the run that is still active — one run at a time.
    pub fn start(
        &self,
        run_id: &str,
        session_id: &str,
        workflow: &str,
        started_at: DateTime<Utc>,
    ) -> Result<CancellationToken, String> {
        let mut runs = self.lock();
        if let Some(active) = runs.iter().find(|r| r.state.is_active()) {
            let active = active.run_id.clone();
            drop(runs);
            return Err(active);
        }
        let stop = CancellationToken::new();
        runs.push(RunRecord {
            run_id: run_id.to_owned(),
            session_id: session_id.to_owned(),
            workflow: workflow.to_owned(),
            state: RunState::Running,
            paused_reason: None,
            paused_detail: None,
            started_at,
            ended_at: None,
            outcome: None,
            stop: stop.clone(),
        });
        drop(runs);
        Ok(stop)
    }

    /// The id of the run that is running or paused, if any.
    #[must_use]
    pub fn active(&self) -> Option<String> {
        self.lock()
            .iter()
            .find(|r| r.state.is_active())
            .map(|r| r.run_id.clone())
    }

    #[must_use]
    pub fn get(&self, run_id: &str) -> Option<RunRecord> {
        self.lock().iter().find(|r| r.run_id == run_id).cloned()
    }

    /// Every run, newest first.
    #[must_use]
    pub fn list(&self) -> Vec<RunRecord> {
        self.lock().iter().rev().cloned().collect()
    }

    pub fn mark_running(&self, run_id: &str) {
        self.update(run_id, |r| {
            r.state = RunState::Running;
            r.paused_reason = None;
            r.paused_detail = None;
        });
    }

    pub fn mark_paused(&self, run_id: &str, reason: &'static str, detail: &str) {
        self.update(run_id, |r| {
            r.state = RunState::Paused;
            r.paused_reason = Some(reason);
            r.paused_detail = Some(detail.to_owned());
        });
    }

    pub fn mark_ended(&self, run_id: &str, state: RunState, outcome: Value, at: DateTime<Utc>) {
        self.update(run_id, |r| {
            r.state = state;
            r.paused_reason = None;
            r.paused_detail = None;
            r.ended_at = Some(at);
            r.outcome = Some(outcome);
        });
    }

    /// Fire a run's stop token.
    ///
    /// # Errors
    ///
    /// [`StopError::NotFound`] for an unknown id,
    /// [`StopError::AlreadyEnded`] for a run that is neither running nor
    /// paused.
    pub fn stop(&self, run_id: &str) -> Result<RunRecord, StopError> {
        let runs = self.lock();
        let record = runs
            .iter()
            .find(|r| r.run_id == run_id)
            .ok_or(StopError::NotFound)?
            .clone();
        drop(runs);
        if !record.state.is_active() {
            return Err(StopError::AlreadyEnded(record.state));
        }
        record.stop.cancel();
        Ok(record)
    }

    fn update(&self, run_id: &str, apply: impl FnOnce(&mut RunRecord)) {
        let mut runs = self.lock();
        if let Some(record) = runs.iter_mut().find(|r| r.run_id == run_id) {
            apply(record);
        } else {
            // Internal invariant: every supervisor was registered first.
            warn!(run_id, "run record missing; state change dropped");
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<RunRecord>> {
        // A poisoned lock means a panic mid-update; the records are
        // plain data, still consistent enough to read.
        self.runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

// --- the manifest -----------------------------------------------------------

/// What a run needs to resume after a `session-runner` restart: written
/// next to the blackboard before the first instruction, deleted with it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunManifest {
    pub run_id: String,
    pub session_id: String,
    pub workflow: String,
    /// The raw `params` object of the request — re-bound (and
    /// re-validated) on every resume.
    pub params: Value,
    pub started_at: DateTime<Utc>,
}

const MANIFEST_SUFFIX: &str = ".run.json";

impl RunManifest {
    /// `<state_dir>/<session_id>.run.json`.
    #[must_use]
    pub fn path(state_dir: &Path, session_id: &str) -> PathBuf {
        state_dir.join(format!("{session_id}{MANIFEST_SUFFIX}"))
    }

    /// The blackboard the run persists to.
    #[must_use]
    pub fn blackboard_path(&self, state_dir: &Path) -> PathBuf {
        state_dir.join(format!("{}.json", self.session_id))
    }

    /// Write the manifest with the workspace atomic-write pattern the
    /// blackboard uses (sibling temp file, fsync, rename, fsync the
    /// parent) — a crash or power loss must never leave a partial
    /// manifest, which self-resume would skip, silently losing the
    /// operator's run.
    ///
    /// # Errors
    ///
    /// The serialization or I/O error, with the path, when the file
    /// cannot be written.
    pub async fn write(&self, state_dir: &Path) -> Result<(), String> {
        let path = Self::path(state_dir, &self.session_id);
        let bytes = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        let target = path.clone();
        tokio::task::spawn_blocking(move || crate::blackboard::write_atomic(&target, &bytes))
            .await
            .map_err(|e| {
                format!(
                    "cannot write {}: write task join error: {e}",
                    path.display()
                )
            })?
            .map_err(|e| e.to_string())
    }

    /// Every manifest in `state_dir`, unreadable ones logged and skipped.
    pub async fn scan(state_dir: &Path) -> Vec<Self> {
        let mut found = Vec::new();
        let mut entries = match tokio::fs::read_dir(state_dir).await {
            Ok(entries) => entries,
            Err(e) => {
                warn!(dir = %state_dir.display(), error = %e, "cannot scan for run manifests");
                return found;
            }
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            let is_manifest = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(MANIFEST_SUFFIX));
            if !is_manifest {
                continue;
            }
            match tokio::fs::read(&path).await {
                Ok(bytes) => match serde_json::from_slice::<Self>(&bytes) {
                    Ok(manifest) => found.push(manifest),
                    Err(e) => {
                        warn!(path = %path.display(), error = %e, "unreadable run manifest; skipped");
                    }
                },
                Err(e) => {
                    warn!(path = %path.display(), error = %e, "cannot read run manifest; skipped");
                }
            }
        }
        found.sort_by_key(|m| m.started_at);
        found
    }
}

/// Delete a file that may already be gone; anything else is logged.
async fn remove_quietly(path: &Path, what: &str) {
    if let Err(e) = tokio::fs::remove_file(path).await {
        if e.kind() != std::io::ErrorKind::NotFound {
            warn!(path = %path.display(), error = %e, "could not delete the ended run's {what}");
        }
    }
}

// --- ids and payloads -------------------------------------------------------

/// A fresh run id.
#[must_use]
pub fn mint_run_id() -> String {
    format!("run-{}", uuid::Uuid::new_v4().simple())
}

/// A fresh session id: the UTC start time plus a short suffix, so two
/// runs started in the same second cannot share a blackboard.
#[must_use]
pub fn mint_session_id(now: DateTime<Utc>) -> String {
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    format!(
        "session-{}-{}",
        now.format("%Y%m%d-%H%M%S"),
        suffix.get(..4).unwrap_or("0000")
    )
}

/// The session id names the blackboard file — it must not traverse.
#[must_use]
pub fn valid_session_id(session_id: &str) -> bool {
    !(session_id.is_empty()
        || session_id.contains(['/', '\\'])
        || session_id == "."
        || session_id == "..")
}

/// `rp`'s HTTP origin, derived from its MCP endpoint.
///
/// The base for the default SSE stream URL (and the legacy completion
/// POST). Tolerates a trailing slash on the endpoint (`…/mcp/`), which
/// would otherwise survive the suffix strip and double the slash in
/// derived URLs.
#[must_use]
pub fn rp_base_url(mcp_server_url: &str) -> &str {
    let trimmed = mcp_server_url.trim_end_matches('/');
    trimmed.strip_suffix("/mcp").unwrap_or(trimmed)
}

/// The SSE endpoint for a run: the configured override, else derived
/// from `rp`'s MCP endpoint.
#[must_use]
pub fn events_url(config: &Config, mcp_server_url: &str) -> String {
    config
        .events_url
        .clone()
        .unwrap_or_else(|| format!("{}/api/events/subscribe", rp_base_url(mcp_server_url)))
}

/// The completion payload: `workflow` / `outcome` / `error`, plus any
/// values the document accumulated under `session.report.*` (fixed keys
/// win on a name collision).
#[must_use]
pub fn completion_result(
    workflow: &str,
    outcome: &str,
    error: Option<&str>,
    report: Option<&Value>,
) -> Value {
    let mut result = Map::new();
    result.insert("workflow".to_owned(), json!(workflow));
    result.insert("outcome".to_owned(), json!(outcome));
    if let Some(error) = error {
        result.insert("error".to_owned(), json!(error));
    }
    if let Some(Value::Object(report)) = report {
        for (key, value) in report {
            if !result.contains_key(key) {
                result.insert(key.clone(), value.clone());
            }
        }
    }
    Value::Object(result)
}

// --- the supervisor ---------------------------------------------------------

/// What `POST /runs` validated before spawning: the parsed document and
/// the live client it validated against, handed to the supervisor so the
/// first pass does not connect twice.
pub struct Launch {
    pub document: Document,
    pub mcp: McpClient,
}

/// First reconnect delay after `rp` is lost; doubles up to
/// [`MAX_BACKOFF`].
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// How a wait ended other than by the condition clearing.
enum WaitEnd {
    Stopped,
    /// `rp` went away during the wait.
    Unavailable(String),
}

/// How the connect loop ended other than with a client.
enum ConnectEnd {
    Stopped,
    /// `rp_outage_grace` spent; carries the last error.
    GraceSpent(String),
    /// `rp` is back but the document no longer passes validation.
    Invalid(String),
}

/// Own one run to its end (design § Runs → Lifecycle).
///
/// `launch` is the client and document `POST /runs` already validated;
/// `None` is a resume after a service restart, which waits for `rp`
/// unbounded and re-executes with `_recovery.reason = "engine_restart"`.
pub async fn supervise(
    state: Arc<AppState>,
    manifest: RunManifest,
    stop: CancellationToken,
    launch: Option<Launch>,
) {
    let run_id = manifest.run_id.clone();
    let Some(mcp_url) = state.config.mcp_server_url.clone() else {
        // `POST /runs` refuses without it; a manifest can only hit this
        // when the config changed under a restart.
        end_failed(
            &state,
            &manifest,
            "no mcp_server_url configured; the run cannot reach rp",
            None,
        )
        .await;
        return;
    };
    let connection = state.config.rp_connection();
    let mut launch = launch;
    let mut recovery: Option<&'static str> = if launch.is_none() {
        Some("engine_restart")
    } else {
        None
    };
    // The startup resume waits for rp unbounded — the boot order is not
    // the run's fault; every later reconnect is bounded by the grace.
    let mut grace_deadline: Option<Instant> = None;

    loop {
        let (document, mcp) = match launch.take() {
            Some(launch) => (launch.document, launch.mcp),
            None => match connect_with_backoff(
                &state,
                &manifest,
                &mcp_url,
                &connection,
                grace_deadline,
                &stop,
            )
            .await
            {
                Ok(pair) => pair,
                Err(ConnectEnd::Stopped) => {
                    end_stopped(&state, &manifest, None).await;
                    return;
                }
                Err(ConnectEnd::GraceSpent(message)) => {
                    let error = format!(
                        "rp unreachable for {}: {message}",
                        humantime::format_duration(state.config.rp_outage_grace)
                    );
                    end_failed(&state, &manifest, &error, None).await;
                    return;
                }
                Err(ConnectEnd::Invalid(message)) => {
                    end_failed(&state, &manifest, &message, None).await;
                    return;
                }
            },
        };

        // A resume never re-executes into a closed gate.
        if recovery.is_some() {
            match wait_for_safe(&state, &run_id, &mcp, &mcp_url, &connection, &stop).await {
                Ok(()) => {}
                Err(WaitEnd::Stopped) => {
                    end_stopped(&state, &manifest, None).await;
                    return;
                }
                Err(WaitEnd::Unavailable(message)) => {
                    state.runs.mark_paused(&run_id, "rp_outage", &message);
                    recovery = Some("rp_outage");
                    grace_deadline = Instant::now().checked_add(state.config.rp_outage_grace);
                    continue;
                }
            }
        }

        let pass = Pass {
            state: &state,
            manifest: &manifest,
            document: &document,
            mcp: &mcp,
            mcp_url: &mcp_url,
            connection: &connection,
            recovery,
            stop: &stop,
        };
        match pass.execute().await {
            PassEnd::Ended => return,
            PassEnd::Paused(reason) => {
                state
                    .runs
                    .mark_paused(&run_id, reason.name(), reason.detail());
                recovery = Some(match reason {
                    PauseReason::Safety(_) => "safety_interruption",
                    PauseReason::RpOutage(_) => "rp_outage",
                });
                grace_deadline = Instant::now().checked_add(state.config.rp_outage_grace);
                // Reconnect (rp may have restarted behind the pause) and
                // re-validate before re-executing.
            }
        }
    }
}

/// How one execution pass ended.
enum PassEnd {
    /// The run is over and recorded (complete, failed, or stopped).
    Ended,
    /// The run is paused; the supervisor waits and re-executes.
    Paused(PauseReason),
}

/// One execution of the document against a connected `rp`: bind the
/// parameters, load or replace the blackboard, subscribe to events, run.
struct Pass<'a> {
    state: &'a AppState,
    manifest: &'a RunManifest,
    document: &'a Document,
    mcp: &'a McpClient,
    mcp_url: &'a str,
    connection: &'a RpConnection,
    /// `Some` on a resume: the `_recovery.reason` the document sees.
    recovery: Option<&'static str>,
    stop: &'a CancellationToken,
}

impl Pass<'_> {
    async fn execute(self) -> PassEnd {
        let (state, manifest) = (self.state, self.manifest);
        let supplied = (!manifest.params.is_null()).then_some(&manifest.params);
        let params = match bind_parameters(&self.document.parameters, supplied) {
            Ok(mut params) => {
                if let Some(reason) = self.recovery {
                    info!(run_id = %manifest.run_id, reason, "resuming; re-executing from the root");
                    if let Value::Object(map) = &mut params {
                        map.insert("_recovery".to_owned(), json!({ "reason": reason }));
                    }
                }
                params
            }
            Err(issues) => {
                let message = format!("parameter validation failed: {}", issue_messages(&issues));
                end_failed(state, manifest, &message, None).await;
                return PassEnd::Ended;
            }
        };

        // Fresh start: a leftover file under this id is deleted eagerly.
        // Resume: reload what the paused run persisted.
        let blackboard_path = manifest.blackboard_path(&state.config.state_dir);
        let blackboard = if self.recovery.is_some() {
            Blackboard::load(blackboard_path).await
        } else {
            Blackboard::replace(blackboard_path).await
        };
        let mut blackboard = match blackboard {
            Ok(blackboard) => blackboard,
            Err(e) => {
                end_failed(state, manifest, &e.to_string(), None).await;
                return PassEnd::Ended;
            }
        };

        let intake =
            events::subscribe(events_url(&state.config, self.mcp_url), self.connection).await;
        state.runs.mark_running(&manifest.run_id);
        let outcome = run_with_stop(
            self.document,
            &params,
            &mut blackboard,
            self.mcp,
            &SystemClock,
            intake,
            self.stop,
        )
        .await;
        let report = blackboard.value().get("report").cloned();
        match outcome {
            RunOutcome::Completed => {
                end(
                    state,
                    manifest,
                    RunState::Complete,
                    completion_result(&manifest.workflow, "complete", None, report.as_ref()),
                )
                .await;
                PassEnd::Ended
            }
            RunOutcome::Failed(error) => {
                end_failed(state, manifest, &error.message, report.as_ref()).await;
                PassEnd::Ended
            }
            RunOutcome::Stopped => {
                end_stopped(state, manifest, report.as_ref()).await;
                PassEnd::Ended
            }
            RunOutcome::Paused(reason) => PassEnd::Paused(reason),
        }
    }
}

/// Connect to `rp`, fetch the catalog, and (re)validate the document
/// against it — retrying with backoff while `rp` is unreachable, until
/// `deadline` (`None`: unbounded) or the stop token.
async fn connect_with_backoff(
    state: &AppState,
    manifest: &RunManifest,
    mcp_url: &str,
    connection: &RpConnection,
    deadline: Option<Instant>,
    stop: &CancellationToken,
) -> Result<(Document, McpClient), ConnectEnd> {
    let mut delay = INITIAL_BACKOFF;
    loop {
        // Three outcomes: connected and valid; connected but invalid
        // (the run fails); not connected (retry).
        let message = match connect_and_validate(state, manifest, mcp_url, connection).await {
            Ok(pair) => return Ok(pair),
            Err(Attempt::Invalid(message)) => return Err(ConnectEnd::Invalid(message)),
            Err(Attempt::Unreachable(message)) => message,
        };
        debug!(run_id = %manifest.run_id, error = %message, "rp unreachable; will retry");
        state
            .runs
            .mark_paused(&manifest.run_id, "rp_outage", &message);
        let now = Instant::now();
        if deadline.is_some_and(|d| now >= d) {
            return Err(ConnectEnd::GraceSpent(message));
        }
        let sleep_for = deadline.map_or(delay, |d| delay.min(d.saturating_duration_since(now)));
        tokio::select! {
            () = stop.cancelled() => return Err(ConnectEnd::Stopped),
            () = tokio::time::sleep(sleep_for) => {}
        }
        delay = delay.saturating_mul(2).min(MAX_BACKOFF);
    }
}

/// One connect attempt's failure.
enum Attempt {
    /// `rp` did not answer: retry.
    Unreachable(String),
    /// `rp` answered and the document does not pass: the run fails.
    Invalid(String),
}

/// Connect, fetch the catalog, and run the document through all three
/// validation layers against it — `rp` answered, so the document must
/// pass now or the run fails loudly rather than mid-night.
async fn connect_and_validate(
    state: &AppState,
    manifest: &RunManifest,
    mcp_url: &str,
    connection: &RpConnection,
) -> Result<(Document, McpClient), Attempt> {
    let mcp = McpClient::connect(mcp_url, connection.auth(), connection.ca_path())
        .await
        .map_err(|e| Attempt::Unreachable(e.to_string()))?;
    let catalog = mcp
        .list_tools()
        .await
        .map_err(|e| Attempt::Unreachable(e.to_string()))?;
    let path = resolve_workflow_path(&state.config.workflows_dir, &manifest.workflow)
        .map_err(Attempt::Invalid)?;
    let source = tokio::fs::read_to_string(&path).await.map_err(|e| {
        Attempt::Invalid(format!(
            "cannot read workflow `{}` at {}: {e}",
            manifest.workflow,
            path.display()
        ))
    })?;
    let document = Document::parse(&source).map_err(|issues| {
        Attempt::Invalid(format!(
            "document failed validation: {}",
            issue_messages(&issues)
        ))
    })?;
    let issues = validate_against_catalog(&document, &catalog);
    if !issues.is_empty() {
        return Err(Attempt::Invalid(format!(
            "catalog validation failed: {}",
            issue_messages(&issues)
        )));
    }
    Ok((document, mcp))
}

fn issue_messages(issues: &[crate::document::ValidationIssue]) -> String {
    issues
        .iter()
        .map(|i| {
            if i.pointer.is_empty() {
                i.message.clone()
            } else {
                format!("{}: {}", i.pointer, i.message)
            }
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// Wait until `rp` reports safe conditions: `get_safety_status` every
/// `safety_poll_interval`, woken early by a `safety_changed` event with
/// `new_state: "safe"`. Marks the run paused for safety while it waits.
async fn wait_for_safe(
    state: &AppState,
    run_id: &str,
    mcp: &McpClient,
    mcp_url: &str,
    connection: &RpConnection,
    stop: &CancellationToken,
) -> Result<(), WaitEnd> {
    let mut intake: Option<crate::engine::EventIntake> = None;
    loop {
        match mcp.call("get_safety_status", Map::new()).await {
            Ok(status) => {
                if status.get("overall").and_then(Value::as_str) == Some("safe") {
                    debug!(run_id, "rp reports safe conditions");
                    return Ok(());
                }
                let detail = unsafe_detail(&status);
                debug!(run_id, %detail, "rp reports unsafe conditions; waiting");
                state.runs.mark_paused(run_id, "safety", &detail);
            }
            Err(ToolCallError::Unavailable(message)) => return Err(WaitEnd::Unavailable(message)),
            Err(e) => {
                // A healthy rp that cannot answer its own safety status:
                // keep waiting on the poll cadence.
                debug!(run_id, error = %e, "get_safety_status failed; waiting");
            }
        }
        // Subscribed lazily: the first status read decides whether a
        // wait is needed at all.
        let events = match intake.as_mut() {
            Some(events) => events,
            None => intake
                .insert(events::subscribe(events_url(&state.config, mcp_url), connection).await),
        };
        let poll = tokio::time::sleep(state.config.safety_poll_interval);
        tokio::pin!(poll);
        loop {
            tokio::select! {
                () = stop.cancelled() => return Err(WaitEnd::Stopped),
                event = events.next() => {
                    if event.event == "safety_changed"
                        && event.payload.get("new_state").and_then(Value::as_str) == Some("safe")
                    {
                        debug!(run_id, "safety_changed: safe; re-reading the status");
                        break;
                    }
                }
                () = &mut poll => break,
            }
        }
    }
}

/// A one-line description of an unsafe `get_safety_status` result.
fn unsafe_detail(status: &Value) -> String {
    let overall = status
        .get("overall")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let monitors = status
        .get("monitors")
        .and_then(Value::as_array)
        .map(|monitors| {
            monitors
                .iter()
                .filter_map(|m| {
                    let id = m.get("id").and_then(Value::as_str)?;
                    let state = m.get("state").and_then(Value::as_str)?;
                    Some(format!("{id}={state}"))
                })
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    if monitors.is_empty() {
        format!("rp reports {overall}")
    } else {
        format!("rp reports {overall} (monitors: {monitors})")
    }
}

/// The `session.report.*` object a run accumulated: the in-memory
/// blackboard's when a pass just ended, else the persisted one — a stop
/// or a failure that lands during a pause wait still reports what the
/// document summarised before pausing.
async fn report_for(
    state: &AppState,
    manifest: &RunManifest,
    in_memory: Option<&Value>,
) -> Option<Value> {
    if let Some(report) = in_memory {
        return Some(report.clone());
    }
    let path = manifest.blackboard_path(&state.config.state_dir);
    match Blackboard::load(path).await {
        Ok(blackboard) => blackboard.value().get("report").cloned(),
        Err(e) => {
            debug!(run_id = %manifest.run_id, error = %e, "no persisted report to carry into the outcome");
            None
        }
    }
}

async fn end_failed(state: &AppState, manifest: &RunManifest, error: &str, report: Option<&Value>) {
    warn!(run_id = %manifest.run_id, error, "run failed");
    let report = report_for(state, manifest, report).await;
    end(
        state,
        manifest,
        RunState::Failed,
        completion_result(&manifest.workflow, "failed", Some(error), report.as_ref()),
    )
    .await;
}

async fn end_stopped(state: &AppState, manifest: &RunManifest, report: Option<&Value>) {
    let report = report_for(state, manifest, report).await;
    end(
        state,
        manifest,
        RunState::Stopped,
        completion_result(&manifest.workflow, "stopped", None, report.as_ref()),
    )
    .await;
}

/// Record the outcome and delete the run's files.
async fn end(state: &AppState, manifest: &RunManifest, run_state: RunState, outcome: Value) {
    info!(run_id = %manifest.run_id, state = ?run_state, "run ended");
    state
        .runs
        .mark_ended(&manifest.run_id, run_state, outcome, Utc::now());
    remove_quietly(
        &manifest.blackboard_path(&state.config.state_dir),
        "blackboard",
    )
    .await;
    remove_quietly(
        &RunManifest::path(&state.config.state_dir, &manifest.session_id),
        "manifest",
    )
    .await;
}

/// Self-resume on startup (design § Runs): every manifest in `state_dir`
/// is resumed under its original ids, waiting for `rp` unbounded.
pub async fn resume_on_start(state: Arc<AppState>) {
    let manifests = RunManifest::scan(&state.config.state_dir).await;
    if manifests.is_empty() {
        debug!("no run manifests to resume");
        return;
    }
    for manifest in manifests {
        match state.runs.start(
            &manifest.run_id,
            &manifest.session_id,
            &manifest.workflow,
            manifest.started_at,
        ) {
            Ok(stop) => {
                info!(
                    run_id = %manifest.run_id,
                    workflow = %manifest.workflow,
                    "resuming the run left by the previous process"
                );
                tokio::spawn(supervise(state.clone(), manifest, stop, None));
            }
            Err(active) => warn!(
                run_id = %manifest.run_id,
                active,
                "a run is already active; manifest left in place"
            ),
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn the_registry_admits_one_active_run_at_a_time() {
        let registry = RunRegistry::default();
        let now = Utc::now();
        registry.start("run-1", "s-1", "flats", now).unwrap();
        assert_eq!(
            registry.start("run-2", "s-2", "flats", now).unwrap_err(),
            "run-1"
        );
        registry.mark_paused("run-1", "safety", "clouds");
        assert_eq!(
            registry.start("run-2", "s-2", "flats", now).unwrap_err(),
            "run-1",
            "a paused run still owns the rig"
        );
        registry.mark_ended("run-1", RunState::Complete, json!({}), now);
        registry.start("run-2", "s-2", "flats", now).unwrap();
        assert_eq!(registry.active().as_deref(), Some("run-2"));
        let listed: Vec<_> = registry.list().into_iter().map(|r| r.run_id).collect();
        assert_eq!(listed, vec!["run-2", "run-1"], "newest first");
    }

    #[test]
    fn a_pause_and_a_resume_are_reported_on_the_record() {
        let registry = RunRegistry::default();
        registry
            .start("run-1", "s-1", "deep-sky", Utc::now())
            .unwrap();
        registry.mark_paused("run-1", "rp_outage", "connection refused");
        let record = registry.get("run-1").unwrap();
        assert_eq!(record.state, RunState::Paused);
        assert_eq!(record.paused_reason, Some("rp_outage"));
        assert_eq!(record.paused_detail.as_deref(), Some("connection refused"));
        registry.mark_running("run-1");
        let record = registry.get("run-1").unwrap();
        assert_eq!(record.state, RunState::Running);
        assert_eq!(record.paused_reason, None);
        let json = serde_json::to_value(&record).unwrap();
        assert_eq!(json["state"], json!("running"));
        assert!(json.get("paused_reason").is_none(), "{json}");
        assert!(json.get("stop").is_none(), "the token is not serialized");
    }

    #[test]
    fn stopping_fires_the_token_once_and_refuses_ended_runs() {
        let registry = RunRegistry::default();
        let token = registry.start("run-1", "s-1", "flats", Utc::now()).unwrap();
        assert_eq!(registry.stop("run-9").unwrap_err(), StopError::NotFound);
        registry.stop("run-1").unwrap();
        assert!(token.is_cancelled());
        registry.mark_ended("run-1", RunState::Stopped, json!({}), Utc::now());
        assert_eq!(
            registry.stop("run-1").unwrap_err(),
            StopError::AlreadyEnded(RunState::Stopped)
        );
    }

    #[tokio::test]
    async fn the_manifest_round_trips_and_scans_in_start_order() {
        let dir = tempfile::tempdir().unwrap();
        let older = RunManifest {
            run_id: "run-a".into(),
            session_id: "s-a".into(),
            workflow: "flats".into(),
            params: json!({ "camera_id": "main-cam" }),
            started_at: Utc::now() - chrono::Duration::minutes(5),
        };
        let newer = RunManifest {
            run_id: "run-b".into(),
            session_id: "s-b".into(),
            workflow: "deep_sky".into(),
            params: json!({}),
            started_at: Utc::now(),
        };
        newer.write(dir.path()).await.unwrap();
        older.write(dir.path()).await.unwrap();
        // A stray blackboard and garbage are not manifests.
        std::fs::write(dir.path().join("s-a.json"), b"{}").unwrap();
        std::fs::write(dir.path().join("bad.run.json"), b"not json").unwrap();

        let found = RunManifest::scan(dir.path()).await;
        assert_eq!(found, vec![older.clone(), newer]);
        assert_eq!(
            RunManifest::path(dir.path(), "s-a"),
            dir.path().join("s-a.run.json")
        );
        assert_eq!(
            older.blackboard_path(dir.path()),
            dir.path().join("s-a.json")
        );
    }

    #[test]
    fn minted_ids_are_distinct_and_path_safe() {
        let now = Utc::now();
        let a = mint_session_id(now);
        let b = mint_session_id(now);
        assert_ne!(a, b);
        assert!(a.starts_with("session-"), "{a}");
        assert!(valid_session_id(&a));
        assert!(mint_run_id().starts_with("run-"));
        assert_ne!(mint_run_id(), mint_run_id());
        for bad in ["", ".", "..", "a/b", "a\\b"] {
            assert!(!valid_session_id(bad), "{bad:?}");
        }
    }

    /// A stop that lands outside an execution pass (during a pause wait)
    /// still carries the report the document persisted before pausing.
    #[tokio::test]
    async fn a_stop_during_a_pause_keeps_the_persisted_report_in_the_outcome() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(Config {
            server: crate::config::ServerConfig::new(0),
            workflows_dir: dir.path().join("workflows"),
            state_dir: dir.path().to_path_buf(),
            mcp_server_url: None,
            events_url: None,
            service_auth: None,
            ca_cert: None,
            rp_outage_grace: Duration::from_secs(1),
            safety_poll_interval: Duration::from_millis(50),
            resume_on_start: false,
        });
        let manifest = RunManifest {
            run_id: "run-1".into(),
            session_id: "s-1".into(),
            workflow: "flats".into(),
            params: json!({}),
            started_at: Utc::now(),
        };
        manifest.write(dir.path()).await.unwrap();
        std::fs::write(
            dir.path().join("s-1.json"),
            br#"{ "report": { "frames": 12 } }"#,
        )
        .unwrap();
        state
            .runs
            .start("run-1", "s-1", "flats", Utc::now())
            .unwrap();

        end_stopped(&state, &manifest, None).await;

        let record = state.runs.get("run-1").unwrap();
        assert_eq!(record.state, RunState::Stopped);
        assert_eq!(
            record.outcome,
            Some(json!({ "workflow": "flats", "outcome": "stopped", "frames": 12 }))
        );
        assert!(
            !dir.path().join("s-1.json").exists(),
            "the blackboard is deleted"
        );
        assert!(
            !dir.path().join("s-1.run.json").exists(),
            "the manifest is deleted"
        );
    }

    #[test]
    fn the_completion_payload_merges_the_report_under_the_fixed_keys() {
        let report = json!({ "frames": 30, "outcome": "shadowed" });
        let payload = completion_result("flats", "complete", None, Some(&report));
        assert_eq!(
            payload,
            json!({ "workflow": "flats", "outcome": "complete", "frames": 30 })
        );
        let payload = completion_result("flats", "failed", Some("lens cap on"), None);
        assert_eq!(payload["error"], json!("lens cap on"));
    }

    #[test]
    fn unsafe_detail_names_the_monitors() {
        let status = json!({
            "overall": "unsafe",
            "monitors": [ { "id": "weather", "state": "unsafe" }, { "id": "roof", "state": "safe" } ]
        });
        assert_eq!(
            unsafe_detail(&status),
            "rp reports unsafe (monitors: weather=unsafe, roof=safe)"
        );
        assert_eq!(unsafe_detail(&json!({})), "rp reports unknown");
    }

    #[test]
    fn rp_base_url_strips_the_mcp_suffix_with_or_without_a_trailing_slash() {
        for url in ["http://host:11115/mcp", "http://host:11115/mcp/"] {
            assert_eq!(rp_base_url(url), "http://host:11115", "{url}");
        }
        assert_eq!(rp_base_url("http://host:11115/"), "http://host:11115");
    }
}
