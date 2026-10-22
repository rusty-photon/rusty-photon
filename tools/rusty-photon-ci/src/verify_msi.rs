//! `cargo ci verify-msi`: proves the suite MSI through its whole lifecycle
//! before it ships (docs/packaging-windows.md).
//!
//! A silent install of every feature, each kind of service's checks, the
//! failure actions, the removal of one feature, and the uninstall, which
//! keeps the configs and logs, as a deb's `remove` does. It runs elevated, on a Windows box with no rusty-photon install and no
//! `%ProgramData%\rusty-photon`, and refuses any other: windows-latest in
//! CI, never an imaging box. The kinds of service differ as they do on Linux
//! ([`crate::verify_packages`]):
//! - every service is installed, demand-start when it is [`GATED`] and
//!   automatic otherwise, to restart 5 s after a failure with the
//!   failure-actions flag set: a serial driver's exit on its absent device is
//!   a clean error exit, which Windows restarts only with the flag;
//! - a [`GATED`] service stays stopped, with no config;
//! - a [`SERIAL`] driver writes its config and logs its handshake attempt,
//!   and the first of them a second attempt, which only a restart makes;
//! - qhy-camera either starts or logs that `qhyccd.dll`, the operator's, is
//!   missing: anything else is the loader crash its delay-load link
//!   prevents;
//! - the rest run and answer their port, phd2-guider's `/health` with 503
//!   when there is no PHD2 to talk to; the ones that write a config write
//!   it, and ui-htmx's names rp and no drivers;
//! - sentinel, killed, is restarted; filemonitor takes a `ParamChange`, its
//!   reload; doctor runs, and its renewal task is registered;
//! - removing the `ZwoCamera` feature takes zwo-camera, its exe and its DLL,
//!   and leaves zwo-focuser running with its DLL and the ZWO license both
//!   install.
//!
//! With `--upgrade-from`, it first installs that MSI, an earlier nightly's,
//! waits for sentinel's config, disables and stops its services and clears
//! their logs, so the install under test is an upgrade in place over the
//! configs they left. Only this proves the nightly channel's same-version
//! upgrade (docs/packaging-windows.md, Nightly channel). After the upgrade
//! one Programs and Features entry must remain, the new MSI's, and the
//! shipped doctor's `--fix`, the operator's migration path, must leave no
//! retired config key and provision TLS and the observatory credential. The
//! checks then probe each service as its config says, over HTTPS with the
//! credential where `--fix` turned them on.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use crate::packaging::{self, io, GATED, HEALTH_PROBED, NO_CONFIG_FILE, SERIAL};
use crate::process::{self, Clock, Cmd, Runner};

/// The MSI a run verifies, and how.
#[derive(Clone, Debug, Default)]
pub struct Request {
    /// The MSI; `dist/<v>/rusty-photon-<v>-x64.msi` for the workspace's
    /// version `<v>` when `None`.
    pub msi: Option<PathBuf>,
    /// An earlier MSI to install first, so the one under test upgrades it.
    pub upgrade_from: Option<PathBuf>,
    /// Leave the product installed, to debug.
    pub keep: bool,
}

/// Where the MSI puts things, and where the run keeps msiexec's logs.
#[derive(Clone, Debug)]
pub struct Places {
    /// `%ProgramData%\rusty-photon`: the configs, the PKI and `logs`.
    pub data: PathBuf,
    /// `%ProgramFiles%\rusty-photon`: the exes and DLLs.
    pub install: PathBuf,
    /// `%TEMP%`.
    pub temp: PathBuf,
}

impl Places {
    /// This machine's.
    #[must_use]
    pub fn this() -> Self {
        let var = |name: &str| {
            std::env::var_os(name)
                .map(PathBuf::from)
                .unwrap_or_default()
        };
        Self {
            data: var("ProgramData").join("rusty-photon"),
            install: var("ProgramFiles").join("rusty-photon"),
            temp: var("TEMP"),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Packaging(#[from] packaging::Error),
    #[error(transparent)]
    Process(#[from] process::Error),
    #[error("{0}")]
    Refused(String),
    #[error("FAIL [{service}]: {reason}")]
    Failed { service: String, reason: String },
}

fn refused(reason: impl Into<String>) -> Error {
    Error::Refused(reason.into())
}

/// A Windows service's state, as `sc.exe` numbers it.
const RUNNING: u32 = 4;
const STOPPED: u32 = 1;

/// The Programs and Features entries of the product, one `entry:<Comments>`
/// line each. An x64 MSI registers in the native hive, which a 64-bit
/// PowerShell reads.
const ARP: &str = "Get-ChildItem 'HKLM:\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Uninstall' \
                   | ForEach-Object { Get-ItemProperty $_.PSPath } \
                   | Where-Object { $_.DisplayName -eq 'Rusty Photon' } \
                   | ForEach-Object { 'entry:' + $_.Comments }";

/// Every installed `rusty-photon-*` service, a name a line.
const SERVICES: &str = "Get-Service -Name 'rusty-photon-*' | ForEach-Object { $_.Name }";

fn service_name(service: &str) -> String {
    format!("rusty-photon-{service}")
}

/// A service's state and process ID, from `sc.exe queryex`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Status {
    state: u32,
    pid: u32,
}

/// The number that starts the value of `field` in `sc.exe`'s output, as in
/// `STATE              : 4  RUNNING`.
fn sc_number(output: &str, field: &str) -> Option<u32> {
    output.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if name.trim() != field {
            return None;
        }
        value.split_whitespace().next()?.parse().ok()
    })
}

/// A service's start type, from `sc.exe qc`: `START_TYPE : 2 AUTO_START`,
/// with `(DELAYED)` after it for a delayed start.
fn start_type(output: &str) -> &'static str {
    let delayed = output.contains("(DELAYED)");
    match sc_number(output, "START_TYPE") {
        Some(2) if delayed => "AutomaticDelayedStart",
        Some(2) => "Automatic",
        Some(3) => "Manual",
        Some(4) => "Disabled",
        _ => "unknown",
    }
}

/// A log file's text: msiexec writes UTF-16 where it starts one with that
/// byte order mark.
fn text_of(bytes: &[u8]) -> String {
    match bytes {
        [0xff, 0xfe, rest @ ..] => {
            let units: Vec<u16> = rest
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| u16::from_le_bytes(*pair))
                .collect();
            String::from_utf16_lossy(&units)
        }
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// The last `count` lines of `text`.
fn last_lines(text: &str, count: usize) -> Vec<&str> {
    let lines: Vec<&str> = text.lines().collect();
    lines
        .get(lines.len().saturating_sub(count)..)
        .unwrap_or_default()
        .to_vec()
}

/// What a command printed, after a colon, for the reason it failed; nothing
/// when it printed nothing.
fn printed(output: &str) -> String {
    let output = output.trim();
    if output.is_empty() {
        String::new()
    } else {
        format!(": {output}")
    }
}

/// Whether a line of a verbose msiexec log says what failed: the action
/// that returned 3, an error's number, a custom action's error, a service
/// that did not start.
fn says_what_failed(line: &str) -> bool {
    let lower = line.to_lowercase();
    let numbered = |after: &str| {
        lower.match_indices(after).any(|(at, _)| {
            lower
                .get(at.saturating_add(after.len())..)
                .and_then(|rest| rest.bytes().next())
                .is_some_and(|byte| byte.is_ascii_digit())
        })
    };
    lower.contains("return value 3")
        || (lower.starts_with("error ") && numbered("error "))
        || numbered("error 1")
        || (lower.contains("customaction ") && lower.contains(" returned actual error"))
        || lower.contains("failed to start")
        || lower.contains("could not be")
        || lower.contains("mainenginethread is returning")
}

/// The full version an MSI's file name carries:
/// `rusty-photon-<version>-x64.msi`.
fn msi_version(msi: &Path) -> Option<String> {
    let name = msi.file_name()?.to_str()?;
    let version = name
        .strip_prefix("rusty-photon-")?
        .strip_suffix("-x64.msi")?;
    (!version.is_empty()).then(|| version.to_owned())
}

struct Run<'a, R: Runner, C: Clock> {
    runner: &'a mut R,
    clock: &'a mut C,
    places: &'a Places,
    /// The install log a failure shows: the earlier MSI's while it installs.
    install_log: PathBuf,
    uninstall_log: PathBuf,
    /// The curl config that carries the observatory credential, once doctor
    /// has provisioned it, so the credential is in no command line.
    credential: Option<tempfile::NamedTempFile>,
    /// Whether an earlier MSI was installed first.
    upgraded: bool,
}

impl<R: Runner, C: Clock> Run<'_, R, C> {
    fn logs(&self) -> PathBuf {
        self.places.data.join("logs")
    }

    fn config(&self, service: &str) -> PathBuf {
        self.places.data.join(format!("{service}.json"))
    }

    /// The text of `service`'s newest log, its last `count` lines.
    fn log_tail(&self, service: &str, count: usize) -> String {
        let prefix = format!("{service}.");
        let newest = fs::read_dir(self.logs())
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(&prefix))
            .max_by_key(|entry| entry.metadata().and_then(|meta| meta.modified()).ok());
        newest
            .and_then(|entry| fs::read(entry.path()).ok())
            .map(|bytes| last_lines(&text_of(&bytes), count).join("\n"))
            .unwrap_or_default()
    }

    /// The service's failure, with the end of its log and what msiexec's
    /// logs say failed on stderr.
    fn fail(&self, service: &str, reason: &str) -> Error {
        eprintln!("verify-msi: FAIL [{service}]: {reason}");
        let tail = self.log_tail(service, 40);
        if !tail.is_empty() {
            eprintln!("--- the last 40 lines of {service}'s log ---\n{tail}");
        }
        for log in [&self.uninstall_log, &self.install_log] {
            let Ok(bytes) = fs::read(log) else {
                continue;
            };
            let text = text_of(&bytes);
            let failed: Vec<&str> = text.lines().filter(|line| says_what_failed(line)).collect();
            eprintln!(
                "--- msiexec log {}: what failed ---\n{}",
                log.display(),
                last_lines(&failed.join("\n"), 30).join("\n")
            );
        }
        Error::Failed {
            service: service.to_owned(),
            reason: reason.to_owned(),
        }
    }

    /// Fails `service` unless `ok`.
    fn require(&self, ok: bool, service: &str, reason: &str) -> Result<(), Error> {
        if ok {
            Ok(())
        } else {
            Err(self.fail(service, reason))
        }
    }

    /// Asks `ready` up to `attempts` times, a second apart, until it says
    /// yes.
    fn wait_until(&mut self, attempts: u32, mut ready: impl FnMut(&mut Self) -> bool) -> bool {
        for attempt in 1..=attempts {
            if ready(self) {
                return true;
            }
            if attempt < attempts {
                self.clock.sleep(Duration::from_secs(1));
            }
        }
        false
    }

    /// msiexec's exit code for `args`.
    fn msiexec(&mut self, args: &[&str]) -> Result<Option<i32>, Error> {
        Ok(self.runner.code(&Cmd::new("msiexec", args))?)
    }

    /// What PowerShell prints for `script`.
    fn powershell(&mut self, script: &str) -> Result<String, Error> {
        let cmd = Cmd::new(
            "powershell",
            &["-NoProfile", "-NonInteractive", "-Command", script],
        );
        Ok(self.runner.read(&cmd)?)
    }

    /// The installed `rusty-photon-*` services.
    fn installed(&mut self) -> Result<Vec<String>, Error> {
        Ok(self
            .powershell(SERVICES)?
            .lines()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .collect())
    }

    /// `name`'s state and process ID; `None` when it is not installed.
    fn status(&mut self, name: &str) -> Option<Status> {
        let output = self
            .runner
            .read(&Cmd::new("sc.exe", &["queryex", name]))
            .ok()?;
        Some(Status {
            state: sc_number(&output, "STATE")?,
            pid: sc_number(&output, "PID").unwrap_or(0),
        })
    }

    /// Whether `service` is not installed, as sc.exe says of a service that
    /// does not exist: exit 1060, `ERROR_SERVICE_DOES_NOT_EXIST`. Any other
    /// failure to query it proves nothing, and fails the check.
    fn absent(&mut self, service: &str) -> Result<bool, Error> {
        let name = service_name(service);
        let (code, said) = self
            .runner
            .output(&Cmd::new("sc.exe", &["queryex", &name]))?;
        match code {
            Some(0) => Ok(false),
            Some(1060) => Ok(true),
            _ => Err(self.fail(
                service,
                &format!("sc.exe queryex {name} exited {code:?}{}", printed(&said)),
            )),
        }
    }

    fn state_is(&mut self, service: &str, state: u32) -> bool {
        self.status(&service_name(service))
            .is_some_and(|status| status.state == state)
    }

    fn sc(&mut self, args: &[&str]) -> Result<String, Error> {
        Ok(self.runner.read(&Cmd::new("sc.exe", args))?)
    }

    /// Whether the scheduled task that renews the TLS certificates exists.
    fn renewal_task(&mut self) -> bool {
        let query = Cmd::new("schtasks", &["/Query", "/TN", "rusty-photon-renew"]);
        self.runner.code(&query).is_ok_and(|code| code == Some(0))
    }

    /// Installs the MSI `from` first, then leaves its configs and nothing
    /// else running or logged, for the install under test to upgrade.
    fn seed(&mut self, from: &Path) -> Result<(), Error> {
        let log = self.places.temp.join("rusty-photon-msi-prior-install.log");
        // A failure here shows the log of the install that failed.
        let main_log = std::mem::replace(&mut self.install_log, log.clone());
        let name = from.file_name().unwrap_or_default().to_string_lossy();
        println!("== upgrade seed: installing the prior MSI ({name})");
        let code = self.msiexec(&[
            "/i",
            &from.to_string_lossy(),
            "/qn",
            "/norestart",
            "/l*v",
            &log.to_string_lossy(),
            "ADDLOCAL=ALL",
        ])?;
        self.require(
            code == Some(0),
            "msiexec",
            &format!("prior-MSI install exited {code:?} (log: {})", log.display()),
        )?;
        let seeded = self.status(&service_name("sentinel")).is_some();
        self.require(
            seeded,
            "msiexec",
            "prior MSI installed no services: the upgrade proof would prove nothing",
        )?;
        // The earlier install's configs, in its schema, are what the
        // upgrade must keep and doctor must migrate.
        let sentinel = self.config("sentinel");
        let written = self.wait_until(30, |_| sentinel.is_file());
        self.require(
            written,
            "sentinel",
            "timed out after 30s waiting for the prior install's self-created sentinel.json",
        )?;
        // The checks read the services' logs, which the earlier install's
        // output would satisfy. Disabled first, so a restart the failure
        // actions already scheduled cannot start one again; the upgrade
        // installs the services afresh, which undoes it.
        let services = self.installed()?;
        for name in &services {
            let config = Cmd::new("sc.exe", &["config", name, "start=", "disabled"]);
            self.runner.code(&config)?;
        }
        for name in &services {
            self.runner.code(&Cmd::new("sc.exe", &["stop", name]))?;
        }
        let stopped = self.wait_until(60, |run| {
            services.iter().all(|name| {
                run.status(name)
                    .is_none_or(|status| status.state == STOPPED)
            })
        });
        self.require(
            stopped,
            "msiexec",
            "timed out after 60s waiting for all seeded services stopped",
        )?;
        // Gone already is as good; a log left behind is not.
        let logs = self.logs();
        fs::remove_dir_all(&logs)
            .or_else(|err| match err.kind() {
                std::io::ErrorKind::NotFound => Ok(()),
                _ => Err(err),
            })
            .map_err(io(&logs))?;
        println!(
            "== upgrade seed: prior services stopped and logs cleared (the checks now see the \
             upgraded install only)"
        );
        self.install_log = main_log;
        Ok(())
    }

    /// One Programs and Features entry remains after the upgrade, the new
    /// MSI's: `RemoveExistingProducts` took the old one rather than
    /// installing beside it.
    fn upgraded_in_place(&mut self, msi: &Path) -> Result<(), Error> {
        let entries: Vec<String> = self
            .powershell(ARP)?
            .lines()
            .filter_map(|line| line.trim_end().strip_prefix("entry:"))
            .map(str::to_owned)
            .collect();
        self.require(
            entries.len() == 1,
            "msiexec",
            &format!(
                "expected exactly one Rusty Photon entry in Programs and Features after the \
                 upgrade, found {} (side by side?)",
                entries.len()
            ),
        )?;
        let name = msi.file_name().unwrap_or_default().to_string_lossy();
        let Some(version) = msi_version(msi) else {
            return Err(self.fail(
                "msiexec",
                &format!(
                    "cannot tell the surviving entry's MSI: '{name}' is not named \
                     rusty-photon-<version>-x64.msi"
                ),
            ));
        };
        let expected = format!("rusty-photon {version}");
        let comments = entries.first().cloned().unwrap_or_default();
        self.require(
            comments == expected,
            "msiexec",
            &format!(
                "surviving entry's comments '{comments}' != '{expected}' (the old product \
                 survived the upgrade?)"
            ),
        )?;
        println!("== upgrade: OK (one entry in Programs and Features after installing over the prior MSI)");
        Ok(())
    }

    /// Runs the shipped doctor's `--fix` over the upgraded install, as the
    /// operator does after an upgrade (docs/packaging-windows.md), and
    /// restarts `active`, so the checks see what it made.
    fn converge(&mut self, active: &[String]) -> Result<(), Error> {
        let doctor = self.places.install.join("rusty-photon-doctor.exe");
        let installed = doctor.is_file();
        self.require(installed, "doctor", "rusty-photon-doctor.exe not installed")?;
        println!(
            "== upgrade: running doctor --fix (the shipped binary, the operator's migration path)"
        );
        let fix = Cmd::new(&doctor.to_string_lossy(), &["--fix", "--json"]);
        let (code, json) = self.runner.output(&fix)?;
        // 1 is a diagnosis with problems; anything else is a crash, which on
        // Windows can be a negative NTSTATUS.
        if !matches!(code, Some(0 | 1)) {
            return Err(self.fail("doctor", &format!("doctor --fix exited {code:?}")));
        }
        let Ok(report) = serde_json::from_str::<Value>(&json) else {
            return Err(self.fail(
                "doctor",
                &format!("doctor --fix --json printed what is not JSON:\n{json}"),
            ));
        };
        let applied = report
            .get("fixes_applied")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        println!("== upgrade: doctor --fix applied {applied} fix(es)");
        // A key the upgraded services retired, still there after --fix, is
        // a breaking config change shipped without its remedy.
        let retired: Vec<String> = report
            .get("checks")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|check| {
                check.get("name").and_then(Value::as_str) == Some("config.retired-keys")
                    && check.get("status").and_then(Value::as_str) == Some("fail")
            })
            .map(|check| {
                let field = |name: &str| check.get(name).and_then(Value::as_str).unwrap_or("");
                format!("{}: {}", field("service"), field("detail"))
            })
            .collect();
        self.require(
            retired.is_empty(),
            "doctor",
            &format!(
                "config.retired-keys still failing after --fix (a breaking config change \
                 without a doctor remedy?):\n{}",
                retired.join("\n")
            ),
        )?;
        let pki = self.places.data.join("pki");
        for file in ["ca.pem", "credential"] {
            let made = pki.join(file).is_file();
            self.require(
                made,
                "doctor",
                &format!("--fix provisioning did not create pki\\{file}"),
            )?;
        }
        let path = pki.join("credential");
        let credential = fs::read_to_string(&path).map_err(io(&path))?;
        self.credential = Some(curl_credential(credential.trim(), &self.places.temp)?);
        // A fix takes effect when its service restarts. A failed restart
        // shows in the running check that follows, with the service's log,
        // after what net said.
        for service in active {
            let name = service_name(service);
            for verb in ["stop", "start"] {
                let (code, said) = self.runner.output(&Cmd::new("net", &[verb, &name]))?;
                if code != Some(0) {
                    eprintln!(
                        "verify-msi: net {verb} {name} exited {code:?}{}",
                        printed(&said)
                    );
                }
            }
        }
        println!("== upgrade: OK (doctor --fix converged the install; active services restarted)");
        Ok(())
    }

    /// Installed with its start type and failure actions.
    fn installed_as(&mut self, service: &str) -> Result<(), Error> {
        let name = service_name(service);
        let Ok(config) = self.sc(&["qc", &name]) else {
            return Err(self.fail(service, &format!("service {name} not installed")));
        };
        let expected = if GATED.contains(&service) {
            "Manual"
        } else {
            "Automatic"
        };
        let start = start_type(&config);
        self.require(
            start == expected,
            service,
            &format!("StartType is {start}, expected {expected}"),
        )?;
        // The flag makes a clean error exit, a serial driver's on its absent
        // device, a failure Windows restarts after (ADR-015).
        let actions = self.sc(&["qfailure", &name])?;
        self.require(
            actions.contains("RESTART -- Delay = 5000"),
            service,
            &format!("failure actions do not restart after 5000 ms:\n{actions}"),
        )?;
        let flag = self.sc(&["qfailureflag", &name])?;
        self.require(
            flag.contains("TRUE"),
            service,
            &format!("SERVICE_CONFIG_FAILURE_ACTIONS_FLAG not set:\n{flag}"),
        )
    }

    fn gated(&mut self, service: &str) -> Result<(), Error> {
        let stopped = self.state_is(service, STOPPED);
        self.require(stopped, service, "gated service is not stopped")?;
        let config = self.config(service).exists();
        self.require(
            !config,
            service,
            "config exists on a fresh install of a gated service",
        )?;
        println!("== {service}: OK (gated: Manual and stopped, no config)");
        Ok(())
    }

    /// Waits for the config `service` writes itself.
    fn created(&mut self, service: &str) -> Result<(), Error> {
        let config = self.config(service);
        let written = self.wait_until(30, |_| config.is_file());
        self.require(
            written,
            service,
            &format!(
                "timed out after 30s waiting for config self-created at {}",
                config.display()
            ),
        )
    }

    /// ui-htmx's config names rp, its one required target, and none of the
    /// drivers it no longer keeps.
    fn ui_config(&self) -> Result<(), Error> {
        let path = self.config("ui-htmx");
        let config = fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok());
        let Some(config) = config else {
            return Err(self.fail("ui-htmx", "self-created config is not JSON"));
        };
        self.require(
            config.get("drivers").is_none(),
            "ui-htmx",
            "self-created config carries the retired drivers key",
        )?;
        self.require(
            config.get("rp").is_some(),
            "ui-htmx",
            "self-created config has no rp target",
        )
    }

    /// Whether a probe of `service` takes HTTPS, and whether it sends the
    /// credential: what its config says, the same source the service reads.
    /// Only an upgrade's `--fix` turns them on; a service with no config
    /// serves plain HTTP.
    fn posture(&self, service: &str) -> Result<(bool, bool), Error> {
        let path = self.config(service);
        if !path.is_file() {
            return Ok((false, false));
        }
        let config = fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok());
        let Some(config) = config else {
            return Err(self.fail(service, "config is not JSON"));
        };
        let set = |pointer: &str| {
            config
                .pointer(pointer)
                .is_some_and(|value| !value.is_null())
        };
        Ok((set("/server/tls"), set("/server/auth")))
    }

    /// Running, its config written where it writes one, and answering its
    /// port.
    fn serves(&mut self, service: &str) -> Result<(), Error> {
        let running = self.wait_until(60, |run| run.state_is(service, RUNNING));
        self.require(
            running,
            service,
            "timed out after 60s waiting for: service RUNNING",
        )?;
        if !NO_CONFIG_FILE.contains(&service) {
            self.created(service)?;
        }
        if service == "ui-htmx" {
            self.ui_config()?;
        }
        let (tls, auth) = self.posture(service)?;
        let scheme = if tls { "https" } else { "http" };
        let port = packaging::port(service).unwrap_or_default();
        let path = if HEALTH_PROBED.contains(&service) {
            "/health"
        } else {
            "/management/apiversions"
        };
        let url = format!("{scheme}://127.0.0.1:{port}{path}");
        let credential = self
            .credential
            .as_ref()
            .filter(|_| auth)
            .map(|file| file.path().to_string_lossy().into_owned());
        let mut args = vec!["--max-time", "5", "-o", "NUL"];
        // A certificate from the CA --fix just made: the handshake
        // completing at all shows the issued pair loads and serves.
        if tls {
            args.push("-k");
        }
        if let Some(credential) = &credential {
            args.extend(["-K", credential]);
        }
        let answered = if service == "phd2-guider" {
            // No PHD2 here, so its `/health` answers 503, which still shows
            // the listener serves; with one, a success.
            args.extend(["-s", "-w", "%{http_code}", &url]);
            let probe = Cmd::new("curl", &args);
            self.wait_until(60, |run| {
                run.runner
                    .read(&probe)
                    .ok()
                    .and_then(|code| code.trim().parse::<u16>().ok())
                    .is_some_and(|code| (200..300).contains(&code) || code == 503)
            })
        } else {
            args.extend(["-fsS", &url]);
            let probe = Cmd::new("curl", &args);
            self.wait_until(60, |run| {
                run.runner.code(&probe).is_ok_and(|code| code == Some(0))
            })
        };
        self.require(
            answered,
            service,
            &format!("timed out after 60s waiting for an HTTP response on {url}"),
        )?;
        println!("== {service}: OK (running, {scheme} port {port}{path})");
        Ok(())
    }

    /// How many handshake attempts `service`'s log holds.
    fn handshakes(&self, service: &str) -> usize {
        self.log_tail(service, 500)
            .matches("eager startup handshake")
            .count()
    }

    fn serial(&mut self, service: &str) -> Result<(), Error> {
        self.created(service)?;
        let tried = self.wait_until(30, |run| run.handshakes(service) >= 1);
        self.require(
            tried,
            service,
            "timed out after 30s waiting for: 'eager startup handshake' in the service log",
        )?;
        println!("== {service}: OK (config self-created; retrying on absent serial device)");
        Ok(())
    }

    /// The driver's exit on its absent device is a clean error exit: a
    /// second attempt shows Windows counted it a failure and restarted it.
    fn restarted_after_error(&mut self, service: &str) -> Result<(), Error> {
        let again = self.wait_until(90, |run| run.handshakes(service) >= 2);
        self.require(
            again,
            service,
            "timed out after 90s waiting for: a second handshake attempt (the restart after a \
             clean error exit)",
        )?;
        println!(
            "== {service}: OK (restarted after a clean error exit: the failure-actions flag works)"
        );
        Ok(())
    }

    /// qhy-camera started, or reported the DLL missing: one of the two,
    /// read from the one snapshot of its log that showed it.
    fn qhy_camera(&mut self) -> Result<(), Error> {
        let mut log = String::new();
        let resolved = self.wait_until(30, |run| {
            log = run.log_tail("qhy-camera", 500);
            log.contains("Service started successfully") || log.contains("qhyccd.dll not found")
        });
        self.require(
            resolved,
            "qhy-camera",
            "timed out after 30s waiting for the delay-load to resolve either way (started, or \
             reported the missing DLL; not a loader crash)",
        )?;
        if log.contains("qhyccd.dll not found") {
            println!("== qhy-camera: OK (preflight reported the missing DLL; no loader crash)");
        } else {
            println!("== qhy-camera: OK (delay-load resolved; service started)");
        }
        Ok(())
    }

    fn has_log(&self, service: &str) -> bool {
        let prefix = format!("{service}.");
        fs::read_dir(self.logs())
            .into_iter()
            .flatten()
            .any(|entry| {
                entry.is_ok_and(|entry| entry.file_name().to_string_lossy().starts_with(&prefix))
            })
    }

    /// Windows restarts sentinel after a kill.
    fn killed(&mut self) -> Result<(), Error> {
        let name = service_name("sentinel");
        let pid = self.status(&name).map_or(0, |status| status.pid);
        self.require(pid != 0, "sentinel", "no PID for the running service")?;
        println!("== sentinel: killing PID {pid} to watch Windows restart it");
        self.runner
            .run(&Cmd::new("taskkill", &["/F", "/PID", &pid.to_string()]))?;
        let restarted = self.wait_until(60, |run| {
            run.status(&name).is_some_and(|status| {
                status.state == RUNNING && status.pid != pid && status.pid != 0
            })
        });
        self.require(
            restarted,
            "sentinel",
            "timed out after 60s waiting for: a restart after the kill (new PID, RUNNING)",
        )?;
        println!("== sentinel: OK (restarted after a hard kill)");
        Ok(())
    }

    /// Doctor, which sentinel's feature carries, runs, and its renewal task
    /// is registered.
    fn doctor(&mut self) -> Result<(), Error> {
        let doctor = self.places.install.join("rusty-photon-doctor.exe");
        let installed = doctor.is_file();
        self.require(installed, "doctor", "rusty-photon-doctor.exe not installed")?;
        let doctor = doctor.to_string_lossy();
        // A diagnosis may find problems on this box (1); anything else is a
        // crash, which on Windows can be a negative NTSTATUS. Its report is
        // stdout; a crash's reason, stderr, reaches the log.
        let (code, _) = self.runner.output(&Cmd::new(&doctor, &["--json"]))?;
        self.require(
            matches!(code, Some(0 | 1)),
            "doctor",
            &format!("rusty-photon-doctor --json exited {code:?}"),
        )?;
        // With nothing due, the renewal task's steady state.
        let (code, said) = self.runner.output(&Cmd::new(&doctor, &["tls", "renew"]))?;
        self.require(
            code == Some(0),
            "doctor",
            &format!("tls renew (nothing due) exited {code:?}{}", printed(&said)),
        )?;
        let task = self.renewal_task();
        self.require(
            task,
            "doctor",
            "scheduled task rusty-photon-renew not registered",
        )?;
        println!("== doctor: OK (binary runs; renewal task registered)");
        Ok(())
    }

    /// Removing one zwo feature leaves the other whole (ADR-014).
    fn remove_zwo_camera(&mut self, msi: &str) -> Result<(), Error> {
        println!("== modify: REMOVE=ZwoCamera");
        let code = self.msiexec(&["/i", msi, "/qn", "/norestart", "REMOVE=ZwoCamera"])?;
        self.require(
            code == Some(0),
            "zwo-camera",
            &format!("feature remove exited {code:?}"),
        )?;
        let gone = self.absent("zwo-camera")?;
        self.require(
            gone,
            "zwo-camera",
            "service still installed after feature remove",
        )?;
        let install = self.places.install.clone();
        for (file, what) in [
            ("rusty-photon-zwo-camera.exe", "exe"),
            ("ASICamera2.dll", "ASICamera2.dll"),
        ] {
            self.require(
                !install.join(file).exists(),
                "zwo-camera",
                &format!("{what} still present after feature remove"),
            )?;
        }
        let running = self.state_is("zwo-focuser", RUNNING);
        self.require(
            running,
            "zwo-focuser",
            "not running after removing the zwo-camera feature",
        )?;
        self.require(
            install.join("EAF_focuser.dll").is_file(),
            "zwo-focuser",
            "EAF_focuser.dll disappeared with the zwo-camera feature",
        )?;
        self.require(
            install.join("ZWO-SDK-LICENSE.txt").is_file(),
            "zwo-focuser",
            "shared ZWO license disappeared while a zwo feature is installed",
        )?;
        println!(
            "== modify: OK (zwo-camera gone; zwo-focuser, its DLL and the shared license intact)"
        );
        Ok(())
    }

    /// The uninstall takes the services, the exes and the renewal task, and
    /// leaves the configs, the PKI and the logs, which only the manual purge
    /// takes.
    fn uninstall(&mut self, msi: &str, services: &[String]) -> Result<(), Error> {
        println!("== uninstall: msiexec /qn /x");
        let log = self.uninstall_log.to_string_lossy().into_owned();
        let code = self.msiexec(&["/x", msi, "/qn", "/norestart", "/l*v", &log])?;
        self.require(
            code == Some(0),
            "msiexec",
            &format!("uninstall exited {code:?} (log: {log})"),
        )?;
        for service in services {
            let gone = self.absent(service)?;
            self.require(gone, service, "service still installed after uninstall")?;
        }
        let exes = fs::read_dir(&self.places.install)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .any(|entry| {
                Path::new(&entry.file_name())
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"))
            });
        let install = self.places.install.display().to_string();
        self.require(
            !exes,
            "msiexec",
            &format!("exes left under {install} after uninstall"),
        )?;
        let task = self.renewal_task();
        self.require(
            !task,
            "doctor",
            "scheduled task rusty-photon-renew survived uninstall",
        )?;
        let config = self.config("sentinel").is_file();
        self.require(
            config,
            "sentinel",
            "self-created config did not survive uninstall (only the manual purge takes it)",
        )?;
        // Losing the CA or the credential would strand every trust anchor
        // and client credential handed out.
        if self.upgraded {
            for file in ["ca.pem", "credential"] {
                let kept = self.places.data.join("pki").join(file).is_file();
                self.require(
                    kept,
                    "doctor",
                    &format!(
                        "provisioned pki\\{file} did not survive uninstall (only the manual \
                         purge takes it)"
                    ),
                )?;
            }
        }
        let logs = fs::read_dir(self.logs()).is_ok_and(|mut entries| entries.next().is_some());
        self.require(logs, "msiexec", "log files did not survive uninstall")?;
        println!("== uninstall: OK (services and exes gone; configs and logs survive)");
        Ok(())
    }

    /// Installs the MSI, over the earlier one [`Run::seed`] installed when
    /// there is one, and runs every check; with `keep`, it stops before the
    /// uninstall.
    fn lifecycle(&mut self, msi: &Path, services: &[String], keep: bool) -> Result<(), Error> {
        let msi_arg = msi.to_string_lossy().into_owned();
        println!("== install: msiexec /qn ADDLOCAL=ALL");
        let log = self.install_log.to_string_lossy().into_owned();
        let code = self.msiexec(&[
            "/i",
            &msi_arg,
            "/qn",
            "/norestart",
            "/l*v",
            &log,
            "ADDLOCAL=ALL",
        ])?;
        self.require(
            code == Some(0),
            "msiexec",
            &format!("silent install exited {code:?} (log: {log})"),
        )?;
        let (gated, serial, active) = kinds(services);
        if self.upgraded {
            self.upgraded_in_place(msi)?;
            self.converge(&active)?;
        }
        for service in services {
            self.installed_as(service)?;
        }
        println!(
            "== static: {} services installed, start types and failure actions OK",
            services.len()
        );
        for service in &gated {
            self.gated(service)?;
        }
        for service in &active {
            self.serves(service)?;
        }
        for service in &serial {
            self.serial(service)?;
        }
        if let Some(first) = serial.first() {
            self.restarted_after_error(first)?;
        }
        self.qhy_camera()?;
        let started = active.iter().chain(&serial).map(String::as_str);
        for service in started.chain(["qhy-camera"]) {
            let logged = self.has_log(service);
            let logs = self.logs().display().to_string();
            self.require(
                logged,
                service,
                &format!("no rolling log file under {logs}"),
            )?;
        }
        println!("== logs: OK (rolling files present for every started service)");
        self.killed()?;
        let reload = Cmd::new(
            "sc.exe",
            &["control", "rusty-photon-filemonitor", "paramchange"],
        );
        if let Err(error) = self.runner.run(&reload) {
            return Err(self.fail(
                "filemonitor",
                &format!("sc control paramchange failed: {error}"),
            ));
        }
        println!("== filemonitor: OK (accepted ParamChange)");
        self.doctor()?;
        self.remove_zwo_camera(&msi_arg)?;
        if keep {
            println!("verify-msi: --keep set; leaving the product installed");
            return Ok(());
        }
        self.uninstall(&msi_arg, services)
    }
}

/// The services by kind: the [`GATED`] ones, the [`SERIAL`] drivers, and
/// the rest but qhy-camera, which all run and serve.
fn kinds(services: &[String]) -> (Vec<String>, Vec<String>, Vec<String>) {
    let gated: Vec<String> = services
        .iter()
        .filter(|service| GATED.contains(&service.as_str()))
        .cloned()
        .collect();
    let serial: Vec<String> = SERIAL
        .iter()
        .filter(|service| services.iter().any(|name| name == *service))
        .map(|service| (*service).to_owned())
        .collect();
    let active = services
        .iter()
        .filter(|service| {
            !gated.contains(service) && !serial.contains(service) && *service != "qhy-camera"
        })
        .cloned()
        .collect();
    (gated, serial, active)
}

/// A curl config file that sends `credential` as the observatory user's.
fn curl_credential(credential: &str, temp: &Path) -> Result<tempfile::NamedTempFile, Error> {
    let mut file = tempfile::NamedTempFile::new_in(temp).map_err(io(temp))?;
    let quoted = credential.replace('\\', "\\\\").replace('"', "\\\"");
    writeln!(file, "user = \"observatory:{quoted}\"").map_err(io(file.path()))?;
    Ok(file)
}

/// Verifies the MSI `request` names and returns how many services it
/// installs.
///
/// # Errors
///
/// When the run is refused, as on a box that is not elevated or has an
/// install already, or a check fails.
pub fn verify(
    runner: &mut impl Runner,
    clock: &mut impl Clock,
    root: &Path,
    places: &Places,
    request: &Request,
) -> Result<usize, Error> {
    // fltmc, the file-system filter list, runs only elevated.
    let elevated = runner
        .code(&Cmd::new("fltmc", &[]))
        .is_ok_and(|code| code == Some(0));
    if !elevated {
        return Err(refused(
            "must run elevated (msiexec /qn and service control need it)",
        ));
    }
    let msi = if let Some(msi) = &request.msi {
        msi.clone()
    } else {
        let version = packaging::workspace_version(root)?;
        root.join("dist")
            .join(&version)
            .join(format!("rusty-photon-{version}-x64.msi"))
    };
    if !msi.is_file() {
        return Err(refused(format!(
            "{} not found: run `cargo ci build-msi` first",
            msi.display()
        )));
    }
    // msiexec takes no `\\?\` path, which canonicalizing would give.
    let msi = std::path::absolute(&msi).map_err(io(&msi))?;
    let upgrade_from = match &request.upgrade_from {
        Some(from) if !from.is_file() => {
            return Err(refused(format!(
                "--upgrade-from {} not found",
                from.display()
            )));
        }
        Some(from) => Some(std::path::absolute(from).map_err(io(from))?),
        None => None,
    };
    let services: Vec<String> = packaging::packaged(root)?
        .into_iter()
        .filter(|service| service != packaging::NO_WINDOWS_SDK)
        .collect();

    let mut run = Run {
        runner,
        clock,
        places,
        install_log: places.temp.join("rusty-photon-msi-install.log"),
        uninstall_log: places.temp.join("rusty-photon-msi-uninstall.log"),
        credential: None,
        upgraded: upgrade_from.is_some(),
    };
    // The checks assume a fresh install: what an earlier one left would fail
    // them, or pass them, for the wrong reason.
    if !run.installed()?.is_empty() {
        return Err(refused(
            "rusty-photon-* services already installed: msiexec /x the previous install first",
        ));
    }
    if places.data.exists() {
        return Err(refused(format!(
            "{} already exists: delete it (the manual purge) so the fresh-install checks mean \
             something",
            places.data.display()
        )));
    }
    if let Some(from) = &upgrade_from {
        run.seed(from)?;
    }

    run.lifecycle(&msi, &services, request.keep)?;
    if !request.keep {
        println!("\nverify-msi: OK ({} services)", services.len());
    }
    Ok(services.len())
}

// The tools' tests compare Unix paths; the MSI is verified on Windows.
#[cfg(test)]
#[cfg(unix)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;
    use crate::process::{Script, Stopped};

    /// The services the fixture's MSI installs: one of every kind.
    const SERVICES: &[&str] = &[
        "filemonitor",
        "phd2-guider",
        "ppba-driver",
        "qhy-camera",
        "sentinel",
        "sky-survey-camera",
        "ui-htmx",
        "zwo-camera",
        "zwo-focuser",
    ];

    const PRIOR: &str = "rusty-photon-0.1.0+nightly.202610210507.gdef5678-x64.msi";
    const NIGHTLY: &str = "0.1.0+nightly.202610220507.gabc1234";

    /// A Windows box as the MSI leaves it, which a test may break: what the
    /// commands the run gives answer, and what an install, a feature's
    /// removal and the uninstall do to its files.
    struct Sim {
        places: Places,
        installed: bool,
        /// Set by the seed's `sc.exe stop`, cleared by the next install.
        stopped: bool,
        zwo_camera_removed: bool,
        sentinel_pid: u32,
        handshakes: usize,
        qhy_log: &'static str,
        ui_config: &'static str,
        /// Whether the logs were gone when the install under test began.
        logs_cleared_at_install: Option<bool>,
        arp: Vec<String>,
        fix_report: String,
        broken: Option<Broken>,
    }

    /// One way the box goes wrong.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Broken {
        GatedRunning,
        /// The install writes sky-survey-camera's config.
        GatedConfig,
        /// filemonitor writes no config.
        NoConfig,
        /// zwo-focuser writes no log.
        NoLog,
        /// Removing the `ZwoCamera` feature leaves this file installed.
        FeatureRemoveKeeps(&'static str),
        /// Removing the `ZwoCamera` feature takes this file too.
        FeatureRemoveTakes(&'static str),
        /// Removing the `ZwoCamera` feature stops zwo-focuser.
        FeatureRemoveStopsFocuser,
        UninstallKeepsExe,
        /// The uninstall takes this, under the data directory.
        UninstallTakes(&'static str),
    }

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    impl Sim {
        fn new(places: Places) -> Self {
            Self {
                places,
                installed: false,
                stopped: false,
                zwo_camera_removed: false,
                sentinel_pid: 500,
                handshakes: 2,
                qhy_log: "qhyccd.dll not found\n",
                ui_config: r#"{"rp": {"url": "http://127.0.0.1:11115"}}"#,
                logs_cleared_at_install: None,
                arp: Vec::new(),
                fix_report: r#"{"checks": [{"name": "config.retired-keys", "status": "ok"}],
                    "fixes_applied": [{"op": "remove"}, {"op": "add"}]}"#
                    .to_owned(),
                broken: None,
            }
        }

        fn running(&self) -> Vec<&'static str> {
            SERVICES
                .iter()
                .copied()
                .filter(|service| !(*service == "zwo-camera" && self.zwo_camera_removed))
                .collect()
        }

        /// What an install of every feature makes, as the services write
        /// their configs and logs on first start.
        fn install(&mut self, msi: &str) {
            if !msi.ends_with(PRIOR) {
                self.logs_cleared_at_install = Some(!self.places.data.join("logs").exists());
            }
            for service in SERVICES {
                write(
                    &self
                        .places
                        .install
                        .join(format!("rusty-photon-{service}.exe")),
                    "",
                );
            }
            for file in [
                "rusty-photon-doctor.exe",
                "ASICamera2.dll",
                "EAF_focuser.dll",
                "ZWO-SDK-LICENSE.txt",
            ] {
                write(&self.places.install.join(file), "");
            }
            let (gated, serial, active) =
                kinds(&SERVICES.iter().map(|&s| s.to_owned()).collect::<Vec<_>>());
            for service in active.iter().chain(&serial) {
                let config = self.places.data.join(format!("{service}.json"));
                let text = if service == "ui-htmx" {
                    self.ui_config
                } else {
                    "{}"
                };
                let silent = NO_CONFIG_FILE.contains(&service.as_str())
                    || (service == "filemonitor" && self.broken == Some(Broken::NoConfig));
                if !silent && !config.exists() {
                    write(&config, text);
                }
                if service == "zwo-focuser" && self.broken == Some(Broken::NoLog) {
                    continue;
                }
                let log = if serial.contains(service) {
                    "eager startup handshake\n".repeat(self.handshakes)
                } else {
                    "Service started successfully\n".to_owned()
                };
                write(
                    &self.places.data.join(format!("logs/{service}.2026-10-22")),
                    &log,
                );
            }
            write(
                &self.places.data.join("logs/qhy-camera.2026-10-22"),
                self.qhy_log,
            );
            if self.broken == Some(Broken::GatedConfig) {
                for service in &gated {
                    write(&self.places.data.join(format!("{service}.json")), "{}");
                }
            }
            self.installed = true;
            self.stopped = false;
        }

        fn sc(&mut self, args: &[String]) -> Result<String, i32> {
            let name = args.get(1).cloned().unwrap_or_default();
            let service = name
                .strip_prefix("rusty-photon-")
                .unwrap_or_default()
                .to_owned();
            let installed = self.installed && self.running().contains(&service.as_str());
            match args.first().map(String::as_str) {
                Some("queryex") if installed => {
                    let gated = GATED.contains(&service.as_str()) && self.broken != Some(Broken::GatedRunning);
                    let removal = self.broken == Some(Broken::FeatureRemoveStopsFocuser)
                        && service == "zwo-focuser"
                        && self.zwo_camera_removed;
                    let (state, word) = if self.stopped || gated || removal {
                        (STOPPED, "STOPPED")
                    } else {
                        (RUNNING, "RUNNING")
                    };
                    let pid = if service == "sentinel" { self.sentinel_pid } else { 1000 };
                    Ok(format!(
                        "SERVICE_NAME: {name}\n        STATE              : {state}  {word}\n        PID                : {pid}\n"
                    ))
                }
                Some("qc") if installed => {
                    let start = if GATED.contains(&service.as_str()) { "3   DEMAND_START" } else { "2   AUTO_START" };
                    Ok(format!("SERVICE_NAME: {name}\n        START_TYPE         : {start}\n"))
                }
                Some("qfailure") if installed => Ok(
                    "        FAILURE_ACTIONS              : RESTART -- Delay = 5000 milliseconds.\n".to_owned(),
                ),
                Some("qfailureflag") if installed => {
                    Ok("        FAILURE_ACTIONS_ON_NONCRASH_FAILURES: TRUE\n".to_owned())
                }
                Some("stop") => {
                    self.stopped = true;
                    Ok(String::new())
                }
                Some("config" | "control") => Ok(String::new()),
                _ => Err(1060),
            }
        }

        fn msiexec(&mut self, args: &[String]) -> Result<String, i32> {
            let msi = args.get(1).cloned().unwrap_or_default();
            match (
                args.first().map(String::as_str),
                args.last().map(String::as_str),
            ) {
                (Some("/i"), Some("ADDLOCAL=ALL")) => self.install(&msi),
                (Some("/i"), Some("REMOVE=ZwoCamera")) => {
                    self.zwo_camera_removed = true;
                    for file in ["rusty-photon-zwo-camera.exe", "ASICamera2.dll"] {
                        if self.broken != Some(Broken::FeatureRemoveKeeps(file)) {
                            fs::remove_file(self.places.install.join(file)).unwrap();
                        }
                    }
                    if let Some(Broken::FeatureRemoveTakes(file)) = self.broken {
                        fs::remove_file(self.places.install.join(file)).unwrap();
                    }
                }
                (Some("/x"), _) => {
                    self.installed = false;
                    for entry in fs::read_dir(&self.places.install).unwrap() {
                        let path = entry.unwrap().path();
                        let exe = path.extension().is_some_and(|e| e == "exe");
                        if !(exe && self.broken == Some(Broken::UninstallKeepsExe)) {
                            fs::remove_file(path).unwrap();
                        }
                    }
                    if let Some(Broken::UninstallTakes(path)) = self.broken {
                        let path = self.places.data.join(path);
                        if path.is_dir() {
                            fs::remove_dir_all(path).unwrap();
                        } else {
                            fs::remove_file(path).unwrap();
                        }
                    }
                }
                _ => return Err(1639),
            }
            Ok(String::new())
        }

        /// doctor's `--fix`: the PKI, and TLS and auth for sentinel.
        fn fix(&self) -> String {
            write(&self.places.data.join("pki/ca.pem"), "ca");
            write(&self.places.data.join("pki/credential"), "s3cret\n");
            write(
                &self.places.data.join("sentinel.json"),
                r#"{"server": {"tls": {"cert": "c"}, "auth": {"user": "observatory"}}}"#,
            );
            self.fix_report.clone()
        }

        fn answer(&mut self, cmd: &Cmd) -> Result<String, i32> {
            let doctor = self.places.install.join("rusty-photon-doctor.exe");
            let lines = |lines: Vec<String>| lines.concat();
            match cmd.program.as_str() {
                "taskkill" => {
                    self.sentinel_pid = self.sentinel_pid.saturating_add(1);
                    Ok(String::new())
                }
                "powershell" if cmd.args.last().is_some_and(|script| script == SERVICES_PS) => {
                    let running = self.running();
                    let installed = running.iter().filter(|_| self.installed);
                    Ok(lines(
                        installed
                            .map(|service| format!("rusty-photon-{service}\n"))
                            .collect(),
                    ))
                }
                "powershell" => Ok(lines(
                    self.arp
                        .iter()
                        .map(|comments| format!("entry:{comments}\n"))
                        .collect(),
                )),
                "sc.exe" => self.sc(&cmd.args),
                "msiexec" => self.msiexec(&cmd.args),
                // The renewal task goes with the product.
                "schtasks" if self.installed => Ok(String::new()),
                "schtasks" => Err(1),
                "curl" if cmd.args.contains(&"-w".to_owned()) => Ok("503".to_owned()),
                "fltmc" | "net" | "curl" => Ok(String::new()),
                program if program == doctor.to_string_lossy() => {
                    if cmd.args == ["--fix", "--json"] {
                        Ok(self.fix())
                    } else {
                        Ok(String::new())
                    }
                }
                // What a shell answers for a program it cannot find.
                _ => Err(127),
            }
        }
    }

    const SERVICES_PS: &str = super::SERVICES;

    /// A repository with the fixture's services, svbony-camera too, and a
    /// built MSI, beside a Windows box's directories.
    struct Fixture {
        dir: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let fixture = Self { dir };
            let root = fixture.root();
            write(
                &root.join("Cargo.toml"),
                "[workspace]\nmembers = []\n\n[workspace.package]\nversion = \"0.1.0\"\n",
            );
            for service in SERVICES.iter().chain(&["svbony-camera"]) {
                fs::create_dir_all(root.join("services").join(service).join("pkg")).unwrap();
            }
            write(&fixture.msi("0.1.0"), "msi");
            write(&fixture.msi(NIGHTLY), "msi");
            write(&fixture.dir.path().join("prior").join(PRIOR), "msi");
            fs::create_dir_all(fixture.places().temp).unwrap();
            fixture
        }

        fn root(&self) -> PathBuf {
            self.dir.path().join("repo")
        }

        fn msi(&self, version: &str) -> PathBuf {
            self.root()
                .join("dist")
                .join(version)
                .join(format!("rusty-photon-{version}-x64.msi"))
        }

        fn places(&self) -> Places {
            let windows = self.dir.path().join("C");
            Places {
                data: windows.join("ProgramData/rusty-photon"),
                install: windows.join("Program Files/rusty-photon"),
                temp: windows.join("Temp"),
            }
        }

        fn sim(&self) -> Rc<RefCell<Sim>> {
            Rc::new(RefCell::new(Sim::new(self.places())))
        }

        /// Runs the verifier on the box `sim` simulates, `script`'s rules
        /// first, and returns what it gave and the commands it ran.
        fn verify(
            &self,
            sim: &Rc<RefCell<Sim>>,
            script: Script,
            request: &Request,
        ) -> (Result<usize, Error>, Vec<String>) {
            let sim = Rc::clone(sim);
            let mut runner = script.on_cmd("", move |cmd| sim.borrow_mut().answer(cmd));
            let result = verify(
                &mut runner,
                &mut Stopped::default(),
                &self.root(),
                &self.places(),
                request,
            );
            (result, self.shorten(&runner.lines))
        }

        /// The commands, with the box's directories and the repository
        /// shortened, and the polls of a service's state left out.
        fn shorten(&self, lines: &[String]) -> Vec<String> {
            let places = self.places();
            lines
                .iter()
                .filter(|line| !line.starts_with("sc.exe q"))
                .map(|line| {
                    line.replace(&places.install.display().to_string(), "<install>")
                        .replace(&places.temp.display().to_string(), "<temp>")
                        .replace(&self.root().join("dist").display().to_string(), "<dist>")
                        .replace(
                            &self.dir.path().join("prior").display().to_string(),
                            "<prior>",
                        )
                })
                .collect()
        }
    }

    fn curl(port: u16, path: &str) -> String {
        format!("curl --max-time 5 -o NUL -fsS http://127.0.0.1:{port}{path}")
    }

    fn powershell(script: &str) -> String {
        format!("powershell -NoProfile -NonInteractive -Command {script}")
    }

    #[test]
    fn verifies_a_fresh_install_through_the_uninstall() {
        let fixture = Fixture::new();
        let sim = fixture.sim();
        let (result, lines) = fixture.verify(&sim, Script::default(), &Request::default());
        assert_eq!(result.unwrap(), 9, "svbony-camera is not in the MSI");
        let msi = "<dist>/0.1.0/rusty-photon-0.1.0-x64.msi";
        assert_eq!(
            lines,
            [
                "fltmc".to_owned(),
                powershell(SERVICES_PS),
                format!("msiexec /i {msi} /qn /norestart /l*v <temp>/rusty-photon-msi-install.log ADDLOCAL=ALL"),
                curl(11111, "/management/apiversions"),
                "curl --max-time 5 -o NUL -s -w %{http_code} http://127.0.0.1:11130/health".to_owned(),
                curl(11114, "/health"),
                curl(11120, "/health"),
                curl(11122, "/management/apiversions"),
                curl(11124, "/management/apiversions"),
                "taskkill /F /PID 500".to_owned(),
                "sc.exe control rusty-photon-filemonitor paramchange".to_owned(),
                "<install>/rusty-photon-doctor.exe --json".to_owned(),
                "<install>/rusty-photon-doctor.exe tls renew".to_owned(),
                "schtasks /Query /TN rusty-photon-renew".to_owned(),
                format!("msiexec /i {msi} /qn /norestart REMOVE=ZwoCamera"),
                format!("msiexec /x {msi} /qn /norestart /l*v <temp>/rusty-photon-msi-uninstall.log"),
                "schtasks /Query /TN rusty-photon-renew".to_owned(),
            ]
        );
        let places = fixture.places();
        assert!(places.data.join("sentinel.json").is_file());
        assert!(!sim.borrow().installed);
    }

    #[test]
    fn checks_every_service_is_installed_with_its_start_type_and_failure_actions() {
        let cases = [
            (
                "sc.exe qc rusty-photon-ppba-driver",
                "        START_TYPE         : 2   AUTO_START  (DELAYED)\n",
                "FAIL [ppba-driver]: StartType is AutomaticDelayedStart, expected Automatic",
            ),
            (
                "sc.exe qc rusty-photon-sky-survey-camera",
                "        START_TYPE         : 2   AUTO_START\n",
                "FAIL [sky-survey-camera]: StartType is Automatic, expected Manual",
            ),
            (
                "sc.exe qc rusty-photon-sentinel",
                "        START_TYPE         : 3   DEMAND_START\n",
                "FAIL [sentinel]: StartType is Manual, expected Automatic",
            ),
            (
                "sc.exe qfailure rusty-photon-filemonitor",
                "        FAILURE_ACTIONS              : RESTART -- Delay = 60000 milliseconds.\n",
                "FAIL [filemonitor]: failure actions do not restart after 5000 ms",
            ),
            (
                "sc.exe qfailureflag rusty-photon-qhy-camera",
                "        FAILURE_ACTIONS_ON_NONCRASH_FAILURES: FALSE\n",
                "FAIL [qhy-camera]: SERVICE_CONFIG_FAILURE_ACTIONS_FLAG not set",
            ),
        ];
        for (prefix, answer, expected) in cases {
            let fixture = Fixture::new();
            let script = Script::default().on(prefix, move || Ok(answer.to_owned()));
            let (result, _) = fixture.verify(&fixture.sim(), script, &Request::default());
            let error = result.unwrap_err().to_string();
            assert!(error.starts_with(expected), "{prefix}: {error}");
        }
        let fixture = Fixture::new();
        let script = Script::default().on("sc.exe qc rusty-photon-zwo-focuser", || Err(()));
        let (result, _) = fixture.verify(&fixture.sim(), script, &Request::default());
        assert_eq!(
            result.unwrap_err().to_string(),
            "FAIL [zwo-focuser]: service rusty-photon-zwo-focuser not installed"
        );
    }

    /// Each kind of service's check, each shown to fail with its behaviour
    /// broken, on a box broken in one way at a time.
    #[test]
    fn names_each_service_that_misbehaves() {
        type Breakage = Box<dyn Fn(&mut Sim)>;
        let cases: Vec<(Breakage, Script, &str)> = vec![
            (Box::new(|sim| sim.broken = Some(Broken::GatedRunning)), Script::default(), "FAIL [sky-survey-camera]: gated service is not stopped"),
            (Box::new(|sim| sim.ui_config = r#"{"rp": {}, "drivers": {}}"#), Script::default(), "FAIL [ui-htmx]: self-created config carries the retired drivers key"),
            (Box::new(|sim| sim.ui_config = "{"), Script::default(), "FAIL [ui-htmx]: self-created config is not JSON"),
            (Box::new(|sim| sim.ui_config = "{}"), Script::default(), "FAIL [ui-htmx]: self-created config has no rp target"),
            (Box::new(|_| ()), Script::default().on("curl --max-time 5 -o NUL -fsS http://127.0.0.1:11124", || Err(())), "FAIL [zwo-focuser]: timed out after 60s waiting for an HTTP response on http://127.0.0.1:11124/management/apiversions"),
            (Box::new(|_| ()), Script::default().on("curl --max-time 5 -o NUL -s -w", || Ok("500".to_owned())), "FAIL [phd2-guider]: timed out after 60s waiting for an HTTP response"),
            (Box::new(|sim| sim.handshakes = 0), Script::default(), "FAIL [ppba-driver]: timed out after 30s waiting for: 'eager startup handshake'"),
            (Box::new(|sim| sim.handshakes = 1), Script::default(), "FAIL [ppba-driver]: timed out after 90s waiting for: a second handshake attempt"),
            (Box::new(|sim| sim.qhy_log = "panic: STATUS_DLL_NOT_FOUND\n"), Script::default(), "FAIL [qhy-camera]: timed out after 30s waiting for the delay-load to resolve"),
            (Box::new(|_| ()), Script::default().on("taskkill", || Ok(String::new())), "FAIL [sentinel]: timed out after 60s waiting for: a restart after the kill"),
            (Box::new(|_| ()), Script::default().on("sc.exe control", || Err(())), "FAIL [filemonitor]: sc control paramchange failed"),
            (Box::new(|sim| sim.broken = Some(Broken::FeatureRemoveKeeps("ASICamera2.dll"))), Script::default(), "FAIL [zwo-camera]: ASICamera2.dll still present after feature remove"),
            (Box::new(|_| ()), Script::default().on("sc.exe queryex rusty-photon-zwo-focuser", || Ok("STATE : 1 STOPPED\n".to_owned())), "FAIL [zwo-focuser]: timed out after 60s waiting for: service RUNNING"),
            (Box::new(|sim| sim.broken = Some(Broken::UninstallKeepsExe)), Script::default(), "FAIL [msiexec]: exes left under"),
            (Box::new(|_| ()), Script::default().on("schtasks", || Ok(String::new())), "FAIL [doctor]: scheduled task rusty-photon-renew survived uninstall"),
            (Box::new(|sim| sim.broken = Some(Broken::UninstallTakes("sentinel.json"))), Script::default(), "FAIL [sentinel]: self-created config did not survive uninstall"),
            (Box::new(|sim| sim.broken = Some(Broken::GatedConfig)), Script::default(), "FAIL [sky-survey-camera]: config exists on a fresh install of a gated service"),
            (Box::new(|sim| sim.broken = Some(Broken::NoConfig)), Script::default(), "FAIL [filemonitor]: timed out after 30s waiting for config self-created at "),
            (Box::new(|sim| sim.broken = Some(Broken::NoLog)), Script::default(), "FAIL [zwo-focuser]: no rolling log file under "),
            (Box::new(|sim| sim.broken = Some(Broken::FeatureRemoveKeeps("rusty-photon-zwo-camera.exe"))), Script::default(), "FAIL [zwo-camera]: exe still present after feature remove"),
            (Box::new(|sim| sim.broken = Some(Broken::FeatureRemoveTakes("EAF_focuser.dll"))), Script::default(), "FAIL [zwo-focuser]: EAF_focuser.dll disappeared with the zwo-camera feature"),
            (Box::new(|sim| sim.broken = Some(Broken::FeatureRemoveTakes("ZWO-SDK-LICENSE.txt"))), Script::default(), "FAIL [zwo-focuser]: shared ZWO license disappeared while a zwo feature is installed"),
            (Box::new(|sim| sim.broken = Some(Broken::UninstallTakes("logs"))), Script::default(), "FAIL [msiexec]: log files did not survive uninstall"),
            (Box::new(|sim| sim.broken = Some(Broken::FeatureRemoveStopsFocuser)), Script::default(), "FAIL [zwo-focuser]: not running after removing the zwo-camera feature"),
            (Box::new(|_| ()), Script::default().on("sc.exe queryex rusty-photon-sentinel", || Ok("        STATE              : 4  RUNNING\n        PID                : 0\n".to_owned())), "FAIL [sentinel]: no PID for the running service"),
            (Box::new(|_| ()), Script::default().on("sc.exe queryex rusty-photon-zwo-camera", || Ok("        STATE              : 4  RUNNING\n".to_owned())), "FAIL [zwo-camera]: service still installed after feature remove"),
            (Box::new(|_| ()), Script::default().on("sc.exe queryex rusty-photon-filemonitor", || Ok("        STATE              : 4  RUNNING\n".to_owned())), "FAIL [filemonitor]: service still installed after uninstall"),
        ];
        for (break_it, script, expected) in cases {
            let fixture = Fixture::new();
            let sim = fixture.sim();
            break_it(&mut sim.borrow_mut());
            let (result, lines) = fixture.verify(&sim, script, &Request::default());
            let error = result.unwrap_err().to_string();
            assert!(
                error.starts_with(expected),
                "{expected}\n got: {error}\n{lines:#?}"
            );
        }
    }

    #[test]
    fn a_doctor_that_crashes_or_a_renewal_that_fails_fails_the_run() {
        let doctor = |args: &'static str, code: i32| {
            let fixture = Fixture::new();
            let install = fixture.places().install.display().to_string();
            let line = format!("{install}/rusty-photon-doctor.exe {args}");
            let script = Script::default().on_cmd(&line, move |_| Err(code));
            let (result, _) = fixture.verify(&fixture.sim(), script, &Request::default());
            result.map_err(|error| error.to_string())
        };
        assert_eq!(
            doctor("--json", 1).unwrap(),
            9,
            "a diagnosis with problems passes"
        );
        assert_eq!(
            doctor("--json", 2).unwrap_err(),
            "FAIL [doctor]: rusty-photon-doctor --json exited Some(2)"
        );
        // An access violation's NTSTATUS, as Windows gives a crash.
        assert_eq!(
            doctor("--json", -1_073_741_819).unwrap_err(),
            "FAIL [doctor]: rusty-photon-doctor --json exited Some(-1073741819)"
        );
        assert_eq!(
            doctor("tls renew", 1).unwrap_err(),
            "FAIL [doctor]: tls renew (nothing due) exited Some(1)"
        );
        let fixture = Fixture::new();
        let script = Script::default().on("schtasks", || Err(()));
        let (result, _) = fixture.verify(&fixture.sim(), script, &Request::default());
        assert_eq!(
            result.unwrap_err().to_string(),
            "FAIL [doctor]: scheduled task rusty-photon-renew not registered"
        );
        let fixture = Fixture::new();
        let script = Script::default().on_cmd("msiexec /i", |cmd| {
            if cmd.args.last().is_some_and(|arg| arg == "ADDLOCAL=ALL") {
                Err(1603)
            } else {
                Ok(String::new())
            }
        });
        let (result, _) = fixture.verify(&fixture.sim(), script, &Request::default());
        let error = result.unwrap_err().to_string();
        assert!(
            error.starts_with("FAIL [msiexec]: silent install exited Some(1603) (log: "),
            "{error}"
        );
    }

    #[test]
    fn a_failed_feature_remove_or_uninstall_fails_the_run() {
        for (last, expected) in [
            (
                "REMOVE=ZwoCamera",
                "FAIL [zwo-camera]: feature remove exited Some(1603)",
            ),
            ("/l*v", "FAIL [msiexec]: uninstall exited Some(1603) (log: "),
        ] {
            let fixture = Fixture::new();
            let sim = fixture.sim();
            let answering = Rc::clone(&sim);
            let script = Script::default().on_cmd("msiexec", move |cmd| {
                let removes = cmd.args.first().is_some_and(|verb| verb == "/x");
                let removing = cmd.args.last().is_some_and(|arg| arg == "REMOVE=ZwoCamera");
                if (last == "REMOVE=ZwoCamera" && removing) || (last == "/l*v" && removes) {
                    Err(1603)
                } else {
                    answering.borrow_mut().answer(cmd)
                }
            });
            let (result, _) = fixture.verify(&sim, script, &Request::default());
            let error = result.unwrap_err().to_string();
            assert!(error.starts_with(expected), "{expected}\n got: {error}");
        }
    }

    /// A box with no hardware may answer either way: phd2-guider's `/health`
    /// 200 or 503, and qhy-camera started or reporting its DLL missing.
    #[test]
    fn accepts_either_answer_a_box_without_hardware_gives() {
        let fixture = Fixture::new();
        let sim = fixture.sim();
        sim.borrow_mut().qhy_log = "Service started successfully\n";
        let script =
            Script::default().on("curl --max-time 5 -o NUL -s -w", || Ok("204".to_owned()));
        let (result, _) = fixture.verify(&sim, script, &Request::default());
        assert_eq!(result.unwrap(), 9);
    }

    #[test]
    fn refuses_a_box_it_cannot_verify_on() {
        let refusal = |fixture: &Fixture, script: Script, request: &Request| match fixture
            .verify(&fixture.sim(), script, request)
            .0
        {
            Err(Error::Refused(reason)) => reason,
            other => panic!("{other:?}"),
        };
        let fixture = Fixture::new();
        assert_eq!(
            refusal(
                &fixture,
                Script::default().on("fltmc", || Err(())),
                &Request::default()
            ),
            "must run elevated (msiexec /qn and service control need it)"
        );
        let installed = Script::default().on("powershell", || Ok("rusty-photon-rp\n".to_owned()));
        assert_eq!(
            refusal(&fixture, installed, &Request::default()),
            "rusty-photon-* services already installed: msiexec /x the previous install first"
        );
        fs::create_dir_all(fixture.places().data).unwrap();
        let reason = refusal(&fixture, Script::default(), &Request::default());
        assert!(reason.ends_with("already exists: delete it (the manual purge) so the fresh-install checks mean something"), "{reason}");
        let fixture = Fixture::new();
        fs::remove_file(fixture.msi("0.1.0")).unwrap();
        let reason = refusal(&fixture, Script::default(), &Request::default());
        assert!(
            reason
                .ends_with("rusty-photon-0.1.0-x64.msi not found: run `cargo ci build-msi` first"),
            "{reason}"
        );
        let request = Request {
            msi: Some(fixture.msi(NIGHTLY)),
            upgrade_from: Some(fixture.dir.path().join("nowhere.msi")),
            keep: false,
        };
        let reason = refusal(&fixture, Script::default(), &request);
        assert!(
            reason.starts_with("--upgrade-from ") && reason.ends_with("nowhere.msi not found"),
            "{reason}"
        );
    }

    fn upgrade(fixture: &Fixture) -> Request {
        Request {
            msi: Some(fixture.msi(NIGHTLY)),
            upgrade_from: Some(fixture.dir.path().join("prior").join(PRIOR)),
            keep: false,
        }
    }

    #[test]
    fn upgrades_in_place_over_an_earlier_msi_and_converges_with_doctor() {
        let fixture = Fixture::new();
        let sim = fixture.sim();
        sim.borrow_mut().arp = vec![format!("rusty-photon {NIGHTLY}")];
        let credential = Rc::new(RefCell::new(String::new()));
        let seen = Rc::clone(&credential);
        let script = Script::default().on_cmd("curl --max-time 5 -o NUL -k -K ", move |cmd| {
            *seen.borrow_mut() = fs::read_to_string(&cmd.args[6]).unwrap();
            Ok(String::new())
        });
        let (result, lines) = fixture.verify(&sim, script, &upgrade(&fixture));
        assert_eq!(result.unwrap(), 9);
        assert_eq!(sim.borrow().logs_cleared_at_install, Some(true));
        assert_eq!(*credential.borrow(), "user = \"observatory:s3cret\"\n");
        let msi = format!("<dist>/{NIGHTLY}/rusty-photon-{NIGHTLY}-x64.msi");
        let services: Vec<String> = SERVICES
            .iter()
            .map(|service| format!("rusty-photon-{service}"))
            .collect();
        let mut expected = vec![
            "fltmc".to_owned(),
            powershell(SERVICES_PS),
            format!("msiexec /i <prior>/{PRIOR} /qn /norestart /l*v <temp>/rusty-photon-msi-prior-install.log ADDLOCAL=ALL"),
            powershell(SERVICES_PS),
        ];
        expected.extend(
            services
                .iter()
                .map(|name| format!("sc.exe config {name} start= disabled")),
        );
        expected.extend(services.iter().map(|name| format!("sc.exe stop {name}")));
        expected.extend([
            format!("msiexec /i {msi} /qn /norestart /l*v <temp>/rusty-photon-msi-install.log ADDLOCAL=ALL"),
            powershell(ARP),
            "<install>/rusty-photon-doctor.exe --fix --json".to_owned(),
        ]);
        for service in [
            "filemonitor",
            "phd2-guider",
            "sentinel",
            "ui-htmx",
            "zwo-camera",
            "zwo-focuser",
        ] {
            expected.push(format!("net stop rusty-photon-{service}"));
            expected.push(format!("net start rusty-photon-{service}"));
        }
        assert_eq!(lines[..expected.len()], expected);
        let sentinel = lines.iter().find(|line| line.contains(":11114")).unwrap();
        assert!(
            sentinel.starts_with("curl --max-time 5 -o NUL -k -K <temp>/")
                && sentinel.ends_with(" -fsS https://127.0.0.1:11114/health"),
            "{sentinel}"
        );
        assert!(
            lines.contains(&curl(11111, "/management/apiversions")),
            "filemonitor's config turns nothing on"
        );
        let places = fixture.places();
        assert!(
            places.data.join("pki/ca.pem").is_file()
                && places.data.join("pki/credential").is_file()
        );
    }

    #[test]
    fn an_upgrade_fails_on_a_side_by_side_install_or_an_unconverged_one() {
        type Breakage = Box<dyn Fn(&mut Sim)>;
        let nightly = format!("rusty-photon {NIGHTLY}");
        let cases: Vec<(Breakage, &str)> = vec![
            (Box::new(|sim| sim.arp.clear()), "FAIL [msiexec]: expected exactly one Rusty Photon entry in Programs and Features after the upgrade, found 0"),
            ({
                let nightly = nightly.clone();
                Box::new(move |sim: &mut Sim| sim.arp = vec![nightly.clone(), "rusty-photon 0.1.0+nightly.202610210507.gdef5678".to_owned()])
            }, "FAIL [msiexec]: expected exactly one Rusty Photon entry in Programs and Features after the upgrade, found 2"),
            (Box::new(|sim| sim.arp = vec!["rusty-photon 0.1.0+nightly.202610210507.gdef5678".to_owned()]), "FAIL [msiexec]: surviving entry's comments 'rusty-photon 0.1.0+nightly.202610210507.gdef5678' != 'rusty-photon 0.1.0+nightly.202610220507.gabc1234'"),
            (Box::new(|sim| sim.fix_report = "panicked".to_owned()), "FAIL [doctor]: doctor --fix --json printed what is not JSON"),
            (Box::new(|sim| sim.fix_report = r#"{"checks": [{"name": "config.retired-keys", "service": "rp", "status": "fail", "detail": "planner.legacy"}]}"#.to_owned()), "FAIL [doctor]: config.retired-keys still failing after --fix (a breaking config change without a doctor remedy?):\nrp: planner.legacy"),
        ];
        for (break_it, expected) in cases {
            let fixture = Fixture::new();
            let sim = fixture.sim();
            sim.borrow_mut().arp = vec![nightly.clone()];
            break_it(&mut sim.borrow_mut());
            let (result, _) = fixture.verify(&sim, Script::default(), &upgrade(&fixture));
            let error = result.unwrap_err().to_string();
            assert!(error.starts_with(expected), "{expected}\n got: {error}");
        }
        // The uninstall keeps what --fix provisioned.
        let fixture = Fixture::new();
        let sim = fixture.sim();
        sim.borrow_mut().arp = vec![nightly.clone()];
        sim.borrow_mut().broken = Some(Broken::UninstallTakes("pki"));
        let (result, _) = fixture.verify(&sim, Script::default(), &upgrade(&fixture));
        assert_eq!(
            result.unwrap_err().to_string(),
            "FAIL [doctor]: provisioned pki\\ca.pem did not survive uninstall (only the manual purge takes it)"
        );
        // A doctor that crashes, on Windows with a negative NTSTATUS too, or
        // provisions no credential.
        for code in [2, -1_073_741_819] {
            let fixture = Fixture::new();
            let sim = fixture.sim();
            sim.borrow_mut().arp = vec![nightly.clone()];
            let line = format!(
                "{}/rusty-photon-doctor.exe --fix",
                fixture.places().install.display()
            );
            let (result, _) = fixture.verify(
                &sim,
                Script::default().on_cmd(&line, move |_| Err(code)),
                &upgrade(&fixture),
            );
            assert_eq!(
                result.unwrap_err().to_string(),
                format!("FAIL [doctor]: doctor --fix exited Some({code})")
            );
        }
        let fixture = Fixture::new();
        let sim = fixture.sim();
        sim.borrow_mut().arp = vec![nightly];
        let line = format!(
            "{}/rusty-photon-doctor.exe --fix",
            fixture.places().install.display()
        );
        let (result, _) = fixture.verify(
            &sim,
            Script::default().on(&line, || Ok("{}".to_owned())),
            &upgrade(&fixture),
        );
        assert_eq!(
            result.unwrap_err().to_string(),
            "FAIL [doctor]: --fix provisioning did not create pki\\ca.pem"
        );
    }

    #[test]
    fn an_earlier_msi_that_fails_to_install_fails_the_run_with_its_log() {
        let fixture = Fixture::new();
        let prior = fixture.dir.path().join("prior").display().to_string();
        let script = Script::default().on_cmd(&format!("msiexec /i {prior}"), |_| Err(1603));
        let (result, _) = fixture.verify(&fixture.sim(), script, &upgrade(&fixture));
        let error = result.unwrap_err().to_string();
        assert!(
            error.starts_with("FAIL [msiexec]: prior-MSI install exited Some(1603) (log: ")
                && error.ends_with("rusty-photon-msi-prior-install.log)"),
            "{error}"
        );
    }

    #[test]
    fn an_earlier_msi_that_leaves_no_config_proves_nothing() {
        let fixture = Fixture::new();
        let sim = fixture.sim();
        let installing = Rc::clone(&sim);
        let prior = fixture.dir.path().join("prior").display().to_string();
        let script = Script::default().on_cmd(&format!("msiexec /i {prior}"), move |_| {
            installing.borrow_mut().installed = true;
            Ok(String::new())
        });
        let (result, _) = fixture.verify(&sim, script, &upgrade(&fixture));
        assert_eq!(
            result.unwrap_err().to_string(),
            "FAIL [sentinel]: timed out after 30s waiting for the prior install's self-created sentinel.json"
        );
        let fixture = Fixture::new();
        let prior = fixture.dir.path().join("prior").display().to_string();
        let script = Script::default().on(&format!("msiexec /i {prior}"), || Ok(String::new()));
        let (result, _) = fixture.verify(&fixture.sim(), script, &upgrade(&fixture));
        assert_eq!(
            result.unwrap_err().to_string(),
            "FAIL [msiexec]: prior MSI installed no services: the upgrade proof would prove nothing"
        );
    }

    /// The seed's logs go before the upgrade, whose checks would read them:
    /// gone already is as good, and a folder it cannot remove fails the run.
    #[test]
    fn the_seeds_logs_go_or_the_upgrade_proves_nothing() {
        for left in [false, true] {
            let fixture = Fixture::new();
            let sim = fixture.sim();
            sim.borrow_mut().arp = vec![format!("rusty-photon {NIGHTLY}")];
            let logs = fixture.places().data.join("logs");
            let answering = Rc::clone(&sim);
            let at = logs.clone();
            // The seed stops zwo-focuser last.
            let script =
                Script::default().on_cmd("sc.exe stop rusty-photon-zwo-focuser", move |cmd| {
                    fs::remove_dir_all(&at).unwrap();
                    if left {
                        // A file, which remove_dir_all refuses.
                        fs::write(&at, "").unwrap();
                    }
                    answering.borrow_mut().answer(cmd)
                });
            let (result, _) = fixture.verify(&sim, script, &upgrade(&fixture));
            if left {
                let error = result.unwrap_err().to_string();
                let path = format!("{}: ", logs.display());
                assert!(error.starts_with(&path), "{error}");
            } else {
                assert_eq!(result.unwrap(), 9);
            }
        }
    }

    /// Only sc.exe's answer for a service that does not exist proves one
    /// removed; any other failure to query it fails the check.
    #[test]
    fn only_a_service_sc_says_does_not_exist_is_gone() {
        for service in ["zwo-camera", "sentinel"] {
            let fixture = Fixture::new();
            let sim = fixture.sim();
            let answering = Rc::clone(&sim);
            let query = format!("sc.exe queryex rusty-photon-{service}");
            // 1722, the RPC server is unavailable, where it would say 1060.
            let script = Script::default().on_cmd(&query, move |cmd| {
                let answer = answering.borrow_mut().answer(cmd);
                answer.map_err(|code| if code == 1060 { 1722 } else { code })
            });
            let (result, _) = fixture.verify(&sim, script, &Request::default());
            assert_eq!(
                result.unwrap_err().to_string(),
                format!(
                    "FAIL [{service}]: sc.exe queryex rusty-photon-{service} exited Some(1722)"
                )
            );
        }
    }

    #[test]
    fn keep_leaves_the_product_installed() {
        let fixture = Fixture::new();
        let sim = fixture.sim();
        let request = Request {
            keep: true,
            ..Request::default()
        };
        let (result, lines) = fixture.verify(&sim, Script::default(), &request);
        assert_eq!(result.unwrap(), 9);
        assert!(sim.borrow().installed);
        assert!(
            lines.last().unwrap().ends_with("REMOVE=ZwoCamera"),
            "{lines:?}"
        );
    }

    #[test]
    fn reads_scs_numbers_and_start_types() {
        let queryex = "SERVICE_NAME: rusty-photon-rp\n        TYPE               : 10  WIN32_OWN_PROCESS\n        STATE              : 4  RUNNING\n                                (STOPPABLE, NOT_PAUSABLE, ACCEPTS_SHUTDOWN)\n        PID                : 4321\n        FLAGS              :\n";
        assert_eq!(sc_number(queryex, "STATE"), Some(4));
        assert_eq!(sc_number(queryex, "PID"), Some(4321));
        assert_eq!(sc_number(queryex, "FLAGS"), None);
        assert_eq!(sc_number(queryex, "WAIT_HINT"), None);
        for (line, start) in [
            ("2   AUTO_START", "Automatic"),
            ("2   AUTO_START  (DELAYED)", "AutomaticDelayedStart"),
            ("3   DEMAND_START", "Manual"),
            ("4   DISABLED", "Disabled"),
            ("0   BOOT_START", "unknown"),
        ] {
            assert_eq!(
                start_type(&format!("        START_TYPE         : {line}\n")),
                start,
                "{line}"
            );
        }
    }

    #[test]
    fn reads_a_log_in_utf_8_or_utf_16() {
        let utf16: Vec<u8> = [0xff, 0xfe]
            .into_iter()
            .chain(
                "MSI (s) Return value 3.\r\n"
                    .encode_utf16()
                    .flat_map(u16::to_le_bytes),
            )
            .collect();
        assert_eq!(text_of(&utf16), "MSI (s) Return value 3.\r\n");
        assert_eq!(text_of(b"plain\n"), "plain\n");
        assert_eq!(last_lines("a\nb\nc\n", 2), ["b", "c"]);
        assert_eq!(last_lines("a\n", 5), ["a"]);
    }

    #[test]
    fn picks_out_the_lines_that_say_what_failed() {
        for line in [
            "Action ended 12:00:01: InstallFinalize. Return value 3.",
            "Error 1920. Service 'rusty-photon-rp' failed to start.",
            "MSI (s) (A0:B4) [12:00:00:000]: Product: Rusty Photon -- Error 1923.",
            "CustomAction RegisterRenewTask returned actual error code 1603",
            "MSI (s) (A0:B4): MainEngineThread is returning 1603",
            "The file could not be opened.",
        ] {
            assert!(says_what_failed(line), "{line}");
        }
        for line in [
            "Action ended 12:00:01: InstallFinalize. Return value 1.",
            "Errors: none",
            "an error 9 mid-line",
            "Property(S): ErrorDialog = ErrorDlg",
        ] {
            assert!(!says_what_failed(line), "{line}");
        }
    }

    #[test]
    fn puts_what_a_command_printed_after_a_colon() {
        assert_eq!(printed("  renewal failed\r\n"), ": renewal failed");
        assert_eq!(printed("\r\n"), "");
    }

    #[test]
    fn reads_the_version_an_msi_is_named_for() {
        assert_eq!(
            msi_version(Path::new("/d/rusty-photon-0.1.0-x64.msi")).as_deref(),
            Some("0.1.0")
        );
        assert_eq!(
            msi_version(Path::new(&format!("rusty-photon-{NIGHTLY}-x64.msi"))).as_deref(),
            Some(NIGHTLY)
        );
        for name in [
            "rusty-photon--x64.msi",
            "rusty-photon-0.1.0.msi",
            "other-0.1.0-x64.msi",
        ] {
            assert_eq!(msi_version(Path::new(name)), None, "{name}");
        }
    }

    #[test]
    fn tails_the_newest_log_of_a_service() {
        let fixture = Fixture::new();
        let places = fixture.places();
        let logs = places.data.join("logs");
        write(&logs.join("rp.2026-10-21"), "old\n");
        std::thread::sleep(Duration::from_millis(20));
        write(&logs.join("rp.2026-10-22"), "1\n2\n3\n");
        write(&logs.join("rp-other.2026-10-23"), "not rp's\n");
        let mut runner = Script::default();
        let mut clock = Stopped::default();
        let run = Run {
            runner: &mut runner,
            clock: &mut clock,
            places: &places,
            install_log: PathBuf::new(),
            uninstall_log: PathBuf::new(),
            credential: None,
            upgraded: false,
        };
        assert_eq!(run.log_tail("rp", 2), "2\n3");
        assert_eq!(run.log_tail("sentinel", 2), "");
        assert!(run.has_log("rp") && !run.has_log("sentinel"));
    }

    #[test]
    fn sorts_the_services_by_kind() {
        let services: Vec<String> = SERVICES.iter().map(|&s| s.to_owned()).collect();
        let (gated, serial, active) = kinds(&services);
        assert_eq!(gated, ["sky-survey-camera"]);
        assert_eq!(serial, ["ppba-driver"]);
        assert_eq!(
            active,
            [
                "filemonitor",
                "phd2-guider",
                "sentinel",
                "ui-htmx",
                "zwo-camera",
                "zwo-focuser"
            ]
        );
    }
}
