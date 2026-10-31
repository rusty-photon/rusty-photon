//! BDD World struct and helpers for the plate-solver suite.
//!
//! The world accumulates state across Given / When / Then steps:
//! the spawned wrapper handle, the temp directory holding fixtures and
//! configs, and the most recent HTTP response body / status / timing.

use bdd_infra::tls_auth::{TlsAuthSmokeWorld, TlsAuthState};
use bdd_infra::ServiceHandle;
use cucumber::World;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tempfile::TempDir;

#[derive(Debug, Default, World)]
pub struct PlateSolverWorld {
    /// Handle to the spawned `plate-solver` binary. None until a
    /// Given step starts the wrapper. Stopped in the cucumber `after`
    /// hook in `tests/bdd.rs`.
    pub service_handle: Option<ServiceHandle>,

    /// Per-scenario temp dir holding the config file, FITS placeholder,
    /// and any temp-dir copy of `mock_astap`.
    pub temp_dir: Option<TempDir>,

    /// `astap_binary_path` the wrapper was started with. Some scenarios
    /// rewrite or delete this between steps.
    pub astap_binary_path: Option<PathBuf>,

    /// `astap_db_directory` the wrapper was started with.
    pub astap_db_directory: Option<PathBuf>,

    /// FITS path the most recent solve request used.
    pub fits_path: Option<PathBuf>,

    /// Path that `mock_astap` writes received argv to when
    /// `MOCK_ASTAP_ARGV_OUT` is set on its env.
    pub argv_out_path: Option<PathBuf>,

    /// Directory set as `MOCK_ASTAP_HOLD_DIR` on `mock_astap`'s env for
    /// `hold` mode: a held child records `started-<pid>` there and waits for
    /// the scenario to create `release` in it.
    pub hold_dir_path: Option<PathBuf>,

    /// When the scenario created the `release` file in `hold_dir_path`.
    pub released_at: Option<Instant>,

    /// Mode for the next wrapper spawn (passed via `astap_extra_env`).
    pub mock_astap_mode: Option<String>,

    /// Result of the most recent HTTP request (status + body).
    pub last_response: Option<HttpResponse>,

    /// Elapsed wall time of the most recent request.
    pub last_response_elapsed: Option<Duration>,

    /// Wrapper stderr after exit (configuration scenarios that wait
    /// for the wrapper to exit non-zero).
    pub last_wrapper_stderr: Option<String>,

    /// Wrapper exit status when the scenario waited for the wrapper to
    /// exit (vs. starting it and leaving it running).
    pub last_wrapper_exit_code: Option<i32>,

    /// For the Scenario Outline that POSTs with a single hint set,
    /// step state populated by the "with that `fits_path` and hint X
    /// set to Y" When step.
    pub pending_hint: Option<(String, f64)>,

    /// Solve requests sent in the background, in the order they were sent.
    pub background_requests: Vec<BackgroundRequest>,

    /// Configuration JSON being accumulated by `configuration.feature`'s
    /// composing Given steps. Materialized to disk by the starting When
    /// step.
    pub pending_config: serde_json::Map<String, serde_json::Value>,

    /// State for the shared TLS + auth smoke steps (`auth.feature`).
    pub tls_auth: TlsAuthState,

    /// Doctor-subcommand smoke state (staged config file + run output)
    pub doctor_smoke: bdd_infra::doctor_smoke::DoctorSmokeState,
}

impl bdd_infra::doctor_smoke::DoctorSmokeWorld for PlateSolverWorld {
    fn doctor_smoke(&mut self) -> &mut bdd_infra::doctor_smoke::DoctorSmokeState {
        &mut self.doctor_smoke
    }

    fn valid_config(&self) -> serde_json::Value {
        // The tls-auth smoke's base config plus a plain `server` block.
        let mut config = TlsAuthSmokeWorld::base_test_config(self);
        config["server"] = serde_json::json!({ "port": 0 });
        config
    }
}

impl TlsAuthSmokeWorld for PlateSolverWorld {
    const PROBE_PATH: &'static str = "/health";

    fn tls_auth(&mut self) -> &mut TlsAuthState {
        &mut self.tls_auth
    }

    fn base_test_config(&self) -> serde_json::Value {
        // Reuse the suite's mock_astap plus an empty db dir so startup
        // validation passes without a real ASTAP install. The db dir is
        // kept (not auto-deleted) under the OS temp dir so it still
        // exists when the service starts, matching the lifetime handling
        // of the harness's staged temp config files.
        let mock_path = Self::mock_astap_path();
        let db_dir = tempfile::Builder::new()
            .prefix("plate-solver-auth-db-")
            .tempdir()
            .expect("create db dir")
            .keep();
        serde_json::json!({
            "astap_binary_path": mock_path.to_string_lossy(),
            "astap_db_directory": db_dir.to_string_lossy(),
        })
    }

    async fn start_with_tls_auth(&mut self, config: serde_json::Value) {
        let handle = bdd_infra::tls_auth::spawn_service_handle(
            &mut self.tls_auth,
            env!("CARGO_PKG_NAME"),
            &config,
        )
        .await;
        self.service_handle = Some(handle);
    }
}

#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub body: serde_json::Value,
}

/// A solve request running on its own task, so the scenario can act while
/// it is outstanding.
///
/// The task returns a failed POST rather than panicking, so a scenario that
/// fails and stops the wrapper first leaves no stray panic behind, and
/// dropping the request aborts the task.
#[derive(Debug)]
pub struct BackgroundRequest {
    task: Option<tokio::task::JoinHandle<reqwest::Result<BackgroundAnswer>>>,
    answer: Option<BackgroundAnswer>,
}

#[derive(Debug, Clone)]
pub struct BackgroundAnswer {
    pub response: HttpResponse,
    /// When the answer reached the test, which is never before the wrapper
    /// sent it.
    pub answered_at: Instant,
}

impl BackgroundRequest {
    /// POST `body` to `url` on a new task.
    pub fn spawn(url: String, body: serde_json::Value) -> Self {
        let client = PlateSolverWorld::http_client();
        let task = tokio::spawn(async move {
            let resp = client.post(&url).json(&body).send().await?;
            let status = resp.status().as_u16();
            let bytes = resp.bytes().await?;
            let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
            Ok(BackgroundAnswer {
                response: HttpResponse { status, body },
                answered_at: Instant::now(),
            })
        });
        Self {
            task: Some(task),
            answer: None,
        }
    }

    /// Wait for the answer.
    pub async fn answer(&mut self) -> &BackgroundAnswer {
        if let Some(task) = self.task.take() {
            let answer = task
                .await
                .expect("background request task")
                .expect("background POST");
            self.answer = Some(answer);
        }
        self.answer
            .as_ref()
            .expect("a background request has a task or an answer")
    }
}

impl Drop for BackgroundRequest {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

impl PlateSolverWorld {
    /// Locate the in-tree `mock_astap` binary the same way
    /// `tests/runner_integration.rs` does.
    pub fn mock_astap_path() -> PathBuf {
        if let Ok(p) = std::env::var("MOCK_ASTAP_BINARY") {
            let path = PathBuf::from(p);
            if path.exists() {
                return path;
            }
        }
        if let Some(p) = option_env!("CARGO_BIN_EXE_mock_astap") {
            let path = PathBuf::from(p);
            if path.exists() {
                return path;
            }
        }
        panic!(
            "mock_astap binary not found. Tried MOCK_ASTAP_BINARY env var, then \
             CARGO_BIN_EXE_mock_astap. Run `cargo build --tests -p plate-solver`."
        )
    }

    /// Lazily create the per-scenario temp dir.
    pub fn temp_dir_path(&mut self) -> PathBuf {
        if self.temp_dir.is_none() {
            self.temp_dir = Some(TempDir::new().expect("create temp dir"));
        }
        self.temp_dir.as_ref().unwrap().path().to_path_buf()
    }

    /// Shared suite-wide HTTP client, built once so a poll or a retry
    /// does not pay a fresh connection pool and TCP connect per call.
    ///
    /// The timeout is what bounds a wedged wrapper: without one, a
    /// request that never answers parks inside a single `await` and the
    /// shard dies as an opaque Bazel `TIMEOUT` naming no scenario.
    ///
    /// It has to clear the longest response the wrapper can legitimately
    /// produce, or it preempts a valid `solve_timeout` and reports the
    /// suite's own impatience as a service fault. That bound is set by
    /// the *largest* `max_solve_timeout` any scenario configures, not by
    /// the 2 min default: the real-ASTAP scenarios raise it to 300 s in
    /// `real_astap_steps.rs` so a ~48 s blind solve has room on slower
    /// legs, which puts the worst legitimate response at 300 s + the 2 s
    /// force-kill grace. 360 s clears that, and stays well inside the
    /// `large` test target's 900 s so the named failure is what surfaces.
    /// Raising `max_solve_timeout` in any scenario means raising this
    /// too.
    pub fn http_client() -> reqwest::Client {
        static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
        CLIENT
            .get_or_init(|| {
                reqwest::Client::builder()
                    .timeout(Duration::from_secs(360))
                    .build()
                    .expect("build reqwest client")
            })
            .clone()
    }

    /// Wrapper base URL (e.g., `http://127.0.0.1:11131`). Panics if
    /// the wrapper hasn't been started.
    pub fn wrapper_url(&self) -> String {
        let handle = self
            .service_handle
            .as_ref()
            .expect("wrapper not started — Given step missing?");
        format!("http://127.0.0.1:{}", handle.port())
    }

    /// Build a config pointing at `mock_astap` with the configured
    /// mode (if any), write it under `temp_dir`, and start the wrapper
    /// via `ServiceHandle`. Stores all paths in the world.
    pub async fn start_wrapper_with_mock(&mut self) {
        let mock_path = Self::mock_astap_path();
        let dir = self.temp_dir_path();
        let db_dir = dir.join("db");
        std::fs::create_dir_all(&db_dir).expect("mkdir db");

        let mut extra_env: HashMap<String, String> = HashMap::new();
        if let Some(mode) = self.mock_astap_mode.clone() {
            extra_env.insert("MOCK_ASTAP_MODE".to_string(), mode);
        }
        if let Some(p) = self.argv_out_path.clone() {
            extra_env.insert("MOCK_ASTAP_ARGV_OUT".to_string(), p.display().to_string());
        }
        if let Some(p) = self.hold_dir_path.clone() {
            extra_env.insert("MOCK_ASTAP_HOLD_DIR".to_string(), p.display().to_string());
        }

        self.astap_binary_path = Some(mock_path.clone());
        self.astap_db_directory = Some(db_dir.clone());

        let config_path = write_config(&dir, &mock_path, &db_dir, &extra_env);
        let config_str = config_path.to_string_lossy().into_owned();
        // The wrapper's own debug lines are part of what the supervision
        // scenarios read (`ServiceHandle::stderr_lines`).
        let handle = ServiceHandle::start_with_env(
            env!("CARGO_PKG_NAME"),
            &["--config", &config_str],
            &[("RUST_LOG", "info,plate_solver=debug")],
        )
        .await;
        self.service_handle = Some(handle);
    }

    /// Variant for `health.feature` scenarios that need to mutate the
    /// configured paths after startup. Copies `mock_astap` into the
    /// temp dir and points the config at the copy.
    pub async fn start_wrapper_with_mock_copy(&mut self) {
        let dir = self.temp_dir_path();
        let mock_src = Self::mock_astap_path();
        let mock_dst = dir.join("mock_astap_copy");
        std::fs::copy(&mock_src, &mock_dst).expect("copy mock_astap");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&mock_dst, std::fs::Permissions::from_mode(0o755))
                .expect("chmod mock_astap copy");
        }
        let db_dir = dir.join("db");
        std::fs::create_dir_all(&db_dir).expect("mkdir db");

        self.astap_binary_path = Some(mock_dst.clone());
        self.astap_db_directory = Some(db_dir.clone());

        let extra_env: HashMap<String, String> = HashMap::new();
        let config_path = write_config(&dir, &mock_dst, &db_dir, &extra_env);
        let config_str = config_path.to_string_lossy().into_owned();
        let handle = ServiceHandle::start(env!("CARGO_PKG_NAME"), &config_str).await;
        self.service_handle = Some(handle);
    }

    /// Run the wrapper to completion (rather than leaving it
    /// running). Used by configuration scenarios that assert on
    /// validation-failure exit codes.
    ///
    /// `ServiceHandle` is wrong for this case because it waits for
    /// `bound_addr=` on stdout, which never arrives if the wrapper
    /// exits during config validation. `bdd_infra::run_once_async` runs
    /// it to completion instead: it finds the binary as `ServiceHandle`
    /// does, and runs it off the poll loop every scenario shares.
    pub async fn run_wrapper_to_exit(&mut self, config_path: PathBuf) {
        let config = config_path.to_string_lossy().into_owned();
        let output =
            bdd_infra::run_once_async(env!("CARGO_PKG_NAME"), &["--config", &config], None).await;
        self.last_wrapper_exit_code = Some(output.status.code().unwrap_or(-1));
        self.last_wrapper_stderr = Some(String::from_utf8_lossy(&output.stderr).into_owned());
    }
}

/// Write a JSON config to `dir/config.json` and return the path.
fn write_config(
    dir: &std::path::Path,
    binary_path: &std::path::Path,
    db_directory: &std::path::Path,
    extra_env: &HashMap<String, String>,
) -> PathBuf {
    let body = serde_json::json!({
        "server": {
            "bind_address": "127.0.0.1",
            "port": 0,  // OS picks a free port; ServiceHandle parses it from stdout
        },
        "astap_binary_path": binary_path.to_string_lossy(),
        "astap_db_directory": db_directory.to_string_lossy(),
        "astap_extra_env": extra_env,
    })
    .to_string();
    let p = dir.join("config.json");
    std::fs::write(&p, body).expect("write config");
    p
}
