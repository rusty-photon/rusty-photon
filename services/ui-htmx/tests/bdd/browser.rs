//! Real headless-browser session for the `@browser` BDD scenarios — Layer C of
//! the UI-testing plan ([`docs/plans/archive/ui-testing.md`] §6/§9, obligation P3).
//!
//! geckodriver is treated as an external **system tool** (like OmniSim/ConformU):
//! discovered via `GECKODRIVER_BINARY`, else `geckodriver` on `PATH`. It is
//! spawned on an ephemeral port (no 4444 collisions), in **its own process
//! group** (`process_group(0)`), with `kill_on_drop`, then a headless Firefox is
//! connected through [`thirtyfour`]. Firefox is likewise discovered via
//! `FIREFOX_BINARY` when set, else geckodriver auto-detects the system browser.
//!
//! The process group is load-bearing for teardown (plan §9 Tier 0 step 4 / §10):
//! geckodriver leads the group, Firefox (and its content processes) inherit it,
//! so the whole tree can be reaped with one `killpg` and an orphan check can be
//! scoped to *that group* — it can never match a developer's own Firefox. On
//! Firefox <152 geckodriver's own exit does not reliably tear Firefox down
//! (bugzilla 1430064), so [`BrowserSession::quit`] sweeps the group after the
//! graceful `WebDriver` close, and [`BrowserSession::reap`] is the worst-case
//! kill-the-tree path used when geckodriver has already died.
//!
//! Teardown ordering is also load-bearing (plan §10): [`BrowserSession::quit`]
//! closes the `WebDriver` session — so geckodriver tears Firefox down — and it
//! **must run before** the BFF/driver are stopped (a live session holds
//! connections to the BFF open, which would block the BFF's graceful shutdown and
//! cost it its `.profraw` coverage flush — see `docs/skills/testing.md` §5.4).
//!
//! [`docs/plans/archive/ui-testing.md`]: ../../../../docs/plans/archive/ui-testing.md

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use thirtyfour::prelude::*;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

/// A live browser session: the `WebDriver` client plus the geckodriver child it
/// talks to (which leads the process group the whole browser tree lives in).
pub struct BrowserSession {
    driver: WebDriver,
    geckodriver: Child,
    /// geckodriver's PID. Spawned with `process_group(0)`, so this is also the
    /// **PGID** of the group holding geckodriver + Firefox + content processes —
    /// the single handle the reaper kills and the orphan check scans.
    geckodriver_pid: u32,
}

impl std::fmt::Debug for BrowserSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // WebDriver/Child detail is noise in a World dump; keep the field opaque.
        f.debug_struct("BrowserSession")
            .field("geckodriver_pid", &self.geckodriver_pid)
            .finish_non_exhaustive()
    }
}

impl BrowserSession {
    /// Spawn geckodriver on an ephemeral port and connect a headless Firefox.
    pub async fn start() -> Self {
        let gecko_bin =
            std::env::var("GECKODRIVER_BINARY").unwrap_or_else(|_| "geckodriver".to_string());
        let port = free_port();
        let mut command = Command::new(&gecko_bin);
        command
            .arg("--port")
            .arg(port.to_string())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        // Lead a new process group (pgid == geckodriver's pid): Firefox and its
        // content processes inherit it, so the whole tree is reapable as a unit
        // and the orphan check is scoped to this group (plan §9 step 4 / §10).
        #[cfg(unix)]
        command.process_group(0);
        let mut geckodriver = command
            .spawn()
            .unwrap_or_else(|e| panic!("failed to spawn geckodriver ({gecko_bin:?}): {e}"));
        let geckodriver_pid = geckodriver
            .id()
            .expect("geckodriver has no pid immediately after spawn");

        // Drain geckodriver's stderr into a shared buffer so (a) a startup failure
        // (incompatible Firefox, port already bound, etc.) is reported in the
        // readiness-timeout panic instead of being silently swallowed, and (b) a
        // continuously-read pipe can never fill and block geckodriver mid-test.
        // The task ends at EOF when geckodriver is killed during teardown.
        let stderr_log = Arc::new(Mutex::new(String::new()));
        if let Some(stderr) = geckodriver.stderr.take() {
            let sink = Arc::clone(&stderr_log);
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if let Ok(mut buf) = sink.lock() {
                        buf.push_str(&line);
                        buf.push('\n');
                    }
                }
            });
        }

        let server = format!("http://127.0.0.1:{port}");
        if !wait_for_geckodriver(&server).await {
            // Snapshot the captured stderr (drop the guard before panicking, so a
            // poisoned lock just yields no detail rather than masking the cause).
            let captured = stderr_log
                .lock()
                .map(|buf| buf.trim_end().to_string())
                .unwrap_or_default();
            let detail = if captured.is_empty() {
                "(geckodriver produced no stderr)".to_string()
            } else {
                format!("geckodriver stderr:\n{captured}")
            };
            panic!("geckodriver did not become ready at {server} within 10s; {detail}");
        }

        let mut caps = DesiredCapabilities::firefox();
        caps.set_headless().expect("enable Firefox headless");
        if let Ok(binary) = std::env::var("FIREFOX_BINARY") {
            caps.set_firefox_binary(&binary)
                .expect("set Firefox binary path");
        }

        let driver = WebDriver::new(&server, caps)
            .await
            .expect("connect WebDriver to geckodriver");
        // Implicit wait 0: every browser assertion polls explicitly (a
        // snapshot-once read races the async htmx swap — plan §10).
        driver
            .set_implicit_wait_timeout(Duration::ZERO)
            .await
            .expect("set implicit wait to 0");

        Self {
            driver,
            geckodriver,
            geckodriver_pid,
        }
    }

    /// geckodriver's PID — equal to the PGID of the browser process group.
    pub const fn geckodriver_pid(&self) -> u32 {
        self.geckodriver_pid
    }

    /// Navigate to `url` (top-level; `htmx.min.js` loads and runs).
    pub async fn goto(&self, url: &str) {
        self.driver
            .goto(url)
            .await
            .expect("browser navigation failed");
    }

    /// Poll the **live** DOM (implicit-wait 0, explicit bounded retries) until a
    /// CSS match exists; returns whether it appeared.
    pub async fn wait_present(&self, css: &str, max_iters: usize) -> bool {
        for _ in 0..max_iters {
            if self.driver.find(By::Css(css)).await.is_ok() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        false
    }

    /// Poll until a CSS match exists **and** is enabled; returns success. Used
    /// to observe an htmx swap landing (a disabled field becomes editable).
    pub async fn wait_enabled(&self, css: &str, max_iters: usize) -> bool {
        for _ in 0..max_iters {
            if let Ok(el) = self.driver.find(By::Css(css)).await {
                if el.is_enabled().await.unwrap_or(false) {
                    return true;
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        false
    }

    /// Click the first element matching `css`.
    pub async fn click(&self, css: &str) {
        let element = self
            .driver
            .find(By::Css(css))
            .await
            .expect("element to click not found");
        element.click().await.expect("click failed");
    }

    /// Poll the **live** DOM until the element matching `css` has text containing
    /// `needle`; returns whether it appeared. Used to observe an htmx swap landing
    /// in a region (the main target or an out-of-band sibling).
    pub async fn wait_text_contains(&self, css: &str, needle: &str, max_iters: usize) -> bool {
        for _ in 0..max_iters {
            if let Ok(el) = self.driver.find(By::Css(css)).await {
                if let Ok(text) = el.text().await {
                    if text.contains(needle) {
                        return true;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        false
    }

    /// The current text of the first element matching `css` (empty string if the
    /// element is absent). A point read — callers that need to *wait* should poll
    /// with [`wait_text_contains`](Self::wait_text_contains) first.
    pub async fn text_of(&self, css: &str) -> String {
        match self.driver.find(By::Css(css)).await {
            Ok(el) => el.text().await.unwrap_or_default(),
            Err(_) => String::new(),
        }
    }

    /// The live page source (the current serialized DOM, reflecting htmx swaps) —
    /// used to assert content appears *nowhere* (the dropped-OOB negative case).
    pub async fn page_source(&self) -> String {
        self.driver.source().await.unwrap_or_default()
    }

    /// Poll until the browser's current URL contains `needle`; returns success.
    /// Proves an `HX-Push-Url` history change landed (URL/history is browser-only).
    pub async fn wait_url_contains(&self, needle: &str, max_iters: usize) -> bool {
        for _ in 0..max_iters {
            if let Ok(url) = self.driver.current_url().await {
                if url.as_str().contains(needle) {
                    return true;
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        false
    }

    /// Capture failure-triage artifacts — a PNG screenshot and the live page
    /// source — into `dir` as `<stem>.png` / `<stem>.html`, returning their
    /// **absolute** paths. Call **before** any reap/quit, while the session is
    /// live (plan §9 step 4: "artifact-before-quit"). `dir` must be absolute so
    /// the paths survive the Bazel package-dir chdir — see [`artifact_dir`].
    pub async fn save_failure_artifacts(
        &self,
        dir: &Path,
        stem: &str,
    ) -> Result<(PathBuf, PathBuf), String> {
        let png = self
            .driver
            .screenshot_as_png()
            .await
            .map_err(|e| format!("screenshot failed: {e}"))?;
        let html = self
            .driver
            .source()
            .await
            .map_err(|e| format!("page source failed: {e}"))?;
        std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        let png_path = dir.join(format!("{stem}.png"));
        let html_path = dir.join(format!("{stem}.html"));
        std::fs::write(&png_path, png).map_err(|e| format!("write {}: {e}", png_path.display()))?;
        std::fs::write(&html_path, html)
            .map_err(|e| format!("write {}: {e}", html_path.display()))?;
        Ok((png_path, html_path))
    }

    /// Test-only worst-case trigger: SIGKILL **only** the geckodriver process
    /// (not its group), so it cannot tell Firefox to quit — Firefox is orphaned,
    /// reproducing the non-graceful exit (a panic/timeout, or the Firefox<152
    /// SIGTERM bug, bugzilla 1430064) the reaper must survive. Follow with
    /// [`reap`](Self::reap) to clean up the orphan.
    #[cfg_attr(
        windows,
        expect(
            clippy::unused_self,
            clippy::missing_const_for_fn,
            reason = "the unix-only kill is cfg'd out on Windows; the signature keeps cfg parity"
        )
    )]
    pub fn simulate_geckodriver_crash(&mut self) {
        #[cfg(unix)]
        {
            // SAFETY: `kill` with a valid pid and SIGKILL has no memory concerns.
            unsafe {
                libc::kill(self.geckodriver_pid as libc::pid_t, libc::SIGKILL);
            }
        }
    }

    /// Graceful teardown: close the `WebDriver` session (geckodriver quits Firefox),
    /// sweep the process group as a safety net, then reap geckodriver. Call
    /// **before** stopping the BFF/driver (plan §10).
    pub async fn quit(self) {
        let Self {
            driver,
            mut geckodriver,
            geckodriver_pid,
        } = self;
        // Closes the WebDriver session; geckodriver tears down its Firefox.
        let _ = driver.quit().await;
        // Kill-the-tree safety net: sweep any Firefox/content process still in the
        // group BEFORE reaping geckodriver, so its pid (== the pgid) is still held
        // and can't be recycled mid-kill. Harmless if the graceful quit already
        // closed them; load-bearing on Firefox <152 (bugzilla 1430064).
        reap_group(geckodriver_pid);
        let _ = geckodriver.start_kill();
        let _ = geckodriver.wait().await;
    }

    /// Worst-case kill-the-tree reaper (the plan §9 step 4 / §10 requires it). Used
    /// when geckodriver has already died (the simulated-crash path), so the
    /// `WebDriver` client is simply abandoned — a round-trip would only hang.
    ///
    /// SIGKILLs the whole process group (geckodriver leads it) **and** every
    /// still-tracked pid in `also_kill`. The group kill is the normal path; the
    /// explicit pid kills are belt-and-suspenders for the rare child that escaped
    /// the group (e.g. one that called `setsid`), so no captured process can leak
    /// even if it left the group.
    ///
    /// Deliberately does **not** reap geckodriver: leaving it a zombie keeps its
    /// pid (== the pgid) held, so a group scan run afterwards can't race a recycled
    /// pgid. geckodriver is reaped by `kill_on_drop` when the session is dropped.
    pub fn reap_tree(&mut self, also_kill: &[u32]) {
        reap_group(self.geckodriver_pid);
        for &pid in also_kill {
            kill_pid(pid);
        }
    }
}

/// SIGKILL the whole process group led by `pgid` (geckodriver + Firefox + content
/// processes). A no-op on the rare already-empty group (`killpg` → `ESRCH`).
#[cfg(unix)]
fn reap_group(pgid: u32) {
    // SAFETY: `killpg` with a valid pgid and SIGKILL has no memory concerns.
    unsafe {
        libc::killpg(pgid as libc::pid_t, libc::SIGKILL);
    }
}

#[cfg(not(unix))]
#[expect(
    clippy::unimplemented,
    reason = "deliberate compile-only stub: the @browser layer runs on unix \
              (Linux CI + dev); macOS/Windows support is UI-testing plan §9 \
              step 5, and a silent no-op would mask a porting error"
)]
fn reap_group(_pgid: u32) {
    unimplemented!("process-group reaping is unix-only");
}

/// SIGKILL a single pid — sweeps a process that escaped geckodriver's group. Safe
/// on an already-dead/zombie pid (`kill` → `ESRCH`); the caller only passes pids
/// it captured moments earlier and has not yet waited, so none is recycled.
#[cfg(unix)]
fn kill_pid(pid: u32) {
    // SAFETY: `kill` with a valid pid and SIGKILL has no memory concerns.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
    }
}

#[cfg(not(unix))]
#[expect(
    clippy::unimplemented,
    reason = "deliberate compile-only stub, same rationale as reap_group above"
)]
fn kill_pid(_pid: u32) {
    unimplemented!("pid reaping is unix-only");
}

/// The **live** (non-zombie) PIDs currently in process group `pgid`. Scopes an
/// orphan check to *this* browser tree — it can never match an unrelated Firefox
/// (a developer's own browser lives in a different group). Zombies (state `Z`)
/// are excluded: they are effectively dead, just not yet reaped by their parent.
#[cfg(target_os = "linux")]
pub fn live_pids_in_group(pgid: u32) -> Vec<u32> {
    let mut pids = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return pids;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        }; // numeric pid dirs only
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue; // process exited between readdir and read — already gone
        };
        // /proc/<pid>/stat is `pid (comm) state ppid pgrp ...`. `comm` can contain
        // spaces and parens, so split on the LAST ')': the remaining fields are
        // state(0) ppid(1) pgrp(2) ...
        let Some((_, rest)) = stat.rsplit_once(')') else {
            continue;
        };
        let mut fields = rest.split_whitespace();
        let state = fields.next().unwrap_or("");
        let _ppid = fields.next();
        let pgrp = fields.next().and_then(|s| s.parse::<u32>().ok());
        if state == "Z" {
            continue; // zombie: counts as dead
        }
        if pgrp == Some(pgid) {
            pids.push(pid);
        }
    }
    pids
}

#[cfg(not(target_os = "linux"))]
#[expect(
    clippy::unimplemented,
    reason = "deliberate compile-only stub: the /proc-based orphan scan is \
              Linux-only (the @browser spike runs on Linux/ubuntu); \
              macOS/Windows browser support is UI-testing plan §9 step 5"
)]
pub fn live_pids_in_group(_pgid: u32) -> Vec<u32> {
    unimplemented!("process-group orphan scan is implemented for Linux only");
}

/// Poll until process group `pgid` has no live members (bounded), returning the
/// final survivor list — empty on success. Gives the just-reaped processes a
/// moment to actually die before the assertion reads the result.
pub async fn wait_until_group_drains(pgid: u32, max_iters: usize) -> Vec<u32> {
    for _ in 0..max_iters {
        let live = live_pids_in_group(pgid);
        if live.is_empty() {
            return live;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    live_pids_in_group(pgid)
}

/// The absolute directory failure artifacts are written to. Prefers Bazel's
/// `TEST_UNDECLARED_OUTPUTS_DIR` (an absolute path Bazel sets for test actions
/// and collects into the test outputs), else the OS temp dir — both absolute, so
/// they survive the Bazel package-dir chdir (plan §10), unlike a path relative to
/// the original cwd.
pub fn artifact_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("TEST_UNDECLARED_OUTPUTS_DIR") {
        let path = PathBuf::from(dir);
        if path.is_absolute() {
            return path;
        }
    }
    std::env::temp_dir()
}

/// Whether the BFF flushed a non-empty `.profraw` into `coverage_dir` (Bazel
/// coverage). Mirrors bdd-infra's child path `<COVERAGE_DIR>/ui-htmx-%8m.profraw`
/// — the `%8m` online-merge pool expands to `ui-htmx-<n>.profraw` — and the
/// `ui-htmx-` prefix keeps it distinct from the driver's (`dsd-fp2-*`) pool.
pub fn bff_profraw_flushed(coverage_dir: &OsStr) -> bool {
    let Ok(entries) = std::fs::read_dir(Path::new(coverage_dir)) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        name.starts_with("ui-htmx-")
            && name.ends_with(".profraw")
            // `is_file()` guards against a (pathological) directory of that name:
            // a dir's metadata len is its block size, not 0, so len()>0 alone lies.
            && entry.metadata().is_ok_and(|m| m.is_file() && m.len() > 0)
    })
}

/// Reserve an ephemeral loopback port by binding `:0` and reading it back. A
/// tiny TOCTOU window exists before geckodriver rebinds it; negligible on a test
/// host and far cheaper than parsing geckodriver's stdout.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind an ephemeral port")
        .local_addr()
        .expect("read local_addr")
        .port()
}

/// Poll geckodriver's `/status` until it reports ready (bounded ~10s); returns
/// whether it became ready. The caller surfaces captured stderr on `false`.
async fn wait_for_geckodriver(server: &str) -> bool {
    let status_url = format!("{server}/status");
    for _ in 0..100 {
        if let Ok(resp) = reqwest::Client::new().get(&status_url).send().await {
            if resp.status().is_success() {
                return true;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}
