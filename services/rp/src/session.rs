//! Session lifecycle: the state machine behind `/api/session/*` and the
//! orchestrator invocation protocol (rp.md § Orchestrator Invocation
//! Protocol, § Safety).
//!
//! A session is `Idle`, `Active`, or `Interrupted`. Starting a session
//! invokes the configured orchestrator plugin; a safety unsafe
//! transition moves an active session to `Interrupted` (the safety
//! enforcer also tears down the MCP transport — see `crate::safety`);
//! the safe transition re-invokes the orchestrator with recovery
//! context and the same ids. The invoke POST is retried on transport
//! errors and 5xx responses; a 4xx is permanent. When every attempt
//! fails the session returns to `Idle` and a `session_stopped` event
//! with `reason: "orchestrator_invoke_failed"` is emitted — a session
//! never sits active with an orchestrator that was never reached.
//!
//! The invoke POST is an ordinary rp-as-client call, so it carries the
//! same transport wiring every other one does: rp's top-level `ca_cert`
//! trust, so an orchestrator serving TLS with the observatory's
//! self-signed certificate is reachable, and the registration's own
//! `auth` credential, so one that 401-challenges is too (issue #800).
//! Without both, a plugin was pinned to plain HTTP forever — TLS-
//! enabling it broke every session start.
//!
//! The registry is persisted (rp.md § Session Persistence): every
//! transition — and, via [`SessionManager::persist_progress`], every
//! recorded exposure — rewrites the session state file atomically, and
//! every transition to `Idle` deletes it. On startup
//! [`SessionManager::recover_startup`] reads the file back: a live
//! session is restored (counters included) and the orchestrator is
//! re-invoked with `recovery.reason = "rp_restart"`. Persistence
//! failures are logged at `warn!`, never raised — bookkeeping must not
//! end an otherwise healthy night.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rp_auth::config::ClientAuthConfig;
use serde_json::Value;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::events::EventBus;

/// Attempts made for one orchestrator invocation (initial or recovery).
const INVOKE_ATTEMPTS: u32 = 3;

/// Delay between invocation attempts. Short: the retry exists to ride
/// out an engine mid-restart (systemd brings it back in seconds), not
/// to wait out a long outage.
const INVOKE_RETRY_DELAY: Duration = Duration::from_secs(1);

#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub data_directory: String,
}

enum SessionState {
    Idle,
    Active {
        session_id: String,
        workflow_id: String,
        /// RFC 3339 wall-clock start, minted at `start()` and carried
        /// through interrupts and restarts into the state file.
        started_at: String,
    },
    /// A safety event interrupted the workflow; the ids are kept so the
    /// safe transition can re-invoke the orchestrator for the same
    /// session (its persisted state — e.g. session-runner's blackboard —
    /// is keyed by `session_id`).
    Interrupted {
        session_id: String,
        workflow_id: String,
        started_at: String,
    },
}

/// The `status` field of the persisted session state file.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
enum PersistedStatus {
    Active,
    Interrupted,
}

/// The on-disk shape of the session state file (rp.md § Session
/// Persistence): the registry plus the planner's progress counters.
/// An idle session has no file.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct PersistedSession {
    session_id: String,
    workflow_id: String,
    status: PersistedStatus,
    started_at: String,
    /// The serialized [`crate::planner::progress::SessionProgress`]
    /// store; kept as raw JSON here so persisting never needs to clone
    /// the store — it is serialized under its own lock. `null` when no
    /// progress store is wired (tests).
    #[serde(default)]
    progress: Value,
}

/// Connect-phase timeout for the `/invoke` POST. A loopback or LAN plugin
/// completes the TCP connect far inside this; the bound keeps a
/// black-holed host from stalling the connect indefinitely. Mirrors
/// `equipment::alpaca::ALPACA_CONNECT_TIMEOUT`.
const INVOKE_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Read timeout for the `/invoke` POST. The protocol's acknowledgement is
/// prompt by contract — an orchestrator spawns the workflow and answers
/// with timing estimates (rp.md § Orchestrator Invocation Protocol) — so
/// 10 s is far above any healthy ack. Without it a silently stalled
/// plugin would hang inside a single attempt forever, which is not a
/// transport error and so never reaches [`INVOKE_ATTEMPTS`]' retry: the
/// session would sit `active` behind a workflow that was never
/// acknowledged. Same failure class as the #319 Alpaca hang.
const INVOKE_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// rp's client for the orchestrator `/invoke` POST: `ca_cert_path` is
/// the observatory CA (`Config::ca_cert_path`, rp.md §Configuration),
/// without which an `https://` `invoke_url` signed by that CA fails
/// certificate verification — the same wiring `build_alpaca_client` and
/// the solver/guider clients carry, timeouts included.
fn build_invoke_client(
    ca_cert_path: Option<&Path>,
) -> Result<reqwest::Client, Box<dyn std::error::Error + Send + Sync>> {
    Ok(rusty_photon_tls::client::client_builder(ca_cert_path)?
        .user_agent("rusty-photon-rp")
        .connect_timeout(INVOKE_CONNECT_TIMEOUT)
        .read_timeout(INVOKE_READ_TIMEOUT)
        .build()?)
}

/// The registered orchestrator plugin and the client rp invokes it
/// with (rp.md § Orchestrator Registration). Built once at startup:
/// a registration whose `auth` block does not parse, or whose client
/// cannot be built (an unreadable `ca_cert`), fails startup loud rather
/// than surfacing as a dead `/invoke` on the first session of the night.
///
/// Held behind an `Arc` (rather than derived `Clone`): every
/// [`SessionManager::spawn_invoke`] hands one to a background task, and
/// the registration's `config` is an arbitrarily large opaque object —
/// a workflow plan for `session-runner`, a full flat plan for
/// `calibrator-flats`. Sharing it keeps that off the per-invocation path,
/// leaving exactly one deep clone: the one the POST body needs.
struct Orchestrator {
    invoke_url: String,
    /// The registration's `config` object — opaque to rp, passed
    /// through verbatim in the `/invoke` POST.
    config: Option<Value>,
    /// Carries rp's top-level `ca_cert` trust, so an `invoke_url`
    /// served with the observatory's self-signed certificate verifies
    /// (issue #800 — the same gap #612 closed for the solver/guider).
    client: reqwest::Client,
    /// The registration's `auth` credential, presented as HTTP Basic on
    /// every `/invoke` POST. `None` for a plugin that does not
    /// challenge — every request to one that does would 401.
    auth: Option<ClientAuthConfig>,
}

impl Orchestrator {
    /// Build the client for the registered orchestrator. `Ok(None)` when
    /// no orchestrator is registered — the only reading of `plugins[]`
    /// under which rp legitimately has nothing to invoke.
    ///
    /// The registration itself comes from
    /// [`crate::config::OrchestratorRegistration::sole`], the same parse
    /// `validate_config` runs, so what rp starts with is what
    /// `PUT /api/config` and `rp doctor` accept: exactly one
    /// orchestrator, carrying an `invoke_url` rp can POST to. A second
    /// registration is not resolved by array position here — it fails
    /// startup, because the entry that lost would do no work and say
    /// nothing about it.
    ///
    /// Errors name the registration by its `plugins[]` index, the same
    /// path `validate_config` and doctor use (`plugins.<index>.auth`),
    /// with the plugin's own name beside it: nothing stops two
    /// registrations sharing a `name`, and the index is what an operator
    /// can act on.
    fn from_plugins(
        plugins: &[Value],
        ca_cert_path: Option<&Path>,
    ) -> Result<Option<Self>, String> {
        let Some(registration) = crate::config::OrchestratorRegistration::sole(plugins)
            // The same rendering `load_config` gives a `FieldError`, so
            // the message an operator sees does not depend on which of
            // the two rejected the config.
            .map_err(|e| format!("{} {}", e.path, e.msg))?
        else {
            return Ok(None);
        };
        let client = build_invoke_client(ca_cert_path).map_err(|e| {
            format!(
                "plugins.{} ({}): failed to build the invoke HTTP client: {e}",
                registration.index, registration.name
            )
        })?;

        Ok(Some(Self {
            invoke_url: registration.invoke_url,
            config: registration.config,
            client,
            auth: registration.auth,
        }))
    }
}

pub struct SessionManager {
    state: RwLock<SessionState>,
    event_bus: Arc<EventBus>,
    /// The orchestrator plugin a session start invokes; `None` when none
    /// is registered, which makes every start a no-op invocation.
    orchestrator: Option<Arc<Orchestrator>>,
    mcp_base_url: RwLock<String>,
    /// The planner's `record_exposure` counters, shared with
    /// `McpHandler` (see `lib.rs`). A fresh `start()` clears them — a
    /// new `session_id` is a new night — while the safety
    /// interrupt/resume path never passes through `start()`, so a
    /// resumed session keeps its progress. `None` in tests that don't
    /// exercise the planner.
    planner_progress: Option<Arc<std::sync::Mutex<crate::planner::progress::SessionProgress>>>,
    /// Where the session state file lives (rp.md § Session
    /// Persistence). `None` disables persistence entirely — tests that
    /// only exercise the state machine.
    state_path: Option<PathBuf>,
    /// Camera-cooling controller (rp.md § Camera Cooling): session
    /// start runs its cooldown pass, every transition to idle its
    /// warm-up ramp, and — only under safe conditions — startup
    /// recovery and safety resume its re-adopt path (no-actuation-on-
    /// connect tenet: an unsafe startup leaves the cooler untouched
    /// and defers to the resume path). `None` in tests that only
    /// exercise the state machine.
    cooling: Option<Arc<crate::cooling::CoolingController>>,
}

impl SessionManager {
    /// `ca_cert_path` is rp's top-level `ca_cert` (`Config::ca_cert_path`):
    /// the trust the `/invoke` client needs to reach an orchestrator
    /// serving TLS. Startup aborts loud on a bad registration rather
    /// than leaving the first session start of the night to discover it.
    ///
    /// # Errors
    ///
    /// Returns a message (rendered as `<path> <msg>`, the same shape
    /// `load_config` gives a `FieldError`) when `plugins[]` registers a
    /// second orchestrator, when the registration carries no `invoke_url`
    /// (or one rp cannot POST to) or an `auth` block that does not parse,
    /// or when its `/invoke` client cannot be built.
    pub fn new(
        event_bus: Arc<EventBus>,
        plugins: &[Value],
        ca_cert_path: Option<&Path>,
    ) -> Result<Self, String> {
        let orchestrator = Orchestrator::from_plugins(plugins, ca_cert_path)?.map(Arc::new);

        debug!(
            orchestrator_url = ?orchestrator.as_ref().map(|o| &o.invoke_url),
            authenticated = orchestrator.as_ref().is_some_and(|o| o.auth.is_some()),
            "session manager initialized"
        );

        Ok(Self {
            state: RwLock::new(SessionState::Idle),
            event_bus,
            orchestrator,
            mcp_base_url: RwLock::new(String::new()),
            planner_progress: None,
            state_path: None,
            cooling: None,
        })
    }

    /// Share the planner's `record_exposure` counters so `start()`
    /// can clear them when a fresh session begins.
    #[must_use]
    pub fn with_progress_store(
        mut self,
        store: Arc<std::sync::Mutex<crate::planner::progress::SessionProgress>>,
    ) -> Self {
        self.planner_progress = Some(store);
        self
    }

    /// Enable session-state persistence at the given path (rp.md
    /// § Session Persistence).
    #[must_use]
    pub fn with_state_path(mut self, path: PathBuf) -> Self {
        self.state_path = Some(path);
        self
    }

    /// Wire the camera-cooling controller so session transitions drive
    /// cooldown, warm-up, and recovery (rp.md § Camera Cooling).
    #[must_use]
    pub fn with_cooling(mut self, cooling: Arc<crate::cooling::CoolingController>) -> Self {
        self.cooling = Some(cooling);
        self
    }

    pub async fn set_mcp_base_url(&self, url: String) {
        *self.mcp_base_url.write().await = url;
    }

    /// Start a new session: mint the ids, persist the state, and invoke
    /// the orchestrator.
    ///
    /// # Errors
    ///
    /// Returns a message if a session is already active or interrupted.
    /// A failed orchestrator invoke is not an error here — it stops the
    /// session and emits `session_stopped` instead.
    pub async fn start(self: &Arc<Self>) -> Result<Value, String> {
        let mut state = self.state.write().await;

        match &*state {
            SessionState::Idle => {}
            SessionState::Active { .. } => {
                return Err("a session is already active".to_string());
            }
            SessionState::Interrupted { .. } => {
                return Err(
                    "a session is interrupted, awaiting safe conditions to resume".to_string(),
                );
            }
        }

        let session_id = Uuid::new_v4().to_string();
        let workflow_id = Uuid::new_v4().to_string();
        let started_at = chrono::Utc::now().to_rfc3339();

        *state = SessionState::Active {
            session_id: session_id.clone(),
            workflow_id: workflow_id.clone(),
            started_at,
        };

        // A fresh session is a fresh night: reset the planner's
        // record_exposure counters *before* persisting, so the state
        // file starts the night at zero. The safety interrupt/resume
        // path re-invokes the orchestrator without passing through
        // here, so a resumed session keeps its progress.
        if let Some(progress) = &self.planner_progress {
            progress
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clear();
            debug!("planner progress counters cleared for the fresh session");
        }
        self.persist(&state).await;
        drop(state);

        debug!(session_id = %session_id, workflow_id = %workflow_id, "session started");

        self.event_bus.emit(
            "session_started",
            serde_json::json!({
                "session_id": session_id,
                "workflow_id": workflow_id,
            }),
        );

        self.spawn_invoke(workflow_id.clone(), session_id.clone(), None)
            .await;

        // Cooldown runs concurrently with the orchestrator — imaging
        // preparation is never blocked on thermal settling (rp.md
        // § Camera Cooling). A warm-up still ramping from the previous
        // session is cancelled and superseded.
        if let Some(cooling) = &self.cooling {
            cooling.start_cooldown();
        }

        Ok(serde_json::json!({
            "session_id": session_id,
            "workflow_id": workflow_id,
        }))
    }

    /// Move an active session to `Interrupted` (safety unsafe
    /// transition). Returns whether there was an active session to
    /// interrupt. The MCP teardown happens in `crate::safety` — this is
    /// only the bookkeeping half.
    pub async fn interrupt(&self) -> bool {
        let mut state = self.state.write().await;
        let SessionState::Active {
            session_id,
            workflow_id,
            started_at,
        } = &*state
        else {
            return false;
        };
        debug!(session_id = %session_id, workflow_id = %workflow_id,
               "session interrupted by safety event");
        *state = SessionState::Interrupted {
            session_id: session_id.clone(),
            workflow_id: workflow_id.clone(),
            started_at: started_at.clone(),
        };
        self.persist(&state).await;
        drop(state);
        true
    }

    /// Resume an interrupted session (safety safe transition): mark it
    /// active again and re-invoke the orchestrator with recovery
    /// context. Returns whether there was an interrupted session.
    pub async fn resume(self: &Arc<Self>) -> bool {
        let mut state = self.state.write().await;
        let SessionState::Interrupted {
            session_id,
            workflow_id,
            started_at,
        } = &*state
        else {
            return false;
        };
        let (session_id, workflow_id, started_at) =
            (session_id.clone(), workflow_id.clone(), started_at.clone());
        *state = SessionState::Active {
            session_id: session_id.clone(),
            workflow_id: workflow_id.clone(),
            started_at,
        };
        self.persist(&state).await;
        drop(state);

        // Re-adopt (or re-select) cooler rungs now that conditions are
        // safe: this is the deferred half of an unsafe-at-startup
        // restore (`recover_startup` skips it entirely to honor the
        // no-actuation-on-connect tenet), and a no-op re-adoption for an
        // ordinary live interruption, whose cooler was never touched
        // (rp.md § Camera Cooling → Recovery). Not a tenet violation:
        // this session was already operator-started before the outage
        // or safety event, so re-adopting on its unsafe -> safe
        // transition is automatic cleanup inside an operator-started
        // session, the same carve-out class as park-on-safety-transition
        // (workspace.md § Project Tenets, "No actuation on connect").
        if let Some(cooling) = &self.cooling {
            cooling.recover();
        }

        debug!(session_id = %session_id, workflow_id = %workflow_id,
               "conditions safe again; re-invoking the orchestrator with recovery context");
        let recovery = serde_json::json!({ "reason": "safety_interruption" });
        self.spawn_invoke(workflow_id, session_id, Some(recovery))
            .await;
        true
    }

    /// POST the orchestrator invocation in the background, retrying per
    /// the protocol (rp.md § Orchestrator Invocation Protocol). No-op
    /// when no orchestrator is configured.
    async fn spawn_invoke(
        self: &Arc<Self>,
        workflow_id: String,
        session_id: String,
        recovery: Option<Value>,
    ) {
        let Some(orchestrator) = self.orchestrator.clone() else {
            return;
        };
        let mcp_url = self.mcp_base_url.read().await.clone();
        let body = serde_json::json!({
            "workflow_id": workflow_id,
            "session_id": session_id,
            "mcp_server_url": format!("{}/mcp", mcp_url),
            "recovery": recovery,
            "config": orchestrator.config.clone(),
        });

        let manager = Arc::clone(self);
        tokio::spawn(async move {
            if !invoke_with_retry(&orchestrator, &body).await {
                manager.fail_invoke(&workflow_id).await;
            }
        });
    }

    /// Every invocation attempt failed: return the session to idle so
    /// the operator can see the failure and start over, unless the
    /// session has already moved on (completed, stopped, restarted).
    async fn fail_invoke(&self, failed_workflow_id: &str) {
        let mut state = self.state.write().await;
        let matches = match &*state {
            SessionState::Active { workflow_id, .. }
            | SessionState::Interrupted { workflow_id, .. } => workflow_id == failed_workflow_id,
            SessionState::Idle => false,
        };
        if !matches {
            debug!(workflow_id = %failed_workflow_id,
                   "invoke failure for a session that already moved on; ignoring");
            return;
        }
        warn!(workflow_id = %failed_workflow_id,
              "orchestrator could not be invoked; session returns to idle");
        *state = SessionState::Idle;
        self.delete_state_file().await;
        drop(state);

        self.event_bus.emit(
            "session_stopped",
            serde_json::json!({
                "reason": "orchestrator_invoke_failed",
                "workflow_id": failed_workflow_id,
            }),
        );
        if let Some(cooling) = &self.cooling {
            cooling.start_warmup();
        }
    }

    /// Stop the session from any state: idle it, drop the persisted
    /// state file, emit `session_stopped`, and start the cooler warm-up.
    ///
    /// # Errors
    ///
    /// Never fails — every step absorbs its own failure; the `Result`
    /// keeps `POST /api/session/stop`'s handler in the same shape as
    /// `start`'s.
    pub async fn stop(&self) -> Result<(), String> {
        let mut state = self.state.write().await;
        *state = SessionState::Idle;
        self.delete_state_file().await;
        drop(state);

        debug!("session stopped");

        self.event_bus.emit(
            "session_stopped",
            serde_json::json!({
                "reason": "manual_stop",
            }),
        );

        // Every transition to idle ramps cooled cameras warm (a no-op
        // for cameras rp never commanded).
        if let Some(cooling) = &self.cooling {
            cooling.start_warmup();
        }

        Ok(())
    }

    pub async fn status(&self) -> String {
        let state = self.state.read().await;
        match *state {
            SessionState::Idle => "idle".to_string(),
            SessionState::Active { .. } => "active".to_string(),
            SessionState::Interrupted { .. } => "interrupted".to_string(),
        }
    }

    #[expect(
        clippy::significant_drop_tightening,
        reason = "the state-file delete and stop side effects stay under the state lock so a racing transition cannot interleave between the in-memory change and the file change"
    )]
    pub async fn workflow_complete(&self, workflow_id: &str) {
        let mut state = self.state.write().await;

        // An interrupted session can complete too: the engine may post
        // completion in the same instant the safety monitor turns
        // unsafe — the completion wins, there is nothing left to resume.
        let matches = match &*state {
            SessionState::Active {
                workflow_id: wf_id, ..
            }
            | SessionState::Interrupted {
                workflow_id: wf_id, ..
            } => wf_id == workflow_id,
            SessionState::Idle => false,
        };

        if matches {
            debug!(workflow_id = %workflow_id, "workflow completed, session ending");
            *state = SessionState::Idle;
            self.delete_state_file().await;

            self.event_bus.emit(
                "session_stopped",
                serde_json::json!({
                    "reason": "workflow_complete",
                    "workflow_id": workflow_id,
                }),
            );
            if let Some(cooling) = &self.cooling {
                cooling.start_warmup();
            }
        } else {
            debug!(workflow_id = %workflow_id, "workflow_complete received but no matching active session");
        }
    }

    /// Re-persist the state file with the current planner counters.
    /// Called by the `record_exposure` tool after each recorded frame
    /// (rp.md § Write Strategy: at most one frame's progress is lost to
    /// a power failure). A no-op while idle — an idle session has no
    /// file — or when persistence is not configured.
    ///
    /// Takes the **write** lock despite not mutating: `RwLock` admits
    /// concurrent readers, so a read lock would let two overlapping
    /// `record_exposure` calls race `persist()` and land their atomic
    /// renames out of order — regressing the file to older counters
    /// (a repeated frame after a restart). The write lock upholds the
    /// writer-serialization invariant `persist()` documents.
    pub async fn persist_progress(&self) {
        let state = self.state.write().await;
        self.persist(&state).await;
    }

    /// Startup recovery (rp.md § Recovery Behavior): read the session
    /// state file back and, when a session was live, restore the
    /// registry and the planner's counters. Under safe conditions
    /// (`conditions_safe`, read from the `/mcp` gate after the safety
    /// poller's inline first pass — `BoundServer::start`) the
    /// orchestrator is re-invoked with `recovery.reason = "rp_restart"`;
    /// under unsafe ones the session is restored **interrupted** with
    /// no invocation, and the ordinary unsafe → safe machinery resumes
    /// it when conditions clear. Returns whether a session was
    /// restored. Called once, immediately before the server starts
    /// serving.
    ///
    /// The persisted status itself gates nothing — conditions may have
    /// flipped either way while rp was down, so the current poll, not
    /// the file, decides. An unreadable or corrupt file is never fatal:
    /// rp starts idle with a `warn!`, because refusing to start over
    /// unreadable bookkeeping would be worse than losing one resume.
    pub async fn recover_startup(self: &Arc<Self>, conditions_safe: bool) -> bool {
        let Some(path) = self.state_path.clone() else {
            return false;
        };
        let bytes = match tokio::fs::read(&path).await {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                debug!("no session state file; starting idle");
                return false;
            }
            Err(e) => {
                warn!(path = %path.display(), error = %e,
                      "cannot read the session state file; starting idle");
                return false;
            }
        };
        let persisted: PersistedSession = match serde_json::from_slice(&bytes) {
            Ok(persisted) => persisted,
            Err(e) => {
                warn!(path = %path.display(), error = %e,
                      "session state file is corrupt; starting idle");
                return false;
            }
        };

        // Restore the planner's counters first — the re-invoked
        // orchestrator's dispatch reads them immediately. The store is
        // assigned unconditionally: the file is the source of truth, so
        // missing (`null`) or unreadable persisted counters overwrite
        // whatever is in memory with a zeroed slate rather than
        // trusting the caller to have constructed the store empty.
        if let Some(store) = &self.planner_progress {
            let restored = if persisted.progress.is_null() {
                crate::planner::progress::SessionProgress::default()
            } else {
                serde_json::from_value(persisted.progress).unwrap_or_else(|e| {
                    warn!(error = %e,
                        "persisted progress counters are unreadable; resuming with zeroed counters");
                    crate::planner::progress::SessionProgress::default()
                })
            };
            *store
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = restored;
        }

        let restored_status = if conditions_safe {
            PersistedStatus::Active
        } else {
            PersistedStatus::Interrupted
        };
        let mut state = self.state.write().await;
        *state = match restored_status {
            PersistedStatus::Active => SessionState::Active {
                session_id: persisted.session_id.clone(),
                workflow_id: persisted.workflow_id.clone(),
                started_at: persisted.started_at.clone(),
            },
            PersistedStatus::Interrupted => SessionState::Interrupted {
                session_id: persisted.session_id.clone(),
                workflow_id: persisted.workflow_id.clone(),
                started_at: persisted.started_at.clone(),
            },
        };
        // Normalize the on-disk status to what was restored. When it
        // already matches, the rewrite would reproduce the bytes just
        // read — skip it (the pre-serve path should not pay a needless
        // fsync).
        if !matches!(
            (persisted.status, restored_status),
            (PersistedStatus::Active, PersistedStatus::Active)
                | (PersistedStatus::Interrupted, PersistedStatus::Interrupted)
        ) {
            self.persist(&state).await;
        }
        drop(state);

        if !conditions_safe {
            // No cooler actuation on an unsafe startup (no-actuation-on-
            // connect tenet, docs/workspace.md § Project Tenets): a
            // restored-as-interrupted session leaves the cooler
            // untouched here — `resume()` re-adopts (or re-selects) it
            // on the ordinary unsafe → safe transition instead.
            info!(session_id = %persisted.session_id, workflow_id = %persisted.workflow_id,
                  persisted_status = ?persisted.status,
                  "restored the persisted session as interrupted — conditions are unsafe; \
                   the orchestrator will be re-invoked on the safe transition");
            return true;
        }

        // Re-adopt (or re-select) cooler rungs for the restored session —
        // interrupted sessions included, since the cooler holds through
        // an interruption (rp.md § Camera Cooling → Recovery).
        if let Some(cooling) = &self.cooling {
            cooling.recover();
        }

        info!(session_id = %persisted.session_id, workflow_id = %persisted.workflow_id,
              persisted_status = ?persisted.status, started_at = %persisted.started_at,
              "restored the persisted session; re-invoking the orchestrator with recovery context");
        let recovery = serde_json::json!({ "reason": "rp_restart" });
        self.spawn_invoke(persisted.workflow_id, persisted.session_id, Some(recovery))
            .await;
        true
    }

    /// Serialize the given registry state + current counters and write
    /// the state file atomically (a no-op for `Idle` — an idle session
    /// has no file). Failures are logged at `warn!`, never raised —
    /// bookkeeping must not end an otherwise healthy night (rp.md
    /// § Write Strategy).
    ///
    /// Callers hold the state **write** lock (they pass the value they
    /// just stored in it) **across the write on purpose**: it serializes
    /// concurrent writers so the file can never regress to an older
    /// state, and it makes the delete-then-recreate race with `stop()`
    /// impossible (a `persist_progress` landing after a stop would
    /// otherwise resurrect a stale file that a later restart resumes).
    /// The fsync held under the lock is the accepted cost — transitions
    /// are rare and `record_exposure` runs at frame cadence.
    async fn persist(&self, state: &SessionState) {
        let Some(path) = self.state_path.clone() else {
            return;
        };
        let (session_id, workflow_id, status, started_at) = match state {
            SessionState::Active {
                session_id,
                workflow_id,
                started_at,
            } => (session_id, workflow_id, PersistedStatus::Active, started_at),
            SessionState::Interrupted {
                session_id,
                workflow_id,
                started_at,
            } => (
                session_id,
                workflow_id,
                PersistedStatus::Interrupted,
                started_at,
            ),
            SessionState::Idle => return,
        };
        let progress = self.planner_progress.as_ref().map_or(Value::Null, |store| {
            let store = store
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            serde_json::to_value(&*store).unwrap_or(Value::Null)
        });
        let persisted = PersistedSession {
            session_id: session_id.clone(),
            workflow_id: workflow_id.clone(),
            status,
            started_at: started_at.clone(),
            progress,
        };
        let body = match serde_json::to_vec_pretty(&persisted) {
            Ok(body) => body,
            Err(e) => {
                warn!(error = %e, "cannot serialize the session state; skipping the write");
                return;
            }
        };
        let write_path = path.clone();
        let result =
            tokio::task::spawn_blocking(move || rp_fits::atomic::write_atomic(&write_path, &body))
                .await;
        match result {
            Ok(Ok(())) => debug!(path = %path.display(), "session state persisted"),
            Ok(Err(e)) => warn!(path = %path.display(), error = %e,
                                "failed to write the session state file; continuing"),
            Err(e) => warn!(error = %e, "session state write task failed; continuing"),
        }
    }

    /// Delete the state file — every transition to idle. Missing is
    /// fine (persistence may be disabled, or nothing was ever written).
    async fn delete_state_file(&self) {
        let Some(path) = &self.state_path else {
            return;
        };
        match tokio::fs::remove_file(path).await {
            Ok(()) => debug!(path = %path.display(), "session state file deleted"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => warn!(path = %path.display(), error = %e,
                            "failed to delete the session state file"),
        }
    }
}

/// POST `body` to the orchestrator's invoke URL, retrying transient
/// failures (transport errors, 5xx) up to [`INVOKE_ATTEMPTS`] times with
/// [`INVOKE_RETRY_DELAY`] between attempts. A 4xx response is permanent —
/// the same request will fail the same way. Returns whether an attempt
/// was acknowledged with a success status.
///
/// The credential rides on the request rather than the client's default
/// headers so it never lands in a `Client`-level `Debug` render;
/// `basic_auth` marks the header sensitive on top of that.
async fn invoke_with_retry(orchestrator: &Orchestrator, body: &Value) -> bool {
    let invoke_url = &orchestrator.invoke_url;
    for attempt in 1..=INVOKE_ATTEMPTS {
        debug!(url = %invoke_url, attempt, "invoking orchestrator");
        let mut request = orchestrator.client.post(invoke_url).json(body);
        if let Some(auth) = &orchestrator.auth {
            request = request.basic_auth(&auth.username, Some(&auth.password));
        }
        match request.send().await {
            Ok(resp) if resp.status().is_success() => {
                debug!(status = %resp.status(), attempt, "orchestrator invoked");
                return true;
            }
            Ok(resp) if resp.status().is_client_error() => {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                warn!(%status, %body, "orchestrator rejected the invocation; not retrying");
                return false;
            }
            Ok(resp) => {
                warn!(status = %resp.status(), attempt, max = INVOKE_ATTEMPTS,
                      "orchestrator invocation failed");
            }
            Err(e) => {
                warn!(error = %e, attempt, max = INVOKE_ATTEMPTS,
                      "failed to reach the orchestrator");
            }
        }
        if attempt < INVOKE_ATTEMPTS {
            tokio::time::sleep(INVOKE_RETRY_DELAY).await;
        }
    }
    false
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::routing::post;
    use axum::Json;
    use axum::Router;
    use serde_json::json;

    use super::*;
    use crate::cooling::test_support::{controller_for, stub_router, CoolerSim, Sim};
    use crate::equipment::test_support::spawn_stub;

    /// In-process orchestrator stub: records every `/invoke` body and
    /// answers with the scripted status sequence (last entry repeats).
    struct InvokeStub {
        url: String,
        bodies: Arc<RwLock<Vec<Value>>>,
        hits: Arc<AtomicU32>,
        _shutdown: tokio::sync::oneshot::Sender<()>,
    }

    async fn spawn_invoke_stub(statuses: Vec<StatusCode>) -> InvokeStub {
        #[derive(Clone)]
        struct StubState {
            bodies: Arc<RwLock<Vec<Value>>>,
            hits: Arc<AtomicU32>,
            statuses: Arc<Vec<StatusCode>>,
        }
        let state = StubState {
            bodies: Arc::new(RwLock::new(Vec::new())),
            hits: Arc::new(AtomicU32::new(0)),
            statuses: Arc::new(statuses),
        };
        let app = Router::new()
            .route(
                "/invoke",
                post(
                    |State(state): State<StubState>, Json(body): Json<Value>| async move {
                        state.bodies.write().await.push(body);
                        let n = state.hits.fetch_add(1, Ordering::SeqCst) as usize;
                        let status = *state
                            .statuses
                            .get(n)
                            .or_else(|| state.statuses.last())
                            .unwrap_or(&StatusCode::OK);
                        (
                            status,
                            Json(json!({"estimated_duration": "1s", "max_duration": "0s"})),
                        )
                    },
                ),
            )
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await
                .unwrap();
        });
        InvokeStub {
            url: format!("http://127.0.0.1:{port}/invoke"),
            bodies: state.bodies,
            hits: state.hits,
            _shutdown: tx,
        }
    }

    fn manager_for(invoke_url: &str) -> Arc<SessionManager> {
        let event_bus = Arc::new(EventBus::from_config(&[], None).unwrap());
        let plugins = vec![json!({
            "name": "test-orchestrator",
            "type": "orchestrator",
            "invoke_url": invoke_url,
            "config": {"workflow": "w"},
        })];
        Arc::new(SessionManager::new(event_bus, &plugins, None).unwrap())
    }

    // 300 × 50ms = 15s. Generous because the unreachable-orchestrator test
    // sits through the full retry schedule first, and on Windows a refused
    // localhost connect is not instant — WinSock retries SYNs for about a
    // second before reporting `WSAECONNREFUSED`, so three attempts plus two
    // 1s backoffs already burn ~5s there.
    async fn wait_for_status(manager: &Arc<SessionManager>, expected: &str) -> bool {
        for _ in 0..300 {
            if manager.status().await == expected {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    async fn wait_for_hits(stub: &InvokeStub, expected: u32) -> bool {
        wait_for_count(&stub.hits, expected).await
    }

    /// Polls a hit counter up to 5s — the shape `wait_for_hits` uses for
    /// stubs that own one, for the tests whose stub is a bare counter.
    async fn wait_for_count(hits: &AtomicU32, expected: u32) -> bool {
        for _ in 0..100 {
            if hits.load(Ordering::SeqCst) >= expected {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    /// Polls `sim`'s `set_setpoint_calls` up to 5s for the background
    /// `cooling.recover()` task's actuation to land.
    async fn wait_for_setpoint_calls(sim: &Sim, expected: u32) -> bool {
        for _ in 0..100 {
            if sim.lock().unwrap().set_setpoint_calls >= expected {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    /// Asserts `sim` sees no cooler actuation for `window` — fails as soon
    /// as one is observed rather than only after the window elapses.
    async fn assert_no_setpoint_calls_within(sim: &Sim, window: Duration) {
        let deadline = tokio::time::Instant::now() + window;
        while tokio::time::Instant::now() < deadline {
            assert_eq!(
                sim.lock().unwrap().set_setpoint_calls,
                0,
                "cooler was actuated during the no-actuation window"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn start_invokes_orchestrator_with_null_recovery() {
        let stub = spawn_invoke_stub(vec![StatusCode::OK]).await;
        let manager = manager_for(&stub.url);

        let response = manager.start().await.unwrap();
        assert!(response.get("session_id").is_some());
        assert!(wait_for_hits(&stub, 1).await, "orchestrator never invoked");

        let bodies = stub.bodies.read().await;
        assert!(bodies[0]["recovery"].is_null());
        assert_eq!(bodies[0]["config"], json!({"workflow": "w"}));
        drop(bodies);
        assert_eq!(manager.status().await, "active");
    }

    #[tokio::test]
    async fn a_fresh_start_clears_the_last_recorded_filter() {
        let stub = spawn_invoke_stub(vec![StatusCode::OK]).await;
        let event_bus = Arc::new(EventBus::from_config(&[], None).unwrap());
        let plugins = vec![json!({
            "name": "test-orchestrator",
            "type": "orchestrator",
            "invoke_url": stub.url,
        })];
        let progress = Arc::new(std::sync::Mutex::new(
            crate::planner::progress::SessionProgress::default(),
        ));
        progress.lock().unwrap().record(Some("Red"));
        let manager = Arc::new(
            SessionManager::new(event_bus, &plugins, None)
                .unwrap()
                .with_progress_store(progress.clone()),
        );

        manager.start().await.unwrap();

        assert_eq!(
            progress.lock().unwrap().last_filter_key(),
            None,
            "a fresh session start must forget last night's filter"
        );
    }

    #[tokio::test]
    async fn invoke_retries_a_5xx_then_succeeds() {
        let stub = spawn_invoke_stub(vec![StatusCode::INTERNAL_SERVER_ERROR, StatusCode::OK]).await;
        let manager = manager_for(&stub.url);

        manager.start().await.unwrap();
        assert!(wait_for_hits(&stub, 2).await, "no retry after the 5xx");
        // The session must remain active: the retry succeeded.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(manager.status().await, "active");
    }

    #[tokio::test]
    async fn invoke_gives_up_immediately_on_4xx() {
        let stub = spawn_invoke_stub(vec![StatusCode::BAD_REQUEST]).await;
        let manager = manager_for(&stub.url);
        let mut events = manager.event_bus.subscribe();

        manager.start().await.unwrap();
        assert!(
            wait_for_status(&manager, "idle").await,
            "session did not return to idle after the permanent invoke failure"
        );
        assert_eq!(
            stub.hits.load(Ordering::SeqCst),
            1,
            "a 4xx must not be retried"
        );

        // session_started, then session_stopped with the failure reason.
        let started = events.recv().await.unwrap();
        assert_eq!(started.event, "session_started");
        let stopped = events.recv().await.unwrap();
        assert_eq!(stopped.event, "session_stopped");
        assert_eq!(stopped.payload["reason"], "orchestrator_invoke_failed");
    }

    #[tokio::test]
    async fn invoke_unreachable_exhausts_retries_and_returns_to_idle() {
        // Bind a port then drop the listener: connects are refused.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let manager = manager_for(&format!("http://127.0.0.1:{port}/invoke"));

        manager.start().await.unwrap();
        assert!(
            wait_for_status(&manager, "idle").await,
            "session did not return to idle with an unreachable orchestrator"
        );
    }

    #[tokio::test]
    async fn interrupt_then_resume_reinvokes_with_recovery_and_same_ids() {
        let stub = spawn_invoke_stub(vec![StatusCode::OK]).await;
        let manager = manager_for(&stub.url);

        manager.start().await.unwrap();
        assert!(wait_for_hits(&stub, 1).await);

        assert!(manager.interrupt().await);
        assert_eq!(manager.status().await, "interrupted");

        assert!(manager.resume().await);
        assert_eq!(manager.status().await, "active");
        assert!(wait_for_hits(&stub, 2).await, "resume never re-invoked");

        let bodies = stub.bodies.read().await;
        assert_eq!(
            bodies[1]["recovery"],
            json!({"reason": "safety_interruption"})
        );
        assert_eq!(bodies[1]["workflow_id"], bodies[0]["workflow_id"]);
        assert_eq!(bodies[1]["session_id"], bodies[0]["session_id"]);
    }

    #[tokio::test]
    async fn start_is_refused_while_active_and_while_interrupted() {
        let stub = spawn_invoke_stub(vec![StatusCode::OK]).await;
        let manager = manager_for(&stub.url);
        manager.start().await.unwrap();

        let err = manager.start().await.unwrap_err();
        assert!(err.contains("already active"), "got: {err}");

        manager.interrupt().await;
        // The refusal must name the interrupted state, not claim the
        // session is active — the operator would otherwise look for a
        // running workflow that isn't there.
        let err = manager.start().await.unwrap_err();
        assert!(err.contains("interrupted"), "got: {err}");
    }

    #[tokio::test]
    async fn workflow_complete_ends_an_interrupted_session() {
        let stub = spawn_invoke_stub(vec![StatusCode::OK]).await;
        let manager = manager_for(&stub.url);
        manager.start().await.unwrap();
        assert!(wait_for_hits(&stub, 1).await);
        let workflow_id = stub.bodies.read().await[0]["workflow_id"]
            .as_str()
            .unwrap()
            .to_owned();

        manager.interrupt().await;
        manager.workflow_complete(&workflow_id).await;
        assert_eq!(manager.status().await, "idle");
        // Nothing left to resume.
        assert!(!manager.resume().await);
    }

    #[tokio::test]
    async fn interrupt_and_resume_are_noops_when_idle() {
        let stub = spawn_invoke_stub(vec![StatusCode::OK]).await;
        let manager = manager_for(&stub.url);
        assert!(!manager.interrupt().await);
        assert!(!manager.resume().await);
        assert_eq!(manager.status().await, "idle");
    }

    // --- Session-state persistence (rp.md § Session Persistence) ---

    fn state_path(dir: &tempfile::TempDir) -> std::path::PathBuf {
        dir.path().join("session_state.json")
    }

    fn manager_with_state(
        invoke_url: &str,
        path: std::path::PathBuf,
    ) -> (
        Arc<SessionManager>,
        Arc<std::sync::Mutex<crate::planner::progress::SessionProgress>>,
    ) {
        let event_bus = Arc::new(EventBus::from_config(&[], None).unwrap());
        let plugins = vec![json!({
            "name": "test-orchestrator",
            "type": "orchestrator",
            "invoke_url": invoke_url,
            "config": {"workflow": "w"},
        })];
        let progress = Arc::new(std::sync::Mutex::new(
            crate::planner::progress::SessionProgress::default(),
        ));
        let manager = Arc::new(
            SessionManager::new(event_bus, &plugins, None)
                .unwrap()
                .with_progress_store(progress.clone())
                .with_state_path(path),
        );
        (manager, progress)
    }

    fn manager_with_cooling(
        invoke_url: &str,
        path: std::path::PathBuf,
        cooling: Arc<crate::cooling::CoolingController>,
    ) -> Arc<SessionManager> {
        let event_bus = Arc::new(EventBus::from_config(&[], None).unwrap());
        let plugins = vec![json!({
            "name": "test-orchestrator",
            "type": "orchestrator",
            "invoke_url": invoke_url,
            "config": {"workflow": "w"},
        })];
        Arc::new(
            SessionManager::new(event_bus, &plugins, None)
                .unwrap()
                .with_state_path(path)
                .with_cooling(cooling),
        )
    }

    fn read_state(path: &std::path::Path) -> Value {
        let bytes = std::fs::read(path).expect("no session state file");
        serde_json::from_slice(&bytes).expect("session state file is not JSON")
    }

    #[tokio::test]
    async fn start_persists_the_state_file_and_stop_deletes_it() {
        let stub = spawn_invoke_stub(vec![StatusCode::OK]).await;
        let dir = tempfile::tempdir().unwrap();
        let path = state_path(&dir);
        let (manager, _) = manager_with_state(&stub.url, path.clone());

        let response = manager.start().await.unwrap();
        let persisted = read_state(&path);
        assert_eq!(persisted["status"], "active");
        assert_eq!(persisted["session_id"], response["session_id"]);
        assert_eq!(persisted["workflow_id"], response["workflow_id"]);
        assert!(
            persisted["started_at"]
                .as_str()
                .is_some_and(|s| { chrono::DateTime::parse_from_rfc3339(s).is_ok() }),
            "started_at is not RFC 3339: {}",
            persisted["started_at"]
        );

        manager.stop().await.unwrap();
        assert!(!path.exists(), "stop must delete the session state file");
    }

    #[tokio::test]
    async fn interrupt_and_resume_rewrite_the_persisted_status() {
        let stub = spawn_invoke_stub(vec![StatusCode::OK]).await;
        let dir = tempfile::tempdir().unwrap();
        let path = state_path(&dir);
        let (manager, _) = manager_with_state(&stub.url, path.clone());
        manager.start().await.unwrap();
        let started_at = read_state(&path)["started_at"].clone();

        manager.interrupt().await;
        let persisted = read_state(&path);
        assert_eq!(persisted["status"], "interrupted");
        assert_eq!(
            persisted["started_at"], started_at,
            "the start time survives the interrupt"
        );

        manager.resume().await;
        assert_eq!(read_state(&path)["status"], "active");
    }

    #[tokio::test]
    async fn workflow_complete_deletes_the_state_file() {
        let stub = spawn_invoke_stub(vec![StatusCode::OK]).await;
        let dir = tempfile::tempdir().unwrap();
        let path = state_path(&dir);
        let (manager, _) = manager_with_state(&stub.url, path.clone());
        manager.start().await.unwrap();
        assert!(wait_for_hits(&stub, 1).await);
        let workflow_id = stub.bodies.read().await[0]["workflow_id"]
            .as_str()
            .unwrap()
            .to_owned();

        manager.workflow_complete(&workflow_id).await;
        assert!(
            !path.exists(),
            "workflow completion must delete the session state file"
        );
    }

    #[tokio::test]
    async fn a_failed_invocation_deletes_the_state_file() {
        let stub = spawn_invoke_stub(vec![StatusCode::BAD_REQUEST]).await;
        let dir = tempfile::tempdir().unwrap();
        let path = state_path(&dir);
        let (manager, _) = manager_with_state(&stub.url, path.clone());

        manager.start().await.unwrap();
        assert!(wait_for_status(&manager, "idle").await);
        assert!(
            !path.exists(),
            "the invoke-failure transition to idle must delete the state file"
        );
    }

    #[tokio::test]
    async fn persist_progress_rewrites_the_last_filter_and_is_a_noop_when_idle() {
        let stub = spawn_invoke_stub(vec![StatusCode::OK]).await;
        let dir = tempfile::tempdir().unwrap();
        let path = state_path(&dir);
        let (manager, progress) = manager_with_state(&stub.url, path.clone());

        // Idle: no session, no file — even with a filter recorded.
        progress.lock().unwrap().record(Some("Red"));
        manager.persist_progress().await;
        assert!(!path.exists(), "an idle session must have no state file");

        manager.start().await.unwrap();
        // start() cleared it; the persisted store carries no filter.
        assert_eq!(
            read_state(&path)["progress"]["last_filter_key"],
            json!(null)
        );

        progress.lock().unwrap().record(Some("Red"));
        manager.persist_progress().await;
        let persisted = read_state(&path);
        assert_eq!(persisted["progress"]["last_filter_key"], "Red");
    }

    #[tokio::test]
    async fn recover_startup_restores_the_session_and_reinvokes_with_rp_restart() {
        let stub = spawn_invoke_stub(vec![StatusCode::OK]).await;
        let dir = tempfile::tempdir().unwrap();
        let path = state_path(&dir);

        // First life: a session with a recorded frame, then a crash
        // (the manager is simply dropped — nothing deletes the file).
        let (first, progress) = manager_with_state(&stub.url, path.clone());
        first.start().await.unwrap();
        assert!(wait_for_hits(&stub, 1).await);
        progress.lock().unwrap().record(Some("Red"));
        first.persist_progress().await;
        drop(first);

        // Second life: a fresh manager over the same path.
        let (second, fresh_progress) = manager_with_state(&stub.url, path.clone());
        assert!(
            second.recover_startup(true).await,
            "no session was restored"
        );
        assert_eq!(second.status().await, "active");
        assert_eq!(
            fresh_progress.lock().unwrap().last_filter_key(),
            Some("Red"),
            "the last filter must be restored from the state file"
        );

        assert!(wait_for_hits(&stub, 2).await, "no recovery re-invocation");
        let bodies = stub.bodies.read().await;
        assert_eq!(bodies[1]["recovery"], json!({"reason": "rp_restart"}));
        assert_eq!(bodies[1]["workflow_id"], bodies[0]["workflow_id"]);
        assert_eq!(bodies[1]["session_id"], bodies[0]["session_id"]);
        assert_eq!(bodies[1]["config"], json!({"workflow": "w"}));
    }

    #[tokio::test]
    async fn recover_startup_restores_an_interrupted_session_as_active() {
        let stub = spawn_invoke_stub(vec![StatusCode::OK]).await;
        let dir = tempfile::tempdir().unwrap();
        let path = state_path(&dir);
        let (first, _) = manager_with_state(&stub.url, path.clone());
        first.start().await.unwrap();
        first.interrupt().await;
        assert_eq!(read_state(&path)["status"], "interrupted");
        drop(first);

        let (second, _) = manager_with_state(&stub.url, path.clone());
        assert!(second.recover_startup(true).await);
        // Conditions are safe, so the poll — not the file — decides:
        // restored active and re-persisted as such.
        assert_eq!(second.status().await, "active");
        assert_eq!(read_state(&path)["status"], "active");
    }

    #[tokio::test]
    async fn recover_startup_under_unsafe_conditions_restores_interrupted_without_invoking() {
        let stub = spawn_invoke_stub(vec![StatusCode::OK]).await;
        let dir = tempfile::tempdir().unwrap();
        let path = state_path(&dir);
        let (first, _) = manager_with_state(&stub.url, path.clone());
        first.start().await.unwrap();
        assert!(wait_for_hits(&stub, 1).await);
        drop(first);

        let (second, _) = manager_with_state(&stub.url, path.clone());
        assert!(second.recover_startup(false).await);
        // Unsafe conditions: no re-invocation — the ordinary
        // unsafe → safe machinery resumes the session when they clear.
        assert_eq!(second.status().await, "interrupted");
        assert_eq!(read_state(&path)["status"], "interrupted");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            stub.hits.load(Ordering::SeqCst),
            1,
            "an unsafe restore must not re-invoke the orchestrator"
        );

        assert!(second.resume().await, "the safe transition resumes it");
        assert!(wait_for_hits(&stub, 2).await, "resume never re-invoked");
        assert_eq!(
            stub.bodies.read().await[1]["recovery"],
            json!({"reason": "safety_interruption"})
        );
    }

    #[tokio::test]
    async fn recover_startup_recovers_the_cooler_when_conditions_are_safe() {
        let stub = spawn_invoke_stub(vec![StatusCode::OK]).await;
        let dir = tempfile::tempdir().unwrap();
        let path = state_path(&dir);
        let (first, _) = manager_with_state(&stub.url, path.clone());
        first.start().await.unwrap();
        assert!(wait_for_hits(&stub, 1).await);
        drop(first);

        let sim: Sim = Arc::new(std::sync::Mutex::new(CoolerSim::new()));
        let cam_stub = spawn_stub(stub_router(sim.clone())).await;
        let (cooling, _rx) = controller_for(&cam_stub.url(), &[-10]).await;

        let second = manager_with_cooling(&stub.url, path.clone(), cooling);
        assert!(second.recover_startup(true).await);
        assert!(wait_for_hits(&stub, 2).await);

        assert!(
            wait_for_setpoint_calls(&sim, 1).await,
            "a safe restart must recover (command) the cooler"
        );
    }

    /// The no-actuation-on-connect tenet (docs/workspace.md § Project
    /// Tenets), applied to camera cooling: issue #636.
    #[tokio::test]
    async fn recover_startup_under_unsafe_conditions_defers_cooling_to_the_resume_path() {
        let stub = spawn_invoke_stub(vec![StatusCode::OK]).await;
        let dir = tempfile::tempdir().unwrap();
        let path = state_path(&dir);
        let (first, _) = manager_with_state(&stub.url, path.clone());
        first.start().await.unwrap();
        assert!(wait_for_hits(&stub, 1).await);
        drop(first);

        // The cooler is off — the driver-truth read in `run_recover`
        // would otherwise fall through to an actuating cooldown pass.
        let sim: Sim = Arc::new(std::sync::Mutex::new(CoolerSim::new()));
        let cam_stub = spawn_stub(stub_router(sim.clone())).await;
        let (cooling, _rx) = controller_for(&cam_stub.url(), &[-10]).await;

        let second = manager_with_cooling(&stub.url, path.clone(), cooling);
        assert!(second.recover_startup(false).await);
        assert_eq!(second.status().await, "interrupted");

        assert_no_setpoint_calls_within(&sim, Duration::from_millis(300)).await;

        assert!(second.resume().await, "the safe transition resumes it");
        assert!(
            wait_for_setpoint_calls(&sim, 1).await,
            "the deferred cooler recovery must run once conditions are safe"
        );
    }

    #[tokio::test]
    async fn recover_startup_is_a_noop_without_a_state_file() {
        let stub = spawn_invoke_stub(vec![StatusCode::OK]).await;
        let dir = tempfile::tempdir().unwrap();
        let (manager, _) = manager_with_state(&stub.url, state_path(&dir));

        assert!(!manager.recover_startup(true).await);
        assert_eq!(manager.status().await, "idle");
        assert_eq!(stub.hits.load(Ordering::SeqCst), 0, "nothing to re-invoke");
    }

    #[tokio::test]
    async fn recover_startup_with_a_corrupt_file_starts_idle() {
        let stub = spawn_invoke_stub(vec![StatusCode::OK]).await;
        let dir = tempfile::tempdir().unwrap();
        let path = state_path(&dir);
        std::fs::write(&path, b"{ not json").unwrap();
        let (manager, _) = manager_with_state(&stub.url, path.clone());

        assert!(!manager.recover_startup(true).await);
        assert_eq!(manager.status().await, "idle");
        assert_eq!(stub.hits.load(Ordering::SeqCst), 0);
        // The corrupt file is left in place for the operator; the next
        // session start overwrites it.
        assert!(path.exists());
    }

    #[tokio::test]
    async fn recover_startup_clears_a_stale_filter_when_progress_is_unreadable_or_absent() {
        // A store that already holds a filter (a reused manager) must
        // not leak it into the recovered session.
        for progress in [json!("garbage"), Value::Null] {
            let stub = spawn_invoke_stub(vec![StatusCode::OK]).await;
            let dir = tempfile::tempdir().unwrap();
            let path = state_path(&dir);
            std::fs::write(
                &path,
                serde_json::to_vec(&json!({
                    "session_id": "s-1",
                    "workflow_id": "w-1",
                    "status": "active",
                    "started_at": "2026-07-11T00:00:00Z",
                    "progress": progress,
                }))
                .unwrap(),
            )
            .unwrap();

            let (manager, store) = manager_with_state(&stub.url, path.clone());
            store.lock().unwrap().record(Some("Red"));

            assert!(manager.recover_startup(true).await);
            assert_eq!(
                store.lock().unwrap().last_filter_key(),
                None,
                "progress {progress} must overwrite the stale in-memory filter"
            );
        }
    }

    #[tokio::test]
    async fn a_manager_without_a_state_path_never_writes_a_file() {
        let stub = spawn_invoke_stub(vec![StatusCode::OK]).await;
        let manager = manager_for(&stub.url);
        manager.start().await.unwrap();
        manager.persist_progress().await;
        // Nothing observable to assert beyond "no panic" — the manager
        // has no path to write to; recover_startup is equally inert.
        assert!(!manager.recover_startup(true).await);
    }

    #[tokio::test]
    async fn invoke_failure_for_a_moved_on_session_is_ignored() {
        let stub = spawn_invoke_stub(vec![StatusCode::OK]).await;
        let manager = manager_for(&stub.url);
        manager.start().await.unwrap();
        assert!(wait_for_hits(&stub, 1).await);

        // A stale failure from a previous workflow must not clobber the
        // current session.
        manager.fail_invoke("some-older-workflow").await;
        assert_eq!(manager.status().await, "active");

        // Nor may a late failure resurrect activity once the session is
        // over: fail_invoke on an idle manager stays idle.
        let workflow_id = stub.bodies.read().await[0]["workflow_id"]
            .as_str()
            .unwrap()
            .to_owned();
        manager.stop().await.unwrap();
        manager.fail_invoke(&workflow_id).await;
        assert_eq!(manager.status().await, "idle");
    }

    // ----- Plugin TLS / credential wiring (issue #800) -----------------
    //
    // The invoke client is rp's only client that had neither CA trust nor
    // a credential, which pinned every orchestrator plugin to plain HTTP:
    // the moment one served TLS or 401-challenged, `POST
    // /api/session/start` died with `orchestrator_invoke_failed`. These
    // tests pin both halves end-to-end — a stub that actually challenges,
    // and a stub that actually serves a CA-signed certificate — because
    // the config fields alone prove nothing about the client they feed.

    /// Orchestrator stub that 401s unless the request carries exactly
    /// `Basic <base64(user:pass)>`, mirroring `rp_auth`'s middleware.
    async fn spawn_challenging_invoke_stub(username: &str, password: &str) -> InvokeStub {
        use base64::engine::general_purpose::STANDARD as BASE64;
        use base64::Engine as _;

        #[derive(Clone)]
        struct StubState {
            bodies: Arc<RwLock<Vec<Value>>>,
            hits: Arc<AtomicU32>,
            expected: Arc<String>,
        }
        let state = StubState {
            bodies: Arc::new(RwLock::new(Vec::new())),
            hits: Arc::new(AtomicU32::new(0)),
            expected: Arc::new(format!(
                "Basic {}",
                BASE64.encode(format!("{username}:{password}"))
            )),
        };
        let app = Router::new()
            .route(
                "/invoke",
                post(
                    |State(state): State<StubState>,
                     headers: axum::http::HeaderMap,
                     Json(body): Json<Value>| async move {
                        let presented = headers
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or_default();
                        let accepted = presented == state.expected.as_str();
                        if accepted {
                            state.bodies.write().await.push(body);
                        }
                        // Counted last, so an observer that sees the hit
                        // also sees the recorded body — no test race.
                        state.hits.fetch_add(1, Ordering::SeqCst);
                        if accepted {
                            (
                                StatusCode::OK,
                                Json(json!({"estimated_duration": "1s", "max_duration": "0s"})),
                            )
                        } else {
                            (StatusCode::UNAUTHORIZED, Json(json!({})))
                        }
                    },
                ),
            )
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await
                .unwrap();
        });
        InvokeStub {
            url: format!("http://127.0.0.1:{port}/invoke"),
            bodies: state.bodies,
            hits: state.hits,
            _shutdown: tx,
        }
    }

    fn manager_with_auth(invoke_url: &str, auth: Option<Value>) -> Arc<SessionManager> {
        let event_bus = Arc::new(EventBus::from_config(&[], None).unwrap());
        let mut entry = json!({
            "name": "test-orchestrator",
            "type": "orchestrator",
            "invoke_url": invoke_url,
        });
        if let Some(auth) = auth {
            entry
                .as_object_mut()
                .unwrap()
                .insert("auth".to_string(), auth);
        }
        Arc::new(SessionManager::new(event_bus, &[entry], None).unwrap())
    }

    #[tokio::test]
    async fn invoke_presents_the_registration_credential() {
        let stub = spawn_challenging_invoke_stub("observatory", "secret").await;
        let manager = manager_with_auth(
            &stub.url,
            Some(json!({"username": "observatory", "password": "secret"})),
        );

        manager.start().await.unwrap();
        assert!(wait_for_hits(&stub, 1).await, "orchestrator never invoked");
        assert!(
            wait_for_status(&manager, "active").await,
            "an accepted invocation must leave the session active"
        );
        assert_eq!(
            stub.bodies.read().await.len(),
            1,
            "the challenging orchestrator never accepted the credential"
        );
    }

    // The negative half of the test above: without the registration's
    // `auth`, the same stub 401s and the session falls back to idle —
    // proving the credential, not the stub's leniency, is what makes the
    // invocation land.
    #[tokio::test]
    async fn an_uncredentialed_invoke_is_rejected_by_a_challenging_orchestrator() {
        let stub = spawn_challenging_invoke_stub("observatory", "secret").await;
        let manager = manager_with_auth(&stub.url, None);

        manager.start().await.unwrap();
        assert!(wait_for_hits(&stub, 1).await, "orchestrator never invoked");
        assert!(
            wait_for_status(&manager, "idle").await,
            "a 401'd invocation must return the session to idle"
        );
        assert!(
            stub.bodies.read().await.is_empty(),
            "the stub must not have accepted an uncredentialed invocation"
        );
    }

    /// A plugin that accepts the connection but never answers must
    /// surface as a transport error the retry can act on, not hang inside
    /// one attempt — the session would otherwise sit `active` behind a
    /// workflow that was never acknowledged. `start_paused` advances
    /// virtual time so the read timeout and the retry backoff fire in
    /// real-time milliseconds; the outer `timeout` only trips if the
    /// client-level timeout regresses, turning a silent hang into a loud
    /// failure.
    #[tokio::test(start_paused = true)]
    async fn invoke_times_out_on_a_silently_stalled_orchestrator() {
        let app = Router::new().route(
            "/invoke",
            post(|| async { std::future::pending::<Json<Value>>().await }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let manager = manager_with_auth(&format!("http://127.0.0.1:{port}/invoke"), None);
        manager.start().await.unwrap();

        let outcome = tokio::time::timeout(Duration::from_mins(10), async {
            while manager.status().await != "idle" {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
        outcome.expect("a stalled orchestrator must time out, not hang the invocation");
    }

    #[tokio::test]
    async fn a_malformed_orchestrator_auth_block_fails_startup() {
        let event_bus = Arc::new(EventBus::from_config(&[], None).unwrap());
        let plugins = vec![json!({
            "name": "calibrator-flats",
            "type": "orchestrator",
            "invoke_url": "http://127.0.0.1:11170/invoke",
            "auth": {"username": "observatory"},
        })];

        let error = SessionManager::new(event_bus, &plugins, None)
            .err()
            .expect("a half-written credential must fail startup");
        assert!(
            error.contains("plugins.0.auth (calibrator-flats)") && error.contains("password"),
            "a half-written credential must name the field it broke: {error}"
        );
    }

    /// A registration that declares itself the orchestrator and names no
    /// endpoint is malformed, not an rp with no orchestrator: it used to
    /// read as the latter, which is indistinguishable from a rig that
    /// deliberately runs without one — so every `POST
    /// /api/session/start` did nothing and said nothing.
    #[tokio::test]
    async fn an_orchestrator_without_an_invoke_url_fails_startup() {
        let event_bus = Arc::new(EventBus::from_config(&[], None).unwrap());
        let plugins = vec![json!({
            "name": "calibrator-flats",
            "type": "orchestrator",
        })];

        let error = SessionManager::new(event_bus, &plugins, None)
            .err()
            .expect("an orchestrator with nothing to POST to must fail startup");
        assert!(
            error.contains("plugins.0.invoke_url") && error.contains("calibrator-flats"),
            "must name the entry an operator has to fix: {error}"
        );
    }

    /// rp invokes one orchestrator, so a second registration fails
    /// startup rather than being resolved by array position — the entry
    /// that lost would be fully validated, reported clean, and never
    /// invoked, and any writer that round-trips `plugins[]` could swap
    /// which one that is without touching a value.
    #[tokio::test]
    async fn a_second_orchestrator_registration_fails_startup() {
        let event_bus = Arc::new(EventBus::from_config(&[], None).unwrap());
        let plugins = vec![
            json!({
                "name": "calibrator-flats",
                "type": "orchestrator",
                "invoke_url": "http://127.0.0.1:11170/invoke",
            }),
            json!({
                "name": "session-runner",
                "type": "orchestrator",
                "invoke_url": "http://127.0.0.1:11171/invoke",
            }),
        ];

        let error = SessionManager::new(event_bus, &plugins, None)
            .err()
            .expect("two orchestrator registrations must fail startup");
        assert!(
            error.contains("plugins.1.type")
                && error.contains("session-runner")
                && error.contains("plugins.0")
                && error.contains("calibrator-flats"),
            "must name both registrations: {error}"
        );
    }

    /// Which error a config with *both* faults reports matters, because
    /// it is the instruction an operator follows: naming the multiplicity
    /// first would read as "remove one" while pointing at the working
    /// registration, leaving the stub. The malformed entry is named
    /// first, in the same `plugins[]` order `validate_config` reports in,
    /// so the runtime and the load path never disagree about which field
    /// to fix.
    #[tokio::test]
    async fn a_malformed_orchestrator_is_named_before_the_second_one() {
        let event_bus = Arc::new(EventBus::from_config(&[], None).unwrap());
        let plugins = vec![
            json!({"name": "stub", "type": "orchestrator"}),
            json!({
                "name": "session-runner",
                "type": "orchestrator",
                "invoke_url": "http://127.0.0.1:11171/invoke",
            }),
        ];

        let error = SessionManager::new(event_bus, &plugins, None)
            .err()
            .expect("a stub beside a complete registration must fail startup");
        assert!(
            error.contains("plugins.0.invoke_url") && error.contains("stub"),
            "the malformed entry must be named, not the one that works: {error}"
        );
    }

    #[tokio::test]
    async fn an_unreadable_ca_cert_fails_startup() {
        let event_bus = Arc::new(EventBus::from_config(&[], None).unwrap());
        let plugins = vec![json!({
            "name": "calibrator-flats",
            "type": "orchestrator",
            "invoke_url": "https://127.0.0.1:11170/invoke",
        })];

        let error =
            SessionManager::new(event_bus, &plugins, Some(Path::new("/nonexistent/ca.pem")))
                .err()
                .expect("an unreadable ca_cert must fail startup");
        assert!(
            error.contains("plugins.0 (calibrator-flats)") && error.contains("invoke HTTP client"),
            "unexpected error: {error}"
        );
    }

    /// Proves the invoke client actually plumbs `ca_cert_path` into
    /// reqwest: a manager carrying the observatory CA invokes an
    /// orchestrator serving a CA-signed certificate and stays active;
    /// the same manager without that trust fails the handshake and falls
    /// back to idle. The end-to-end analogue of
    /// `ca_trusting_client_connects_to_ca_signed_alpaca_server`.
    #[tokio::test]
    async fn a_ca_trusting_manager_invokes_a_tls_orchestrator() {
        let pki_dir = tempfile::tempdir().unwrap();
        rusty_photon_tls::test_cert::generate_ca(pki_dir.path()).unwrap();
        let ca_cert_pem = std::fs::read_to_string(pki_dir.path().join("ca.pem")).unwrap();
        let ca_key_pem = std::fs::read_to_string(pki_dir.path().join("ca-key.pem")).unwrap();
        let certs_dir = pki_dir.path().join("certs");
        rusty_photon_tls::test_cert::generate_service_cert(
            &ca_cert_pem,
            &ca_key_pem,
            "test-orchestrator",
            &certs_dir,
        )
        .unwrap();
        let tls_config = rusty_photon_tls::config::TlsConfig {
            cert: certs_dir
                .join("test-orchestrator.pem")
                .to_string_lossy()
                .into_owned(),
            key: certs_dir
                .join("test-orchestrator-key.pem")
                .to_string_lossy()
                .into_owned(),
        };

        let hits = Arc::new(AtomicU32::new(0));
        let handler_hits = hits.clone();
        let router = Router::new().route(
            "/invoke",
            post(move |Json(_body): Json<Value>| {
                let hits = handler_hits.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    Json(json!({"estimated_duration": "1s", "max_duration": "0s"}))
                }
            }),
        );
        let addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = rusty_photon_tls::server::bind_dual_stack_tokio(addr)
            .await
            .unwrap();
        let bound_addr = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            rusty_photon_tls::server::serve_tls(listener, router, &tls_config, async {
                shutdown_rx.await.ok();
            })
            .await
            .unwrap();
        });

        // The bound IPv4 loopback, not "localhost": the listener is
        // IPv4-only and the generated cert's SANs include 127.0.0.1.
        let invoke_url = format!("https://{bound_addr}/invoke");
        let plugins = vec![json!({
            "name": "test-orchestrator",
            "type": "orchestrator",
            "invoke_url": invoke_url,
        })];
        let ca_path = pki_dir.path().join("ca.pem");

        let trusting = Arc::new(
            SessionManager::new(
                Arc::new(EventBus::from_config(&[], None).unwrap()),
                &plugins,
                Some(&ca_path),
            )
            .unwrap(),
        );
        trusting.start().await.unwrap();
        assert!(
            wait_for_count(&hits, 1).await,
            "a manager trusting the CA must reach the TLS orchestrator"
        );
        assert_eq!(
            trusting.status().await,
            "active",
            "an acknowledged invocation must leave the session active"
        );

        let untrusting = Arc::new(
            SessionManager::new(
                Arc::new(EventBus::from_config(&[], None).unwrap()),
                &plugins,
                None,
            )
            .unwrap(),
        );
        untrusting.start().await.unwrap();
        assert!(
            wait_for_status(&untrusting, "idle").await,
            "a manager without the CA must fail the handshake and go idle"
        );
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "no untrusted request may reach the orchestrator's handler"
        );

        shutdown_tx.send(()).ok();
        server.await.ok();
    }
}
