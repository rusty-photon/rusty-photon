//! Cucumber `World` for the `ui-htmx` BDD suite.
//!
//! Mirrors the binary-spawning pattern the other services use (see
//! [`sentinel`](../../../../sentinel/tests/bdd/world.rs)): every scenario
//! spawns the *real* `ui-htmx` binary, plus a real `rp` orchestrator where
//! the scenario reads one, and drives the BFF over HTTP. There is no
//! in-process router and no stubbed client: the production clients reach rp's
//! REST, SSE and MCP surfaces end to end.
//!
//! Requires the binaries pre-built with `--all-features` (the BFF's test-only
//! fixture routes are feature-gated): `cargo build --all-features --all-targets`.

use std::path::PathBuf;
use std::time::Duration;

use bdd_infra::ServiceHandle;
use cucumber::World;
use serde_json::{json, Value};
use tempfile::TempDir;

use crate::browser::BrowserSession;
use crate::dom;

/// A reserved, low-numbered loopback port that reliably refuses connections —
/// the repo's convention for an unreachable target (see ui-htmx's `io.rs`
/// test and rp's BDD steps). Deterministic, unlike a released free port.
pub const UNREACHABLE_PORT: u16 = 1;

#[derive(Debug, Default, World)]
pub struct UiWorld {
    /// The BFF under test.
    pub ui: Option<ServiceHandle>,
    /// The BFF's temp config file, which its setup page saves to.
    pub ui_config_path: Option<PathBuf>,
    temp_dir: Option<TempDir>,
    /// The rendered HTML of the last BFF response. Kept as a `String` (never a
    /// parsed DOM): the `!Send` `scraper::Html` is built, queried, and dropped
    /// inside the synchronous [`crate::dom`] helpers, so it never crosses an
    /// `.await` (see [`crate::dom`] and the UI-testing plan §4).
    pub last_body: String,
    /// The URL of the last top-level page navigation, reported as `HX-Current-URL`
    /// on subsequent htmx requests so the captured fragments match what a browser
    /// would send.
    current_url: String,
    /// The real headless-browser session, lazily started by the first
    /// `@browser` step (Layer C / P3). Absent for the default, browser-free suite.
    pub browser: Option<BrowserSession>,
    /// How long `ServiceHandle::stop()` took to bring the BFF down after the
    /// browser was quit. The coverage-invariant scenario (plan §9 Tier 0 step 3)
    /// asserts this is well under the 5s SIGKILL grace — a graceful exit ran the
    /// BFF's `atexit`, so its `.profraw` coverage was flushed (testing.md §5.4).
    pub bff_stop_elapsed: Option<Duration>,
    /// Live PIDs in the browser process group recorded just before the simulated
    /// geckodriver crash (≥ geckodriver + Firefox) — non-empty proves the reaper
    /// had real work to do (plan §9 Tier 0 step 4).
    pub session_pids_before: Vec<u32>,
    /// Live PIDs left in the browser process group after the reaper ran — must be
    /// empty (no orphaned geckodriver/Firefox/content processes survive).
    pub orphan_survivors: Vec<u32>,
    /// The absolute screenshot + page-source paths captured before the reap.
    pub artifacts: Option<(PathBuf, PathBuf)>,
    /// The rp orchestrator the stream and targets suites spawn. Absent for
    /// the scenarios that read no rp.
    pub rp: Option<ServiceHandle>,
    /// rp's OS-assigned port (rp binds `:0`).
    pub rp_port: u16,
    /// The stub Alpaca safety monitor rp polls in the activity-stream
    /// scenarios: flipping it is how the harness provokes rp events on an
    /// empty roster (rp keeps no session to start or stop).
    pub safety_stub: Option<bdd_infra::rp_harness::AlpacaDeviceStub>,
    /// A live reader of the BFF's `/stream/events` SSE proxy. Dropped in the
    /// `after` hook **before** the BFF stops (testing.md §5.4 — an open stream
    /// blocks graceful shutdown and silently loses subprocess coverage).
    pub sse: Option<crate::sse_client::StreamEventsClient>,
    /// The SSE `id` of the first feed frame, captured for the replay-cursor
    /// scenario.
    pub sse_cursor: Option<u64>,
    /// Shared PKI + credentials fixture for the TLS + auth suite
    /// (`auth.feature`, `tls.feature`).
    pub pki: Option<std::sync::Arc<bdd_infra::tls_auth::PkiFixture>>,
    /// Config JSON staged by a Given step for a custom-config BFF start.
    pub pending_config: Option<Value>,
    /// Doctor-subcommand smoke state (staged config file + run output).
    pub doctor_smoke: bdd_infra::doctor_smoke::DoctorSmokeState,
    /// rp's pinned `session.data_directory` for scenarios that restart rp
    /// over the same target store (the stale-goal roster flag). `None`
    /// until a targets-suite Given pins it.
    rp_data_dir: Option<PathBuf>,
    /// The setup-page steps' state (`setup_pages.feature`).
    pub setup_pages: bdd_infra::setup_pages::SetupPagesState,
}

impl bdd_infra::setup_pages::SetupPagesWorld for UiWorld {
    fn setup_pages(&mut self) -> &mut bdd_infra::setup_pages::SetupPagesState {
        &mut self.setup_pages
    }
}

impl bdd_infra::doctor_smoke::DoctorSmokeWorld for UiWorld {
    fn doctor_smoke(&mut self) -> &mut bdd_infra::doctor_smoke::DoctorSmokeState {
        &mut self.doctor_smoke
    }

    fn valid_config(&self) -> serde_json::Value {
        // The same shape `start_bff_with_rp_at` stages: the shared core
        // `server` block plus the required rp target (doctor only parses —
        // nothing is contacted).
        json!({
            "server": { "bind_address": "127.0.0.1", "port": 0 },
            "rp": {}
        })
    }
}

impl UiWorld {
    fn temp_path(&mut self, file: &str) -> PathBuf {
        self.temp_dir
            .get_or_insert_with(|| TempDir::new().expect("failed to create temp dir"))
            .path()
            .join(file)
    }

    // --- spawning real services -------------------------------------------

    /// Spawn just the BFF — no rp process — for the `/fixtures/*`
    /// scenarios (plan §9 Tier 1). The fixtures never talk to rp, so the
    /// configured (unreachable) rp target is never contacted; this is the
    /// lightest setup that brings up a fixtures-capable BFF. The spawned
    /// binary carries the `test-fixtures` feature (cargo `--all-features`;
    /// Bazel `:ui-htmx_fixtures`).
    pub async fn start_bff_only(&mut self) {
        self.start_bff_with_rp_at(UNREACHABLE_PORT).await;
    }

    /// Spawn the BFF from the config JSON a Given step staged in
    /// `pending_config` (the TLS + auth suite: `auth.feature`, `tls.feature`).
    pub async fn start_bff_from_pending_config(&mut self) {
        let config = self.pending_config.take().expect("config not staged");
        let path = self.temp_path("ui-htmx-tls-auth.json");
        std::fs::write(&path, config.to_string()).expect("failed to write BFF config");
        let handle = ServiceHandle::start("ui-htmx", path.to_str().unwrap()).await;
        self.ui = Some(handle);
    }

    pub async fn start_rp(&mut self, builder: &bdd_infra::rp_harness::RpConfigBuilder) {
        let config = builder.build();
        let path = self.temp_path("rp.json");
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&config).expect("serialize rp config"),
        )
        .expect("failed to write rp config");
        let handle = ServiceHandle::start("rp", path.to_str().unwrap()).await;
        assert!(
            bdd_infra::rp_harness::wait_for_rp_healthy(&handle.base_url()).await,
            "rp did not become healthy"
        );
        self.rp_port = handle.port();
        self.rp = Some(handle);
    }

    /// Spawn rp with an empty roster (site configured so site-field validation
    /// scenarios have something to reject).
    pub async fn start_rp_with_empty_roster(&mut self) {
        let mut builder = bdd_infra::rp_harness::RpConfigBuilder::new();
        builder.with_site(47.6, -122.3);
        self.start_rp(&builder).await;
    }

    /// Spawn rp with an otherwise empty roster and one stub safety monitor,
    /// polled fast (250 ms) so a flip lands in test time. The stub starts
    /// safe.
    pub async fn start_rp_with_safety_monitor(&mut self) {
        let stub = bdd_infra::rp_harness::AlpacaDeviceStub::start(
            bdd_infra::rp_harness::StubDevice::SafetyMonitor,
        );
        let mut builder = bdd_infra::rp_harness::RpConfigBuilder::new();
        builder.with_site(47.6, -122.3);
        builder.add_safety_monitor(bdd_infra::rp_harness::SafetyMonitorConfig {
            id: "weather-watcher".to_string(),
            alpaca_url: stub.url(),
            device_number: 0,
        });
        builder.with_safety_poll_interval(Duration::from_millis(250));
        self.safety_stub = Some(stub);
        self.start_rp(&builder).await;
    }

    /// Flip the stub safety monitor and wait until rp's `get_safety_status`
    /// reports the transition — the poll has landed and the
    /// `safety_changed` event is on the bus — so two flips in a row are
    /// never collapsed into none by the poll interval.
    pub async fn set_safety(&self, safe: bool) {
        self.safety_stub
            .as_ref()
            .expect("no stub safety monitor — start rp with one")
            .set_is_safe(safe);
        let expected = if safe { "safe" } else { "unsafe" };
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            // Each poll is bounded by what is left of the budget, so an
            // rp that stops answering fails the step at the deadline
            // rather than at the harness's minutes-long MCP timeout.
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let status =
                tokio::time::timeout(remaining, self.rp_tool("get_safety_status", json!({})))
                    .await
                    .unwrap_or_else(|_| {
                        panic!("get_safety_status did not answer before the {expected} deadline")
                    })
                    .expect("get_safety_status is ungated and must answer");
            if status.get("overall").and_then(Value::as_str) == Some(expected) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "rp never reported the safety monitor as {expected}: {status}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Spawn a BFF on port 0 whose `rp` target is `127.0.0.1:{rp_port}`,
    /// from a temp config file kept in [`ui_config_path`](Self::ui_config_path).
    pub async fn start_bff_with_rp_at(&mut self, rp_port: u16) {
        let config = json!({
            "server": { "bind_address": "127.0.0.1", "port": 0 },
            "rp": { "base_url": format!("http://127.0.0.1:{rp_port}") }
        });
        let path = self.temp_path("ui-htmx.json");
        std::fs::write(&path, config.to_string()).expect("failed to write BFF config");
        let handle = ServiceHandle::start("ui-htmx", path.to_str().unwrap()).await;
        self.ui = Some(handle);
        self.ui_config_path = Some(path);
    }

    /// Spawn a BFF pointed at the running rp.
    pub async fn start_bff_with_rp(&mut self) {
        assert!(self.rp.is_some(), "start rp before the BFF");
        self.start_bff_with_rp_at(self.rp_port).await;
    }

    /// Spawn a BFF pointed at an rp that is not running.
    pub async fn start_bff_with_unreachable_rp(&mut self) {
        self.start_bff_with_rp_at(UNREACHABLE_PORT).await;
    }

    /// Spawn rp with one imaging train (an unconnectable camera entry —
    /// target CRUD and the config joins need no live device) carrying the
    /// given `default_position_angle_degrees` — the targets suite's
    /// inherit-hint fixture.
    pub async fn start_rp_with_imaging_train(&mut self, default_angle: f64) {
        let mut builder = bdd_infra::rp_harness::RpConfigBuilder::new();
        builder.with_site(47.6, -122.3);
        builder.add_camera(bdd_infra::rp_harness::CameraConfig {
            id: "main-cam".to_string(),
            alpaca_url: format!("http://127.0.0.1:{UNREACHABLE_PORT}"),
            device_number: 0,
            cooler_targets_c: Vec::new(),
        });
        builder.add_optical_train(bdd_infra::rp_harness::OpticalTrainConfig {
            telescope: None,
            aperture_mm: None,
            id: "main".to_string(),
            purpose: Some("imaging".to_string()),
            focal_length_mm: None,
            default_position_angle_degrees: Some(default_angle),
            devices: vec!["main-cam".to_string()],
            auto_focus: None,
        });
        self.start_rp(&builder).await;
    }

    /// Spawn rp with one filter wheel (unconnectable — the roster union is
    /// config-level) listing `filters`, over a **pinned** data directory so
    /// [`restart_rp_with_filters`](Self::restart_rp_with_filters) reopens
    /// the same target store.
    pub async fn start_rp_with_filters(&mut self, filters: &[String]) {
        let builder = self.filters_builder(filters);
        self.start_rp(&builder).await;
    }

    /// Stop the running rp and start a fresh one whose filter wheel lists
    /// `filters`, over the same pinned data directory — the only way a
    /// stored goal can come to reference a filter outside the roster,
    /// since rp validates goals at write time.
    pub async fn restart_rp_with_filters(&mut self, filters: &[String]) {
        let mut rp = self.rp.take().expect("rp not started");
        rp.stop().await;
        let builder = self.filters_builder(filters);
        self.start_rp(&builder).await;
    }

    fn filters_builder(&mut self, filters: &[String]) -> bdd_infra::rp_harness::RpConfigBuilder {
        let data_dir = self
            .rp_data_dir
            .get_or_insert_with(|| {
                let dir = self
                    .temp_dir
                    .get_or_insert_with(|| TempDir::new().expect("failed to create temp dir"))
                    .path()
                    .join("rp-data");
                std::fs::create_dir_all(&dir).expect("failed to create rp data dir");
                dir
            })
            .clone();
        let mut builder = bdd_infra::rp_harness::RpConfigBuilder::new();
        builder.with_site(47.6, -122.3);
        builder.with_data_directory(data_dir.to_str().unwrap());
        builder.add_filter_wheel(bdd_infra::rp_harness::FilterWheelConfig {
            wavelengths_nm: vec![],
            id: "wheel".to_string(),
            alpaca_url: format!("http://127.0.0.1:{UNREACHABLE_PORT}"),
            device_number: 0,
            filters: filters.to_vec(),
        });
        builder
    }

    // --- seeding and inspecting rp's target store over MCP -----------------

    /// Call one of rp's target MCP tools on a fresh session (connect, call,
    /// drop — the BFF's own per-request pattern), returning the parsed
    /// result or the tool-error message.
    pub async fn rp_tool(&self, tool: &str, args: Value) -> Result<Value, String> {
        let rp = self.rp.as_ref().expect("rp not started");
        let client =
            bdd_infra::rp_harness::McpTestClient::connect(&format!("{}/mcp", rp.base_url()))
                .await
                .expect("MCP connect to rp failed");
        client.call_tool(tool, args).await
    }

    /// Seed a target through rp's own `add_target` (the operator form).
    pub async fn add_target_via_mcp(&self, params: Value) {
        self.rp_tool("add_target", params)
            .await
            .expect("add_target failed");
    }

    /// Fetch one target's row through `get_target`.
    pub async fn target_via_mcp(&self, slug: &str) -> Result<Value, String> {
        self.rp_tool("get_target", json!({ "slug": slug }))
            .await
            .map(|v| v.get("target").cloned().expect("get_target without target"))
    }

    /// Connect a reader to the BFF's `/stream/events` SSE proxy.
    pub async fn connect_stream_events(&mut self, last_event_id: Option<u64>) {
        let url = self.ui_url("/stream/events");
        self.sse = Some(crate::sse_client::StreamEventsClient::connect(&url, last_event_id).await);
    }

    // --- driving the BFF over HTTP ----------------------------------------

    fn ui_url(&self, path: &str) -> String {
        let ui = self.ui.as_ref().expect("BFF not started");
        format!("{}{}", ui.base_url(), path)
    }

    /// The `HX-*` request headers htmx attaches to a swap request, so a captured
    /// fragment is what a browser would actually receive. The targets pages'
    /// affordances all declare `hx-target="#targets-page"`; htmx sends the
    /// resolved element id (no leading `#`).
    fn hx_headers(&self) -> reqwest::header::HeaderMap {
        use reqwest::header::{HeaderMap, HeaderValue};
        let mut headers = HeaderMap::new();
        headers.insert("HX-Request", HeaderValue::from_static("true"));
        headers.insert("HX-Target", HeaderValue::from_static("targets-page"));
        if let Ok(value) = HeaderValue::from_str(&self.current_url) {
            headers.insert("HX-Current-URL", value);
        }
        headers
    }

    /// GET a BFF page as a top-level browser navigation: no `HX-*` headers, so
    /// the server returns the full styled page. Captures the rendered HTML and
    /// records the URL for `HX-Current-URL` on any follow-up htmx request.
    pub async fn get(&mut self, path: &str) {
        let url = self.ui_url(path);
        let resp = reqwest::Client::new()
            .get(&url)
            .send()
            .await
            .expect("BFF GET failed");
        self.current_url = url;
        self.last_body = resp.text().await.unwrap_or_default();
    }

    /// [`get`](Self::get) with the `Host` header a browser that reached the
    /// BFF by `host` would send — the host the navigation's link to rp's
    /// setup page carries when rp's address is a loopback one.
    pub async fn get_as_host(&mut self, path: &str, host: &str) {
        let url = self.ui_url(path);
        let resp = reqwest::Client::new()
            .get(&url)
            .header(reqwest::header::HOST, host)
            .send()
            .await
            .expect("BFF GET failed");
        self.current_url = url;
        self.last_body = resp.text().await.unwrap_or_default();
    }

    /// Fetch a `/fixtures/*` swap endpoint as an htmx request and return its
    /// `(HX-* response header value, body)` for the named header — the §A
    /// "header-presence tripwire" (plan §9 Tier 1). It proves the divergence-
    /// carrying signal (e.g. `HX-Retarget`) is observable in the *response* even
    /// though the body bytes are a plain fragment a P2 snapshot couldn't tell apart
    /// from a normal swap; the browser then proves it actually retargets.
    pub async fn fixture_response_header_and_body(
        &self,
        path: &str,
        header: &str,
    ) -> (Option<String>, String) {
        let resp = reqwest::Client::new()
            .get(self.ui_url(path))
            .header("HX-Request", "true")
            .send()
            .await
            .expect("fixture GET failed");
        let header_value = resp
            .headers()
            .get(header)
            .and_then(|v| v.to_str().ok())
            .map(String::from);
        let body = resp.text().await.unwrap_or_default();
        (header_value, body)
    }

    /// POST an htmx affordance URL directly (e.g. a target row's Activate
    /// button) and capture the swapped fragment.
    pub async fn post_htmx(&mut self, path: &str) {
        let resp = reqwest::Client::new()
            .post(self.ui_url(path))
            .headers(self.hx_headers())
            .send()
            .await
            .expect("BFF POST failed");
        self.last_body = resp.text().await.unwrap_or_default();
    }

    /// Submit the form in the **already-rendered** `last_body` the way
    /// htmx would: the rendered hidden values and enabled controls, with the
    /// operator's edits replacing (or adding) the named values, `POST`ed to the
    /// form's own `hx-post` URL with htmx's headers.
    pub async fn submit_rendered_form(&mut self, changes: &[(&str, &str)]) {
        let action =
            dom::form_post_url(&self.last_body).expect("rendered page has no <form hx-post>");
        let mut pairs = dom::successful_controls(&self.last_body);
        // The operator edits one or more enabled fields before submitting:
        // replace the rendered value with the typed one (or add it if absent).
        for &(name, value) in changes {
            pairs.retain(|(n, _)| n.as_str() != name);
            pairs.push((name.to_string(), value.to_string()));
        }
        let resp = reqwest::Client::new()
            .post(self.ui_url(&action))
            .headers(self.hx_headers())
            .form(&pairs)
            .send()
            .await
            .expect("BFF POST failed");
        self.last_body = resp.text().await.unwrap_or_default();
    }

    // --- driving a real browser (@browser scenarios, Layer C) -------------

    /// Lazily start the headless browser session (geckodriver + Firefox) the
    /// first time an `@browser` step needs it.
    pub async fn ensure_browser(&mut self) {
        if self.browser.is_none() {
            self.browser = Some(BrowserSession::start().await);
        }
    }

    /// The live browser session (panics if no `@browser` step started one).
    pub const fn browser(&self) -> &BrowserSession {
        self.browser
            .as_ref()
            .expect("browser session not started — is this an @browser scenario?")
    }

    /// Navigate the real browser to a BFF page as a top-level navigation, so
    /// `htmx.min.js` is fetched and executed.
    pub async fn browser_goto(&mut self, path: &str) {
        self.ensure_browser().await;
        let url = self.ui_url(path);
        self.browser().goto(&url).await;
    }

    /// Tear down in the **correct** order — browser first, then the BFF — and time
    /// the BFF stop (plan §9 Tier 0 step 3). Quitting the browser first closes its
    /// `WebDriver` session (and so Firefox's held connections to the BFF), so the
    /// BFF's graceful shutdown completes promptly instead of blocking until the
    /// 5s SIGKILL grace and losing its `.profraw` coverage flush (testing.md
    /// §5.4). Both handles are taken, so the `after`-hook teardown skips them.
    pub async fn quit_browser_then_stop_bff(&mut self) {
        if let Some(browser) = self.browser.take() {
            browser.quit().await;
        }
        let mut ui = self.ui.take().expect("BFF not started");
        let start = std::time::Instant::now();
        ui.stop().await;
        self.bff_stop_elapsed = Some(start.elapsed());
    }

    /// Worst-case browser teardown (plan §9 Tier 0 step 4): capture failure
    /// artifacts while the session is live, simulate a geckodriver crash that
    /// orphans Firefox, then reap the whole process group — recording the group's
    /// membership before the crash and the survivors after the reap. The browser
    /// handle is taken (reaped, not gracefully quit), so the `after`-hook skips it.
    pub async fn crash_and_reap_browser(&mut self) {
        let mut session = self
            .browser
            .take()
            .expect("no browser session — is this an @browser scenario?");
        let pgid = session.geckodriver_pid();
        // Artifact-before-quit: capture the screenshot + page source while the
        // session is still live, at an absolute (chdir-safe) path. Keep the Result
        // — do NOT unwrap here, so a capture failure can't panic *before* the reap
        // and orphan the tree; the Then-step asserts the artifacts landed.
        let dir = crate::browser::artifact_dir();
        let artifacts = session
            .save_failure_artifacts(&dir, "ui-htmx-panic-recovery")
            .await;
        // Record our whole tree (geckodriver + Firefox + content) while it is live
        // and still in the group.
        let before = crate::browser::live_pids_in_group(pgid);
        session.simulate_geckodriver_crash(); // SIGKILL geckodriver → Firefox orphaned
                                              // Kill the tree: the group (killpg) plus every captured pid (covers a child
                                              // that escaped the group). geckodriver is left a zombie holding the pgid, so
                                              // the drain scan below can't race a recycled pgid; it is reaped by
                                              // kill_on_drop when `session` drops at the end of this method.
        session.reap_tree(&before);
        // ~10s bounded: SIGKILLed processes take a moment to become reaped zombies,
        // and a loaded CI host is slower (the scan skips zombies, so a drained
        // group reads empty). Recycle-safe: geckodriver's zombie still holds pgid.
        let survivors = crate::browser::wait_until_group_drains(pgid, 100).await;
        self.artifacts = artifacts.ok();
        self.session_pids_before = before;
        self.orphan_survivors = survivors;
    }
}
