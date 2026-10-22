//! `cargo ci verify-brew`: proves the macOS tarballs through Homebrew itself,
//! before anything is published (docs/packaging-macos.md).
//!
//! It renders the channel's formulas ([`crate::brew_formulas`]) with
//! `file://` URLs to the just-built tarballs, into a scratch tap, and
//! installs the meta-formula, which pulls the whole family: the proof that
//! the channel's dependencies resolve. Then, per service, `brew test`, `brew
//! services start`, an HTTP probe, the config it creates itself under
//! `~/Library/Application Support/rusty-photon`, `brew services stop`, and an
//! uninstall that removes the binary and keeps the config, since Homebrew
//! never purges.
//!
//! The kinds of service differ as they do on Linux ([`crate::verify_packages`]):
//! - a [`GATED`] service is installed and never started, since macOS has no
//!   `ConditionPathExists`: started without a config, it would exit and
//!   launchd would restart it, again and again;
//! - a [`SERIAL`] driver exits on its absent device, so it is checked for
//!   its config and its handshake attempt in the log, not a probe;
//! - the cameras, zwo-focuser and phd2-guider create no config
//!   ([`NO_CONFIG_FILE`]), and phd2-guider's `/health` answers 503 with no
//!   PHD2 to talk to;
//! - zwo-focuser finds its focuser through the HID APIs, which under launchd
//!   block until the user grants a privacy permission, which a CI runner
//!   cannot: it is held to staying alive under launchd, then probed in the
//!   foreground;
//! - each zwo service must load its own SDK dylib, and only that one,
//!   through `@rpath` (ADR-014).
//!
//! A run refuses to start while any formula of either channel is installed:
//! the scratch tap's formulas share their kegs, or conflict with them.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::brew_formulas::{self, Channel};
use crate::build_tarballs::tarball;
use crate::packaging::{self, io, GATED, HEALTH_PROBED, NO_CONFIG_FILE, SERIAL};
use crate::process::{self, Cmd, Runner};

/// The scratch tap the formulas are rendered into.
const TAP: &str = "local/rp-verify";

/// What a run verifies.
#[derive(Clone, Debug)]
pub struct Request {
    pub channel: Channel,
    /// The tarballs; `dist/<workspace version>` when `None`. Its name is the
    /// version the tarballs carry.
    pub dist: Option<PathBuf>,
    /// Only these services, and no meta-formula. `None` verifies every
    /// service with a tarball in `dist`.
    pub services: Option<Vec<String>>,
    /// Leave the scratch tap and whatever is installed, to debug.
    pub keep: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Packaging(#[from] packaging::Error),
    #[error(transparent)]
    Formulas(#[from] brew_formulas::Error),
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

/// A `brew` command, run without its auto-update, cleanup and hints.
fn brew(args: &[&str]) -> Cmd {
    Cmd::new("brew", args)
        .env("HOMEBREW_NO_AUTO_UPDATE", "1")
        .env("HOMEBREW_NO_INSTALL_CLEANUP", "1")
        .env("HOMEBREW_NO_ENV_HINTS", "1")
}

/// The services with a tarball of `version` in `dist`, sorted.
fn services_in(dist: &Path, version: &str) -> Result<Vec<String>, Error> {
    let suffix = format!("-{version}-aarch64-apple-darwin.tar.gz");
    let mut services: Vec<String> = fs::read_dir(dist)
        .map_err(io(dist))?
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter_map(|name| {
            name.strip_prefix("rusty-photon-")?
                .strip_suffix(suffix.as_str())
                .map(str::to_owned)
        })
        .collect();
    services.sort();
    services.dedup();
    Ok(services)
}

/// The first formula of either channel, for `services` or the meta-formula,
/// in `installed`, which is `brew list --formula`'s output.
fn preinstalled(installed: &str, services: &[String]) -> Option<String> {
    let names: Vec<&str> = installed.lines().collect();
    let formulas = services
        .iter()
        .map(|service| format!("rusty-photon-{service}"))
        .chain(["rusty-photon".to_owned()]);
    formulas
        .flat_map(|formula| [formula.clone(), format!("{formula}-nightly")])
        .find(|formula| names.contains(&formula.as_str()))
}

/// The first `"pid": <n>` in `brew services info --json`'s output.
fn pid(info: &str) -> Option<u32> {
    info.split("\"pid\":").nth(1).and_then(|rest| {
        let digits: String = rest
            .trim_start()
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        digits.parse().ok()
    })
}

/// Whether `brew services info --json` says the service runs.
fn running(info: &str) -> bool {
    info.contains("\"running\": true")
}

fn self_creates_config(service: &str) -> bool {
    !GATED.contains(&service) && !NO_CONFIG_FILE.contains(&service)
}

fn probe_path(service: &str) -> &'static str {
    if HEALTH_PROBED.contains(&service) {
        "/health"
    } else {
        "/management/apiversions"
    }
}

#[cfg(unix)]
fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn executable(path: &Path) -> bool {
    path.is_file()
}

/// The last `count` lines of the file at `path`, or nothing.
fn tail(path: &Path, count: usize) -> String {
    let text = fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    let skip = lines.len().saturating_sub(count);
    lines.into_iter().skip(skip).collect::<Vec<_>>().join("\n")
}

/// A run in progress: Homebrew's places, and what it has started.
struct Run<'a, R: Runner> {
    runner: &'a mut R,
    sleep: fn(Duration),
    suffix: &'static str,
    prefix: PathBuf,
    tap_dir: PathBuf,
    config: PathBuf,
    home: PathBuf,
    services: Vec<String>,
    subset: bool,
    started: Vec<String>,
}

impl<R: Runner> Run<'_, R> {
    /// The service's formula, as the scratch tap names it.
    fn formula(&self, service: &str) -> String {
        format!("{TAP}/rusty-photon-{service}{}", self.suffix)
    }

    fn installed_name(&self, service: &str) -> String {
        format!("rusty-photon-{service}{}", self.suffix)
    }

    fn log(&self, service: &str) -> PathBuf {
        self.prefix
            .join(format!("var/log/rusty-photon-{service}.log"))
    }

    fn bin(&self, service: &str) -> PathBuf {
        self.prefix.join(format!("bin/rusty-photon-{service}"))
    }

    fn config_file(&self, service: &str) -> PathBuf {
        self.config.join(format!("{service}.json"))
    }

    /// Checks `ready` up to `attempts` times, a second apart.
    fn wait_until(&mut self, attempts: u32, mut ready: impl FnMut(&mut Self) -> bool) -> bool {
        for attempt in 1..=attempts {
            if ready(self) {
                return true;
            }
            if attempt < attempts {
                (self.sleep)(Duration::from_secs(1));
            }
        }
        false
    }

    /// Whether `url` answers with a success.
    fn answers(&mut self, url: &str) -> bool {
        let curl = Cmd::new("curl", &["-fsS", "-o", "/dev/null", url]);
        self.runner.code(&curl).is_ok_and(|code| code == Some(0))
    }

    fn info(&mut self, service: &str) -> String {
        let formula = self.installed_name(service);
        self.runner
            .read(&brew(&["services", "info", &formula, "--json"]))
            .unwrap_or_default()
    }

    /// Prints where a live but unresponsive process is parked.
    fn sample(&mut self, pid: u32) -> String {
        self.runner
            .read(&Cmd::new("sample", &[&pid.to_string(), "1"]))
            .map(|stack| stack.lines().take(40).collect::<Vec<_>>().join("\n"))
            .unwrap_or_default()
    }

    /// The service's failure, with what can say why on stderr.
    fn fail(&mut self, service: &str, reason: &str) -> Error {
        eprintln!("verify-brew: FAIL [{service}]: {reason}");
        let log = self.log(service);
        eprintln!("--- service log tail ({})", log.display());
        eprintln!("{}", tail(&log, 40));
        eprintln!("--- brew services info");
        let info = self.info(service);
        eprintln!("{info}");
        if let Some(pid) = pid(&info) {
            eprintln!("--- stack sample (pid {pid})");
            eprintln!("{}", self.sample(pid));
        }
        // A foreground run shows what launchd cannot: a loader abort prints
        // to stderr, a crash exits on a signal with nothing printed, a good
        // start prints its startup lines.
        let bin = self.bin(service);
        if executable(&bin) {
            eprintln!("--- direct foreground run (8s)");
            let direct = tempfile::NamedTempFile::new().ok();
            let out = direct
                .as_ref()
                .map_or_else(PathBuf::new, |direct| direct.path().to_path_buf());
            if let Ok(mut started) = self
                .runner
                .start(&Cmd::new(&bin.to_string_lossy(), &[]), &out)
            {
                (self.sleep)(Duration::from_secs(8));
                let end = started.exited().map_or_else(
                    || {
                        started.stop();
                        "direct run: still alive after 8s (killed)".to_owned()
                    },
                    |status| format!("direct run: exited with {status}"),
                );
                eprintln!("{}\n{end}", tail(&out, 40));
            }
        }
        eprintln!("--- newest crash report (if any)");
        if let Some(report) = self.newest_crash_report(service) {
            let text = fs::read(&report).unwrap_or_default();
            let head: Vec<u8> = text.into_iter().take(4000).collect();
            eprintln!("{}", String::from_utf8_lossy(&head));
        }
        Error::Failed {
            service: service.to_owned(),
            reason: reason.to_owned(),
        }
    }

    fn newest_crash_report(&self, service: &str) -> Option<PathBuf> {
        let dir = self.home.join("Library/Logs/DiagnosticReports");
        let prefix = format!("rusty-photon-{service}");
        fs::read_dir(dir)
            .ok()?
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(&prefix))
            .filter_map(|entry| Some((entry.metadata().ok()?.modified().ok()?, entry.path())))
            .max()
            .map(|(_, path)| path)
    }

    /// Fails `service` unless `ok`.
    fn require(&mut self, ok: bool, service: &str, reason: &str) -> Result<(), Error> {
        if ok {
            Ok(())
        } else {
            Err(self.fail(service, reason))
        }
    }

    fn stop(&mut self, service: &str) -> Result<(), Error> {
        let formula = self.installed_name(service);
        let stopped = self
            .runner
            .read(&brew(&["services", "stop", &formula]))
            .is_ok();
        self.require(stopped, service, "brew services stop failed")
    }

    fn install(&mut self) -> Result<(), Error> {
        if self.subset {
            for service in self.services.clone() {
                println!("== {service}: install");
                let formula = self.formula(&service);
                let installed = self.runner.run(&brew(&["install", &formula])).is_ok();
                self.require(installed, &service, "brew install failed")?;
            }
        } else {
            let meta = format!("{TAP}/rusty-photon{}", self.suffix);
            println!(
                "== rusty-photon{}: install (meta — pulls the whole family)",
                self.suffix
            );
            self.runner
                .run(&brew(&["install", &meta]))
                .map_err(|_| refused("meta-formula install failed"))?;
        }
        Ok(())
    }

    /// Exactly its own SDK dylib, loaded through `@rpath` and in the keg
    /// (ADR-014).
    fn zwo_dylib(&mut self, service: &str, own: &str, others: [&str; 2]) -> Result<(), Error> {
        let bin = self.bin(service);
        let loads = self
            .runner
            .read(&Cmd::new("otool", &["-L", &bin.to_string_lossy()]))
            .unwrap_or_default();
        self.require(
            loads.contains(&format!("@rpath/{own}.dylib")),
            service,
            &format!("binary does not load @rpath/{own}.dylib"),
        )?;
        self.require(
            !others.iter().any(|other| loads.contains(other)),
            service,
            &format!(
                "binary links SDKs it must not ({}; ADR-014 per-device link)",
                others.join(", ")
            ),
        )?;
        let keg = self.prefix.join(format!(
            "opt/{}/lib/{own}.dylib",
            self.installed_name(service)
        ));
        self.require(
            keg.exists(),
            service,
            &format!("bundled {own}.dylib missing from the keg"),
        )
    }

    /// Doctor rides in sentinel's formula: installed and runnable, its daily
    /// `tls renew` exits 0 with nothing due, and the renewal plist is in the
    /// keg with this keg's paths.
    fn sentinel(&mut self, service: &str) -> Result<(), Error> {
        let doctor = self.prefix.join("bin/rusty-photon-doctor");
        self.require(
            executable(&doctor),
            service,
            "rusty-photon-doctor missing after install",
        )?;
        let doctor = doctor.to_string_lossy().into_owned();
        // 1 is a finding, which is fine here; more is a broken tool.
        let code = self.runner.code(&Cmd::new(&doctor, &["--json"]))?;
        self.require(
            code.is_some_and(|code| code <= 1),
            service,
            &format!("rusty-photon-doctor --json exited {code:?}"),
        )?;
        let renew = self.runner.code(&Cmd::new(&doctor, &["tls", "renew"]))?;
        self.require(
            renew == Some(0),
            service,
            "rusty-photon-doctor tls renew (nothing due) did not exit 0",
        )?;
        let plist = self.prefix.join(format!(
            "opt/{}/rusty-photon-renew.plist",
            self.installed_name(service)
        ));
        let text = fs::read_to_string(&plist).unwrap_or_default();
        self.require(
            plist.is_file(),
            service,
            "rusty-photon-renew.plist missing from the keg",
        )?;
        self.require(
            text.contains("rusty-photon-doctor"),
            service,
            "renewal plist does not run rusty-photon-doctor",
        )?;
        let lint = self
            .runner
            .code(&Cmd::new("plutil", &["-lint", &plist.to_string_lossy()]))?;
        self.require(lint == Some(0), service, "renewal plist does not lint")
    }

    /// zwo-focuser under launchd, blocked on the HID privacy grant: alive,
    /// not restarting, and serving when run in the foreground.
    fn hid_gated(&mut self, service: &str, url: &str, port: u16) -> Result<(), Error> {
        (self.sleep)(Duration::from_secs(3));
        let info = self.info(service);
        self.require(
            running(&info),
            service,
            "launchd process not running (expected alive-but-blocked on the HID privacy grant)",
        )?;
        if let Some(pid) = pid(&info) {
            println!("-- {service}: launchd instance blocked pre-bind; stack sample of pid {pid}:");
            println!("{}", self.sample(pid));
        }
        self.stop(service)?;
        let bin = self.bin(service).to_string_lossy().into_owned();
        let log = self.log(service);
        let mut foreground = self.runner.start(&Cmd::new(&bin, &[]), &log)?;
        let served = self.wait_until(30, |run| run.answers(url));
        foreground.stop();
        self.require(
            served,
            service,
            &format!(
                "no HTTP response on port {port} ({}) even in a foreground run",
                probe_path(service)
            ),
        )?;
        println!(
            "== {service}: OK (foreground serve, port {port}; launchd probe skipped — HID discovery \
             needs a privacy grant, see docs/packaging-macos.md)"
        );
        Ok(())
    }

    /// phd2-guider answers 503 without PHD2, which still shows it serves.
    fn phd2_answers(&mut self, url: &str) -> Option<String> {
        let mut last = String::new();
        let answered = self.wait_until(30, |run| {
            let curl = Cmd::new(
                "curl",
                &["-s", "-o", "/dev/null", "-w", "%{http_code}", url],
            );
            last = run.runner.read(&curl).unwrap_or_else(|_| "000".to_owned());
            last == "200" || last == "503"
        });
        (!answered).then_some(last)
    }

    fn check(&mut self, service: &str) -> Result<(), Error> {
        let bin = self.bin(service);
        self.require(
            executable(&bin),
            service,
            &format!("{} missing after install", bin.display()),
        )?;
        let formula = self.formula(service);
        let tested = self.runner.run(&brew(&["test", &formula])).is_ok();
        self.require(tested, service, "brew test failed")?;
        match service {
            "zwo-camera" => {
                self.zwo_dylib(service, "libASICamera2", ["libEFWFilter", "libEAFFocuser"])?;
            }
            "zwo-focuser" => {
                self.zwo_dylib(service, "libEAFFocuser", ["libASICamera2", "libEFWFilter"])?;
            }
            "sentinel" => self.sentinel(service)?,
            _ => {}
        }

        let config = self.config_file(service);
        if GATED.contains(&service) {
            self.require(
                !config.exists(),
                service,
                "config exists for a gated service that never ran",
            )?;
            println!("== {service}: OK (installed; gated on config — not started)");
            return Ok(());
        }

        println!("== {service}: brew services start");
        let _ = fs::remove_file(self.log(service));
        let name = self.installed_name(service);
        let started = self
            .runner
            .run(&brew(&["services", "start", &name]))
            .is_ok();
        self.require(started, service, "brew services start failed")?;
        self.started.push(service.to_owned());

        let has_config = |run: &mut Self| {
            fs::metadata(run.config_file(service)).is_ok_and(|meta| meta.len() > 0)
        };
        if SERIAL.contains(&service) {
            // It starts, creates its config and fails on the absent device,
            // not before: a loader or config problem would end it before
            // the handshake.
            let log = self.log(service);
            let tried = self.wait_until(30, |_| {
                fs::read_to_string(&log).is_ok_and(|text| text.contains("eager startup handshake"))
            });
            self.require(
                tried,
                service,
                "no eager-handshake attempt in the service log",
            )?;
            let created = self.wait_until(15, has_config);
            self.require(
                created,
                service,
                &format!("config not self-created at {}", config.display()),
            )?;
            self.stop(service)?;
            println!("== {service}: OK (config self-created; respawning on absent serial device)");
            return Ok(());
        }

        let port = packaging::port(service).unwrap_or_default();
        let url = format!("http://127.0.0.1:{port}{}", probe_path(service));
        if service == "zwo-focuser" {
            return self.hid_gated(service, &url, port);
        }
        if service == "phd2-guider" {
            if let Some(last) = self.phd2_answers(&url) {
                return Err(self.fail(
                    service,
                    &format!(
                        "no HTTP response on port {port} ({}; last code {last})",
                        probe_path(service)
                    ),
                ));
            }
        } else {
            let served = self.wait_until(30, |run| run.answers(&url));
            self.require(
                served,
                service,
                &format!("no HTTP response on port {port} ({})", probe_path(service)),
            )?;
        }
        if self_creates_config(service) {
            let created = self.wait_until(15, has_config);
            self.require(
                created,
                service,
                &format!("config not self-created at {}", config.display()),
            )?;
        }
        self.stop(service)?;
        println!("== {service}: OK (service, port {port})");
        Ok(())
    }

    /// The meta-formula first, as Homebrew refuses to uninstall what an
    /// installed formula depends on; then each service, which keeps its
    /// config, since Homebrew never purges.
    fn uninstall(&mut self) -> Result<(), Error> {
        if !self.subset {
            let meta = format!("rusty-photon{}", self.suffix);
            self.runner
                .run(&brew(&["uninstall", &meta]))
                .map_err(|_| refused("meta-formula uninstall failed"))?;
        }
        for service in self.services.clone() {
            println!("== {service}: uninstall");
            let name = self.installed_name(&service);
            let removed = self.runner.run(&brew(&["uninstall", &name])).is_ok();
            self.require(removed, &service, "brew uninstall failed")?;
            let gone = !self.bin(&service).exists();
            self.require(gone, &service, "binary survived uninstall")?;
            if self_creates_config(&service) {
                let kept = self.config_file(&service).is_file();
                self.require(
                    kept,
                    &service,
                    "config did not survive uninstall (brew never purges)",
                )?;
            }
        }
        Ok(())
    }

    /// Leaves Homebrew as the run found it, whatever happened. Every step
    /// may find nothing to do.
    fn clean_up(&mut self) {
        for service in std::mem::take(&mut self.started) {
            let name = self.installed_name(&service);
            let _ = self.runner.read(&brew(&["services", "stop", &name]));
        }
        let meta = format!("rusty-photon{}", self.suffix);
        let _ = self.runner.read(&brew(&["uninstall", "--force", &meta]));
        for service in self.services.clone() {
            let name = self.installed_name(&service);
            let _ = self.runner.read(&brew(&["uninstall", "--force", &name]));
        }
        if self.runner.read(&brew(&["untap", TAP])).is_err() {
            let _ = fs::remove_dir_all(&self.tap_dir);
        }
    }
}

/// Verifies the tarballs `request` names and returns how many services
/// passed.
///
/// # Errors
///
/// When a service fails a check, or the run cannot be set up: no tarballs, a
/// service without a port, a formula already installed.
pub fn verify(
    runner: &mut impl Runner,
    root: &Path,
    home: &Path,
    request: &Request,
    sleep: fn(Duration),
) -> Result<usize, Error> {
    let dist = match &request.dist {
        Some(dist) => dist.clone(),
        None => root.join("dist").join(packaging::workspace_version(root)?),
    };
    if !dist.is_dir() {
        return Err(refused(format!(
            "{} not found — run `cargo ci build-tarballs` first",
            dist.display()
        )));
    }
    let dist = fs::canonicalize(&dist).map_err(io(&dist))?;
    // build-tarballs names the directory after the version it stamps.
    let version = dist
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let services = match &request.services {
        Some(services) => services.clone(),
        None => services_in(&dist, &version)?,
    };
    if services.is_empty() {
        return Err(refused(format!(
            "no {} in {}",
            tarball("*", &version),
            dist.display()
        )));
    }
    let suffix = match request.channel {
        Channel::Stable => "",
        Channel::Nightly => "-nightly",
    };
    let channel = if suffix.is_empty() {
        "stable"
    } else {
        "nightly"
    };
    println!("Verifying ({channel}): {}", services.join(" "));
    let unmapped: Vec<&str> = services
        .iter()
        .map(String::as_str)
        .filter(|service| packaging::port(service).is_none())
        .collect();
    if !unmapped.is_empty() {
        return Err(refused(format!(
            "no port for: {} — extend packaging::PORTS",
            unmapped.join(" ")
        )));
    }

    // A clean slate, as a fresh container is on Linux: an installed
    // formula of either channel would be verified, and uninstalled, in
    // place of the new one, or conflict with it. This also means everything
    // the clean-up removes is something this run installed.
    let installed = runner
        .read(&brew(&["list", "--formula"]))
        .unwrap_or_default();
    if let Some(formula) = preinstalled(&installed, &services) {
        return Err(refused(format!(
            "{formula} is already installed — uninstall it before verifying (the scratch tap's \
             formulas share its keg or conflict with it). A verify run that was interrupted \
             leaves its formulas installed: `brew uninstall --force` them, then \
             `brew untap {TAP}`"
        )));
    }

    let repository = runner.read(&brew(&["--repository"]))?;
    let prefix = runner.read(&brew(&["--prefix"]))?;
    let mut run = Run {
        runner,
        sleep,
        suffix,
        prefix: PathBuf::from(prefix.trim_end()),
        tap_dir: Path::new(repository.trim_end()).join("Library/Taps/local/homebrew-rp-verify"),
        config: home.join("Library/Application Support/rusty-photon"),
        home: home.to_path_buf(),
        services,
        subset: request.services.is_some(),
        started: Vec::new(),
    };
    let result = verify_in_tap(&mut run, root, &dist, &version, request);
    if request.keep {
        println!("Kept: tap {TAP} (remove: brew untap {TAP}) and any installed formulas");
    } else {
        run.clean_up();
    }
    let count = run.services.len();
    result.map(|()| count)
}

fn verify_in_tap<R: Runner>(
    run: &mut Run<'_, R>,
    root: &Path,
    dist: &Path,
    version: &str,
    request: &Request,
) -> Result<(), Error> {
    // A real tap, laid out as Homebrew expects, so that installing and
    // untapping behave; nothing in it outlives the run.
    if !run.tap_dir.is_dir() {
        run.runner.read(&brew(&["tap-new", "--no-git", TAP]))?;
    }
    let formulas = run.tap_dir.join("Formula");
    fs::create_dir_all(&formulas).map_err(io(&formulas))?;
    // Homebrew trusts the formula named on the command line but not the
    // dependencies an untrusted tap gives it, which is how the meta-formula
    // pulls the family. Older Homebrews have no `trust`.
    if run.runner.read(&brew(&["trust", "--help"])).is_ok() {
        run.runner.read(&brew(&["trust", TAP]))?;
    }
    // launchd creates the log file, not its directory.
    let logs = run.prefix.join("var/log");
    fs::create_dir_all(&logs).map_err(io(&logs))?;

    let rendered = brew_formulas::render(
        root,
        &brew_formulas::Request {
            channel: request.channel,
            version: version.to_owned(),
            dist: dist.to_path_buf(),
            url_base: format!("file://{}", dist.display()),
            output: formulas,
            services: request.services.clone(),
        },
    )?;
    for path in rendered {
        println!("rendered {}", path.display());
    }

    run.install()?;
    for service in run.services.clone() {
        run.check(&service)?;
    }
    run.uninstall()?;
    run.started.clear();
    let channel = if run.suffix.is_empty() {
        "stable"
    } else {
        "nightly"
    };
    println!(
        "\nverify-brew: OK ({} {channel} formulas)",
        run.services.len()
    );
    Ok(())
}

// The tools run on a Mac; their tests compare Unix paths.
#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use crate::process::Script;

    #[test]
    fn finds_the_services_with_a_tarball_of_the_version() {
        let dist = tempfile::tempdir().unwrap();
        for name in [
            tarball("zwo-camera", "0.1.0"),
            tarball("filemonitor", "0.1.0"),
            tarball("rp", "0.0.9"),
            "SHA256SUMS.txt".to_owned(),
        ] {
            fs::write(dist.path().join(name), "").unwrap();
        }
        assert_eq!(
            services_in(dist.path(), "0.1.0").unwrap(),
            ["filemonitor", "zwo-camera"]
        );
    }

    #[test]
    fn finds_a_formula_of_either_channel_already_installed() {
        let services = ["filemonitor".to_owned()];
        assert_eq!(preinstalled("wget\nlibusb\n", &services), None);
        assert_eq!(
            preinstalled("libusb\nrusty-photon-filemonitor\n", &services).as_deref(),
            Some("rusty-photon-filemonitor")
        );
        assert_eq!(
            preinstalled("rusty-photon-nightly\n", &services).as_deref(),
            Some("rusty-photon-nightly")
        );
        // Another service's formula is no conflict for this run.
        assert_eq!(preinstalled("rusty-photon-rp\n", &services), None);
    }

    #[test]
    fn reads_the_pid_and_state_brew_reports() {
        let info = r#"[{"name":"x","running": true,"pid": 4242,"exit_code":null}]"#;
        assert_eq!(pid(info), Some(4242));
        assert!(running(info));
        assert_eq!(pid(r#"[{"running": false,"pid":null}]"#), None);
        assert!(!running(r#"[{"running": false}]"#));
    }

    #[test]
    fn probes_and_configs_by_kind_of_service() {
        assert_eq!(probe_path("sentinel"), "/health");
        assert_eq!(probe_path("filemonitor"), "/management/apiversions");
        assert!(self_creates_config("filemonitor"));
        assert!(!self_creates_config("plate-solver"));
        assert!(!self_creates_config("zwo-camera"));
    }

    #[test]
    fn tails_a_file() {
        let file = tempfile::NamedTempFile::new().unwrap();
        fs::write(file.path(), "a\nb\nc\n").unwrap();
        assert_eq!(tail(file.path(), 2), "b\nc");
        assert_eq!(tail(file.path(), 9), "a\nb\nc");
        assert_eq!(tail(Path::new("/no/such/file"), 2), "");
    }

    /// A repository with `services` packaged and described, their tarballs
    /// built, and a Homebrew whose prefix and repository are directories here.
    struct Mac {
        dir: tempfile::TempDir,
    }

    impl Mac {
        fn new(services: &[&str]) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let mac = Self { dir };
            fs::create_dir_all(mac.root()).unwrap();
            fs::write(
                mac.root().join("Cargo.toml"),
                "[workspace]\nmembers = []\n\n[workspace.package]\nversion = \"0.1.0\"\n",
            )
            .unwrap();
            let dist = mac.root().join("dist/0.1.0");
            fs::create_dir_all(&dist).unwrap();
            for service in services {
                let dir = mac.root().join("services").join(service);
                fs::create_dir_all(dir.join("pkg")).unwrap();
                let manifest =
                    format!("[package]\nname = \"{service}\"\ndescription = \"{service}\"\n");
                fs::write(dir.join("Cargo.toml"), manifest).unwrap();
                fs::write(dist.join(tarball(service, "0.1.0")), service).unwrap();
            }
            fs::write(dist.join("SHA256SUMS.txt"), "sums").unwrap();
            for dir in ["prefix/bin", "repository", "home"] {
                fs::create_dir_all(mac.dir.path().join(dir)).unwrap();
            }
            mac
        }

        fn root(&self) -> PathBuf {
            self.dir.path().join("root")
        }

        fn path(&self, path: &str) -> PathBuf {
            self.dir.path().join(path)
        }

        /// The binary `brew install` would put in the prefix.
        fn install(&self, name: &str) {
            let bin = self.path("prefix/bin").join(name);
            fs::write(&bin, "").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
            }
        }

        /// A Homebrew that has nothing of ours installed and reports its
        /// places as this directory's.
        fn brew(&self) -> Script {
            let repository = format!("{}\n", self.path("repository").display());
            let prefix = format!("{}\n", self.path("prefix").display());
            Script::default()
                .on("brew --repository", move || Ok(repository.clone()))
                .on("brew --prefix", move || Ok(prefix.clone()))
        }

        fn request(services: Option<&[&str]>) -> Request {
            Request {
                channel: Channel::Nightly,
                dist: None,
                services: services.map(|services| services.iter().map(|&s| s.to_owned()).collect()),
                keep: false,
            }
        }

        fn verify(&self, runner: &mut Script, request: &Request) -> Result<usize, Error> {
            fn no_sleep(_: Duration) {}
            let root = self.root();
            verify(runner, &root, &self.path("home"), request, no_sleep)
        }

        fn shorten(&self, lines: &[String]) -> Vec<String> {
            let dir = self.dir.path().display().to_string();
            lines
                .iter()
                .map(|line| line.replace(&dir, "<mac>"))
                .collect()
        }
    }

    #[test]
    fn verifies_a_service_through_its_whole_lifecycle() {
        let mac = Mac::new(&["filemonitor"]);
        mac.install("rusty-photon-filemonitor");
        let config = mac.path("home/Library/Application Support/rusty-photon/filemonitor.json");
        let bin = mac.path("prefix/bin/rusty-photon-filemonitor");
        let mut runner = mac
            .brew()
            .on("brew services start", move || {
                fs::create_dir_all(config.parent().unwrap()).unwrap();
                fs::write(&config, "{}").unwrap();
                Ok(String::new())
            })
            .on("brew uninstall rusty-photon-filemonitor", move || {
                let _ = fs::remove_file(&bin);
                Ok(String::new())
            });
        let count = mac.verify(&mut runner, &Mac::request(None)).unwrap();
        assert_eq!(count, 1);
        assert_eq!(
            mac.shorten(&runner.lines),
            [
                "brew list --formula",
                "brew --repository",
                "brew --prefix",
                "brew tap-new --no-git local/rp-verify",
                "brew trust --help",
                "brew trust local/rp-verify",
                "brew install local/rp-verify/rusty-photon-nightly",
                "brew test local/rp-verify/rusty-photon-filemonitor-nightly",
                "brew services start rusty-photon-filemonitor-nightly",
                "curl -fsS -o /dev/null http://127.0.0.1:11111/management/apiversions",
                "brew services stop rusty-photon-filemonitor-nightly",
                "brew uninstall rusty-photon-nightly",
                "brew uninstall rusty-photon-filemonitor-nightly",
                "brew uninstall --force rusty-photon-nightly",
                "brew uninstall --force rusty-photon-filemonitor-nightly",
                "brew untap local/rp-verify",
            ]
        );
        let formulas = mac.path("repository/Library/Taps/local/homebrew-rp-verify/Formula");
        assert!(formulas
            .join("rusty-photon-filemonitor-nightly.rb")
            .is_file());
        assert!(formulas.join("rusty-photon-nightly.rb").is_file());
    }

    #[test]
    fn a_failed_check_cleans_up_and_names_the_service() {
        let mac = Mac::new(&["filemonitor"]);
        mac.install("rusty-photon-filemonitor");
        let mut runner = mac.brew().on("brew test", || Err(()));
        let error = mac.verify(&mut runner, &Mac::request(None)).unwrap_err();
        assert_eq!(error.to_string(), "FAIL [filemonitor]: brew test failed");
        let lines = mac.shorten(&runner.lines);
        // Then the diagnostics, a foreground run among them, and the clean-up.
        let after: Vec<&str> = lines
            .iter()
            .skip_while(|line| !line.starts_with("brew test"))
            .skip(1)
            .map(String::as_str)
            .collect();
        assert_eq!(
            after,
            [
                "brew services info rusty-photon-filemonitor-nightly --json",
                "<mac>/prefix/bin/rusty-photon-filemonitor",
                "brew uninstall --force rusty-photon-nightly",
                "brew uninstall --force rusty-photon-filemonitor-nightly",
                "brew untap local/rp-verify",
            ]
        );
    }

    #[test]
    fn refuses_to_start_while_a_formula_is_installed() {
        let mac = Mac::new(&["filemonitor"]);
        let mut runner = mac.brew().on("brew list --formula", || {
            Ok("rusty-photon-filemonitor-nightly\n".to_owned())
        });
        let error = mac.verify(&mut runner, &Mac::request(None)).unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("rusty-photon-filemonitor-nightly is already installed"),
            "{error}"
        );
        assert_eq!(runner.lines, ["brew list --formula"]);
    }

    #[test]
    fn installs_a_gated_service_and_never_starts_it() {
        let mac = Mac::new(&["plate-solver"]);
        mac.install("rusty-photon-plate-solver");
        let bin = mac.path("prefix/bin/rusty-photon-plate-solver");
        let mut runner = mac
            .brew()
            .on("brew uninstall rusty-photon-plate-solver", move || {
                let _ = fs::remove_file(&bin);
                Ok(String::new())
            });
        let request = Mac::request(Some(&["plate-solver"]));
        mac.verify(&mut runner, &request).unwrap();
        let lines = mac.shorten(&runner.lines);
        assert!(
            !lines.iter().any(|line| line.contains("services start")),
            "{lines:?}"
        );
        // A subset installs each formula, and has no meta-formula.
        assert!(lines.contains(
            &"brew install local/rp-verify/rusty-photon-plate-solver-nightly".to_owned()
        ));
        assert!(!lines.contains(&"brew uninstall rusty-photon-nightly".to_owned()));
    }

    #[test]
    fn a_serial_driver_needs_its_handshake_and_its_config_not_a_probe() {
        let mac = Mac::new(&["ppba-driver"]);
        mac.install("rusty-photon-ppba-driver");
        let log = mac.path("prefix/var/log/rusty-photon-ppba-driver.log");
        let config = mac.path("home/Library/Application Support/rusty-photon/ppba-driver.json");
        let bin = mac.path("prefix/bin/rusty-photon-ppba-driver");
        let mut runner = mac
            .brew()
            .on("brew services start", move || {
                fs::write(&log, "ppba: eager startup handshake failed\n").unwrap();
                fs::create_dir_all(config.parent().unwrap()).unwrap();
                fs::write(&config, "{}").unwrap();
                Ok(String::new())
            })
            .on("brew uninstall rusty-photon-ppba-driver", move || {
                let _ = fs::remove_file(&bin);
                Ok(String::new())
            });
        mac.verify(&mut runner, &Mac::request(Some(&["ppba-driver"])))
            .unwrap();
        assert!(
            !runner.lines.iter().any(|line| line.starts_with("curl")),
            "{:?}",
            runner.lines
        );
    }

    #[test]
    fn a_serial_driver_that_never_tries_its_device_fails() {
        let mac = Mac::new(&["ppba-driver"]);
        mac.install("rusty-photon-ppba-driver");
        let mut runner = mac.brew();
        let error = mac
            .verify(&mut runner, &Mac::request(Some(&["ppba-driver"])))
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "FAIL [ppba-driver]: no eager-handshake attempt in the service log"
        );
    }

    #[test]
    fn a_service_that_never_answers_fails_after_thirty_probes() {
        let mac = Mac::new(&["filemonitor"]);
        mac.install("rusty-photon-filemonitor");
        let mut runner = mac.brew().on("curl", || Err(()));
        let error = mac.verify(&mut runner, &Mac::request(None)).unwrap_err();
        assert_eq!(
            error.to_string(),
            "FAIL [filemonitor]: no HTTP response on port 11111 (/management/apiversions)"
        );
        let probes = runner
            .lines
            .iter()
            .filter(|line| line.starts_with("curl"))
            .count();
        assert_eq!(probes, 30);
    }

    /// A service that installs, answers its probe and needs no config:
    /// zwo-camera, phd2-guider and zwo-focuser are such.
    fn quiet_service(mac: &Mac, service: &'static str) -> Script {
        mac.install(&format!("rusty-photon-{service}"));
        let bin = mac.path(&format!("prefix/bin/rusty-photon-{service}"));
        mac.brew().on(
            &format!("brew uninstall rusty-photon-{service}"),
            move || {
                let _ = fs::remove_file(&bin);
                Ok(String::new())
            },
        )
    }

    #[test]
    fn a_zwo_service_loads_its_own_dylib_from_the_keg_and_no_other() {
        let mac = Mac::new(&["zwo-camera"]);
        let keg = mac.path("prefix/opt/rusty-photon-zwo-camera-nightly/lib");
        fs::create_dir_all(&keg).unwrap();
        fs::write(keg.join("libASICamera2.dylib"), "").unwrap();
        let own = "\t@rpath/libASICamera2.dylib (compatibility version 0.0.0)\n";
        let request = Mac::request(Some(&["zwo-camera"]));

        let mut runner =
            quiet_service(&mac, "zwo-camera").on("otool -L", move || Ok(own.to_owned()));
        mac.verify(&mut runner, &request).unwrap();

        let both = format!("{own}\t@rpath/libEAFFocuser.dylib\n");
        let mut runner = quiet_service(&mac, "zwo-camera").on("otool -L", move || Ok(both.clone()));
        let error = mac.verify(&mut runner, &request).unwrap_err();
        assert!(
            error.to_string().contains("binary links SDKs it must not"),
            "{error}"
        );

        let mut runner = quiet_service(&mac, "zwo-camera").on("otool -L", || Ok(String::new()));
        let error = mac.verify(&mut runner, &request).unwrap_err();
        assert_eq!(
            error.to_string(),
            "FAIL [zwo-camera]: binary does not load @rpath/libASICamera2.dylib"
        );

        fs::remove_file(keg.join("libASICamera2.dylib")).unwrap();
        let mut runner =
            quiet_service(&mac, "zwo-camera").on("otool -L", move || Ok(own.to_owned()));
        let error = mac.verify(&mut runner, &request).unwrap_err();
        assert_eq!(
            error.to_string(),
            "FAIL [zwo-camera]: bundled libASICamera2.dylib missing from the keg"
        );
    }

    #[test]
    fn sentinel_carries_a_working_doctor_and_its_renewal_plist() {
        let mac = Mac::new(&["sentinel"]);
        mac.install("rusty-photon-doctor");
        let keg = mac.path("prefix/opt/rusty-photon-sentinel-nightly");
        fs::create_dir_all(&keg).unwrap();
        fs::write(
            keg.join("rusty-photon-renew.plist"),
            "<string>rusty-photon-doctor</string>",
        )
        .unwrap();
        let config = mac.path("home/Library/Application Support/rusty-photon/sentinel.json");
        let request = Mac::request(Some(&["sentinel"]));
        let doctor = format!("{}", mac.path("prefix/bin/rusty-photon-doctor").display());
        let started = move || {
            fs::create_dir_all(config.parent().unwrap()).unwrap();
            fs::write(&config, "{}").unwrap();
            Ok(String::new())
        };

        let mut runner = quiet_service(&mac, "sentinel").on("brew services start", started.clone());
        mac.verify(&mut runner, &request).unwrap();
        let lines = mac.shorten(&runner.lines);
        for line in [
            "<mac>/prefix/bin/rusty-photon-doctor --json",
            "<mac>/prefix/bin/rusty-photon-doctor tls renew",
            "plutil -lint <mac>/prefix/opt/rusty-photon-sentinel-nightly/rusty-photon-renew.plist",
        ] {
            assert!(lines.contains(&line.to_owned()), "{line} not in {lines:?}");
        }

        let mut runner = quiet_service(&mac, "sentinel")
            .on("brew services start", started)
            .on(&format!("{doctor} tls renew"), || Err(()));
        let error = mac.verify(&mut runner, &request).unwrap_err();
        assert_eq!(
            error.to_string(),
            "FAIL [sentinel]: rusty-photon-doctor tls renew (nothing due) did not exit 0"
        );
    }

    #[test]
    fn phd2_guider_answering_503_serves() {
        let mac = Mac::new(&["phd2-guider"]);
        let request = Mac::request(Some(&["phd2-guider"]));
        let mut runner = quiet_service(&mac, "phd2-guider")
            .on("curl -s -o /dev/null -w", || Ok("503".to_owned()));
        mac.verify(&mut runner, &request).unwrap();

        let mut runner = quiet_service(&mac, "phd2-guider")
            .on("curl -s -o /dev/null -w", || Ok("404".to_owned()));
        let error = mac.verify(&mut runner, &request).unwrap_err();
        assert_eq!(
            error.to_string(),
            "FAIL [phd2-guider]: no HTTP response on port 11130 (/health; last code 404)"
        );
    }

    #[test]
    fn zwo_focuser_stays_alive_under_launchd_and_serves_in_the_foreground() {
        let mac = Mac::new(&["zwo-focuser"]);
        let keg = mac.path("prefix/opt/rusty-photon-zwo-focuser-nightly/lib");
        fs::create_dir_all(&keg).unwrap();
        fs::write(keg.join("libEAFFocuser.dylib"), "").unwrap();
        let request = Mac::request(Some(&["zwo-focuser"]));
        let script = |info: &'static str| {
            quiet_service(&mac, "zwo-focuser")
                .on("otool -L", || Ok("@rpath/libEAFFocuser.dylib\n".to_owned()))
                .on("brew services info", move || Ok(info.to_owned()))
        };

        let mut runner = script(r#"[{"running": true,"pid": 7}]"#);
        mac.verify(&mut runner, &request).unwrap();
        let lines = mac.shorten(&runner.lines);
        let stopped = lines
            .iter()
            .position(|line| line == "brew services stop rusty-photon-zwo-focuser-nightly")
            .unwrap();
        // Stopped under launchd, then run in the foreground and probed.
        assert_eq!(
            lines[stopped + 1],
            "<mac>/prefix/bin/rusty-photon-zwo-focuser"
        );
        assert_eq!(
            lines[stopped + 2],
            "curl -fsS -o /dev/null http://127.0.0.1:11124/management/apiversions"
        );
        assert!(lines.contains(&"sample 7 1".to_owned()), "{lines:?}");

        let mut runner = script(r#"[{"running": false}]"#);
        let error = mac.verify(&mut runner, &request).unwrap_err();
        assert!(
            error.to_string().contains("launchd process not running"),
            "{error}"
        );
    }

    #[test]
    fn keeps_everything_when_asked() {
        let mac = Mac::new(&["filemonitor"]);
        let mut runner = mac.brew().on("brew test", || Err(()));
        mac.install("rusty-photon-filemonitor");
        let request = Request {
            keep: true,
            ..Mac::request(None)
        };
        mac.verify(&mut runner, &request).unwrap_err();
        assert!(!runner
            .lines
            .iter()
            .any(|line| line.contains("--force") || line.contains("untap")));
    }

    #[test]
    fn refuses_a_dist_with_no_tarballs() {
        let mac = Mac::new(&[]);
        let error = mac
            .verify(&mut mac.brew(), &Mac::request(None))
            .unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("no rusty-photon-*-0.1.0-aarch64-apple-darwin.tar.gz in "),
            "{error}"
        );
    }
}
