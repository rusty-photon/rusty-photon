//! The import pipeline
//! (docs/services/planetarium-bridge.md § The import pipeline).
//!
//! Every accepted Align produces one [`ImportRequest`] — bare ICRS
//! coordinates plus provenance, never a name — submitted to rp's
//! `add_target` MCP tool through `rp-mcp-client`. Delivery failures append
//! the request to the on-disk [`Spool`](crate::spool::Spool); tool failures
//! (rp actively rejected the call) are logged and dropped, because they
//! would fail identically on replay. A single worker task owns the client
//! session and the spool, which serializes deliveries and keeps replay
//! strictly FIFO.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rp_mcp_client::{ClientAuthConfig, McpCallError, RpMcpClient};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::spool::Spool;

/// The provenance `kind` every bridge import carries.
pub const SOURCE_KIND: &str = "planetarium-bridge";

/// One `add_target` import request — also the spool's line format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportRequest {
    /// ICRS, post-`assume_epoch` conversion.
    pub ra_hours: f64,
    pub dec_degrees: f64,
    pub source: ImportSource,
}

/// Provenance rp stores with the import.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportSource {
    pub kind: String,
    /// `ip:port` of the planetarium client, when known.
    pub client: String,
    /// RFC 3339 receipt time of the Align. Replays carry the original.
    pub received_at: String,
}

/// Process-lifetime counters behind `GET /health`.
#[derive(Debug, Default)]
pub struct HealthState {
    rp_reachable: AtomicBool,
    spooled: AtomicU64,
    replayed_total: AtomicU64,
    dropped_total: AtomicU64,
}

impl HealthState {
    pub fn snapshot(&self) -> serde_json::Value {
        serde_json::json!({
            "rp_reachable": self.rp_reachable.load(Ordering::SeqCst),
            "spooled": self.spooled.load(Ordering::SeqCst),
            "replayed_total": self.replayed_total.load(Ordering::SeqCst),
            "dropped_total": self.dropped_total.load(Ordering::SeqCst),
        })
    }

    fn set_reachable(&self, reachable: bool) {
        self.rp_reachable.store(reachable, Ordering::SeqCst);
    }

    fn is_reachable(&self) -> bool {
        self.rp_reachable.load(Ordering::SeqCst)
    }

    fn set_spooled(&self, spooled: usize) {
        self.spooled
            .store(u64::try_from(spooled).unwrap_or(u64::MAX), Ordering::SeqCst);
    }

    fn count_drop(&self) {
        self.dropped_total.fetch_add(1, Ordering::SeqCst);
    }

    fn count_replay(&self) {
        self.replayed_total.fetch_add(1, Ordering::SeqCst);
    }
}

/// One queued Align, stamped with the reachability decision taken at
/// receipt: whether rp was believed reachable when the sync verb was
/// accepted. The worker processes the queue later, and rp coming back in
/// between must not turn an outage-time Align into a direct delivery —
/// its fate was sealed at receipt.
#[derive(Debug)]
struct QueuedImport {
    request: ImportRequest,
    spool_first: bool,
}

/// The device's handle into the pipeline: a fire-and-forget submit.
#[derive(Debug, Clone)]
pub struct Importer {
    tx: mpsc::UnboundedSender<QueuedImport>,
    health: Arc<HealthState>,
}

impl Importer {
    pub fn submit(&self, request: ImportRequest) {
        let queued = QueuedImport {
            spool_first: !self.health.is_reachable(),
            request,
        };
        if self.tx.send(queued).is_err() {
            // Only during shutdown, after the worker ended.
            debug!("import worker gone; request not submitted");
        }
    }
}

/// How the bridge reaches rp (the `rp` config block, resolved).
#[derive(Debug, Clone)]
pub struct RpTarget {
    pub mcp_server_url: String,
    pub service_auth: Option<ClientAuthConfig>,
    pub ca_cert: Option<PathBuf>,
}

/// Everything the worker task needs.
pub struct ImportWorker {
    rx: mpsc::UnboundedReceiver<QueuedImport>,
    spool: Spool,
    health: Arc<HealthState>,
    rp: RpTarget,
    replay_backoff_max: Duration,
    cancel: CancellationToken,
}

/// Build the pipeline: the device-facing [`Importer`], the shared
/// [`HealthState`], and the worker to spawn.
#[must_use]
pub fn pipeline(
    spool_path: PathBuf,
    max_entries: usize,
    replay_backoff_max: Duration,
    rp: RpTarget,
    cancel: CancellationToken,
) -> (Importer, Arc<HealthState>, ImportWorker) {
    let (tx, rx) = mpsc::unbounded_channel();
    let health = Arc::new(HealthState::default());
    let (spool, dropped_at_load) = Spool::load(spool_path, max_entries);
    for _ in 0..dropped_at_load {
        health.count_drop();
    }
    health.set_spooled(spool.len());
    let worker = ImportWorker {
        rx,
        spool,
        health: Arc::clone(&health),
        rp,
        replay_backoff_max,
        cancel,
    };
    let importer = Importer {
        tx,
        health: Arc::clone(&health),
    };
    (importer, health, worker)
}

enum Delivery {
    Delivered,
    /// rp answered with a tool error — never spooled.
    ToolRejected(String),
    /// Transport-level failure — the request must spool.
    Unreachable(String),
}

impl ImportWorker {
    /// Run until cancelled. Owns the only rp session and the spool.
    pub async fn run(mut self) {
        self.run_inner().await;
        // Aligns still queued in the channel must survive the restart:
        // spool them before exiting (the accepted-sync contract).
        while let Ok(queued) = self.rx.try_recv() {
            self.spool_request(&queued.request);
        }
    }

    async fn run_inner(&mut self) {
        let mut client: Option<RpMcpClient> = None;
        let mut backoff = Duration::from_secs(1);

        // Startup probe so /health reports honest reachability before the
        // first Align. Reachability starts false, and an Align that arrives
        // while the probe is still in flight spools durably right away —
        // rp had never answered at receipt time, and its fate must not
        // depend on how the probe resolves moments later.
        {
            // Cloned so the pinned probe future doesn't hold a borrow of
            // `self` across the select arms that mutate the spool.
            let rp = self.rp.clone();
            let probe = connect(&rp);
            tokio::pin!(probe);
            loop {
                tokio::select! {
                    () = self.cancel.cancelled() => return,
                    result = &mut probe => {
                        match result {
                            // The probe session is dropped right away: the
                            // bridge never holds an idle MCP session (see
                            // the idle drop in the main loop).
                            Ok(_) => self.health.set_reachable(true),
                            Err(e) => debug!("rp not reachable at startup: {e}"),
                        }
                        break;
                    }
                    queued = self.rx.recv() => match queued {
                        Some(queued) => {
                            warn!("rp not yet reachable; spooling the import");
                            self.spool_request(&queued.request);
                            self.drain_queued_to_spool();
                        }
                        None => return,
                    },
                }
            }
        }

        loop {
            if self.spool.is_empty() {
                // Idle: drop the session. Sessions live only across a
                // delivery burst — a standing idle session holds an open
                // stream into rp that stalls rp's own graceful shutdown
                // (MSI verify: rp failed to stop, Error 1921, while an
                // idle bridge sat connected), and rp terminates MCP
                // sessions on safety transitions anyway, so every burst
                // reconnects explicitly per ADR-017.
                if client.take().is_some() {
                    debug!("import queue idle; dropping the rp session");
                }
                let queued = tokio::select! {
                    () = self.cancel.cancelled() => return,
                    queued = self.rx.recv() => match queued {
                        Some(queued) => queued,
                        None => return,
                    },
                };
                if queued.spool_first || !self.health.is_reachable() {
                    // rp was unreachable at receipt (the submit-time stamp)
                    // or is unreachable now: spool durably first; the
                    // replay loop delivers (and counts) it once rp is
                    // back. Attempting delivery here instead would race
                    // rp's recovery and turn an outage-time Align into a
                    // direct delivery. Anything else already queued
                    // arrived under the same outage and spools with it.
                    warn!("rp unreachable; spooling the import");
                    self.spool_request(&queued.request);
                    self.drain_queued_to_spool();
                    continue;
                }
                let request = queued.request;
                match deliver(&mut client, &self.rp, &request).await {
                    Delivery::Delivered => {
                        self.health.set_reachable(true);
                        backoff = Duration::from_secs(1);
                    }
                    Delivery::ToolRejected(message) => {
                        self.health.set_reachable(true);
                        error!(
                            ra_hours = request.ra_hours,
                            dec_degrees = request.dec_degrees,
                            "rp rejected the import (not spooled — it would be rejected \
                             again on replay): {message}"
                        );
                    }
                    Delivery::Unreachable(message) => {
                        self.health.set_reachable(false);
                        warn!("rp unreachable ({message}); spooling the import");
                        self.spool_request(&request);
                    }
                }
            } else {
                // Head-of-line replay. A corrupt line is skipped with its
                // original line number; delivery failures back off while
                // still accepting (and directly spooling) new Aligns.
                let head = self
                    .spool
                    .head()
                    .map(|entry| (entry.line.clone(), entry.source_line_no));
                let Some((line, line_no)) = head else {
                    continue;
                };
                let request: ImportRequest = match serde_json::from_str(&line) {
                    Ok(request) => request,
                    Err(e) => {
                        error!(
                            line = line_no,
                            "skipping corrupt spool line ({e}); continuing with the rest"
                        );
                        self.spool.pop_head();
                        self.health.set_spooled(self.spool.len());
                        continue;
                    }
                };
                match deliver(&mut client, &self.rp, &request).await {
                    Delivery::Delivered => {
                        self.spool.pop_head();
                        self.health.set_spooled(self.spool.len());
                        self.health.count_replay();
                        self.health.set_reachable(true);
                        backoff = Duration::from_secs(1);
                    }
                    Delivery::ToolRejected(message) => {
                        self.spool.pop_head();
                        self.health.set_spooled(self.spool.len());
                        self.health.set_reachable(true);
                        error!(
                            ra_hours = request.ra_hours,
                            dec_degrees = request.dec_degrees,
                            "rp rejected the replayed import (dropped — it would be \
                             rejected again): {message}"
                        );
                    }
                    Delivery::Unreachable(message) => {
                        self.health.set_reachable(false);
                        debug!(
                            backoff_seconds = backoff.as_secs_f64(),
                            "rp still unreachable ({message}); backing off"
                        );
                        if !self.backoff_accepting_aligns(backoff).await {
                            return;
                        }
                        backoff = backoff.saturating_mul(2).min(self.replay_backoff_max);
                    }
                }
            }
        }
    }

    /// Sleep out a backoff window while new Aligns keep landing in the
    /// spool behind the backlog (FIFO order). Returns `false` on shutdown.
    async fn backoff_accepting_aligns(&mut self, backoff: Duration) -> bool {
        let sleep = tokio::time::sleep(backoff);
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                () = self.cancel.cancelled() => return false,
                () = &mut sleep => return true,
                queued = self.rx.recv() => match queued {
                    Some(queued) => self.spool_request(&queued.request),
                    None => return false,
                },
            }
        }
    }

    /// Move every already-queued request into the spool (receipt order).
    fn drain_queued_to_spool(&mut self) {
        while let Ok(queued) = self.rx.try_recv() {
            self.spool_request(&queued.request);
        }
    }

    fn spool_request(&mut self, request: &ImportRequest) {
        match serde_json::to_string(request) {
            Ok(line) => {
                if self.spool.append(line) {
                    self.health.count_drop();
                }
                self.health.set_spooled(self.spool.len());
            }
            Err(e) => error!("could not serialize an import for the spool: {e}"),
        }
    }
}

async fn connect(rp: &RpTarget) -> Result<RpMcpClient, String> {
    RpMcpClient::connect(
        &rp.mcp_server_url,
        rp.service_auth.as_ref(),
        rp.ca_cert.as_deref(),
    )
    .await
    .map_err(|e| e.to_string())
}

async fn deliver(
    client: &mut Option<RpMcpClient>,
    rp: &RpTarget,
    request: &ImportRequest,
) -> Delivery {
    if client.is_none() {
        match connect(rp).await {
            Ok(c) => *client = Some(c),
            Err(e) => return Delivery::Unreachable(e),
        }
    }
    let Some(session) = client.as_ref() else {
        return Delivery::Unreachable("no session".to_owned());
    };
    let args = match serde_json::to_value(request) {
        Ok(serde_json::Value::Object(map)) => map,
        Ok(_) | Err(_) => {
            // Unreachable in practice: ImportRequest serializes to an object.
            return Delivery::ToolRejected("import request did not serialize".to_owned());
        }
    };
    match session.call_tool("add_target", args).await {
        Ok(value) => {
            log_outcome(&value);
            Delivery::Delivered
        }
        Err(McpCallError::Request(message)) => {
            // The session is unusable; reconnect on the next attempt.
            *client = None;
            Delivery::Unreachable(message)
        }
        Err(McpCallError::Tool(message)) => Delivery::ToolRejected(message),
        // The session is alive but the response violated the result
        // convention — retrying would send the same request into the same
        // handler, so treat it as a rejection rather than spooling.
        Err(McpCallError::Malformed(message)) => Delivery::ToolRejected(message),
    }
}

/// The one operator-facing log line per import.
fn log_outcome(value: &serde_json::Value) {
    let slug = value["slug"].as_str().unwrap_or("<unknown>");
    if value["created"].as_bool().unwrap_or(true) {
        info!("imported as {slug} (created)");
    } else {
        info!("updated pending target {slug}");
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    /// The spool line format is a public contract
    /// (spooling.feature pins it in a docstring).
    #[test]
    fn import_request_round_trips_the_pinned_spool_shape() {
        let line = r#"{"ra_hours":4.0,"dec_degrees":40.0,"source":{"kind":"planetarium-bridge","client":"10.0.0.1:1","received_at":"2026-07-30T05:00:00Z"}}"#;
        let request: ImportRequest = serde_json::from_str(line).unwrap();
        assert_eq!(request.ra_hours, 4.0);
        assert_eq!(request.dec_degrees, 40.0);
        assert_eq!(request.source.kind, SOURCE_KIND);
        assert_eq!(request.source.client, "10.0.0.1:1");
        let back = serde_json::to_value(&request).unwrap();
        assert_eq!(back["source"]["received_at"], "2026-07-30T05:00:00Z");
    }

    #[test]
    fn serialized_requests_carry_no_naming_fields() {
        let request = ImportRequest {
            ra_hours: 20.9877,
            dec_degrees: 44.5253,
            source: ImportSource {
                kind: SOURCE_KIND.to_owned(),
                client: "192.0.2.1:54321".to_owned(),
                received_at: "2026-07-29T05:41:12.481Z".to_owned(),
            },
        };
        let value = serde_json::to_value(&request).unwrap();
        let object = value.as_object().unwrap();
        let mut keys: Vec<_> = object.keys().collect();
        keys.sort();
        assert_eq!(keys, vec!["dec_degrees", "ra_hours", "source"]);
    }

    #[test]
    fn health_snapshot_has_the_documented_fields() {
        let health = HealthState::default();
        health.set_reachable(true);
        health.set_spooled(2);
        health.count_replay();
        health.count_drop();
        let snapshot = health.snapshot();
        assert_eq!(snapshot["rp_reachable"], true);
        assert_eq!(snapshot["spooled"], 2);
        assert_eq!(snapshot["replayed_total"], 1);
        assert_eq!(snapshot["dropped_total"], 1);
    }
}
