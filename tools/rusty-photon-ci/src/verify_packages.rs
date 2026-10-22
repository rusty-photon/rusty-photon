//! `cargo ci verify-packages`: proves the built packages through their whole
//! lifecycle, before anything is published (docs/packaging.md).
//!
//! It installs them in a podman container that runs systemd: the .deb
//! packages in a Debian one, or with `--rpm` the .rpm packages in a Fedora
//! one.
//!
//! Per service: install, its unit active, the config it creates itself
//! under `/var/lib/rusty-photon/.config/rusty-photon`, an HTTP probe, then
//! removal, which keeps the config, and for a deb the purge, which takes
//! the config and the service's state. The shared user, its home and the
//! `/etc/rusty-photon` link stay. The kinds of service differ as they do on
//! macOS ([`crate::verify_brew`]):
//! - a [`GATED`] service waits for its config with `ConditionPathExists`:
//!   enabled, neither active nor failed, and no config;
//! - a [`SERIAL`] driver exits on its absent device and systemd restarts
//!   it: it is checked for its handshake attempt in the journal and its
//!   config, and held to serving only if it settles active;
//! - the cameras, zwo-focuser and phd2-guider create no config
//!   ([`NO_CONFIG_FILE`]), and phd2-guider's `/health` answers 503 with no
//!   PHD2 to talk to;
//! - svbony-camera's package does not ship the SDK it links (ADR-018): the
//!   run checks it is absent and installs it with the helper the package
//!   carries, as an operator would, before the ordinary checks;
//! - each zwo service, and svbony-camera, must load its own SDK through the
//!   RUNPATH, and no other (ADR-014);
//! - sentinel's package carries the polkit rule that lets it restart the
//!   services, and doctor with its renewal timer.
//!
//! The rpm flavor differs where rpm does. Its scriptlets enable a unit and
//! leave it stopped, which the run checks before it starts the unit. dnf
//! must resolve the packages' declared dependencies from Fedora's
//! repositories, with nothing installed beforehand to make up for them: the
//! proof they declare enough. Erasing a package keeps its config and state,
//! as rpm never purges.
//!
//! Rootless podman cannot set up the units' sandboxing across the `User=`
//! switch (217/USER, 226/NAMESPACE), so a drop-in resets it in the
//! container: the container proves the packaging, and real hosts the
//! hardening (docs/packaging.md).
//!
//! An interrupted run leaves its container, `rusty-photon-verify-<pid>`,
//! running: `podman rm -f` it.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::build_packages::RUNPATH;
use crate::packaging::{self, io, GATED, HEALTH_PROBED, NO_CONFIG_FILE, SERIAL};
use crate::process::{self, Cmd, Runner};

/// The packages a run verifies, and how.
#[derive(Clone, Debug, Default)]
pub struct Request {
    /// The packages; `dist/<workspace version>` when `None`.
    pub dist: Option<PathBuf>,
    /// Only these services. `None` verifies every service with a package in
    /// `dist`.
    pub services: Option<Vec<String>>,
    /// The .rpm packages, in a Fedora container, in place of the .deb
    /// packages in a Debian one.
    pub rpm: bool,
    /// Leave the container, to debug.
    pub keep: bool,
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
    #[error("FAIL: {0}")]
    Layout(String),
}

fn refused(reason: impl Into<String>) -> Error {
    Error::Refused(reason.into())
}

/// What a run waits with, so that its tests need not wait.
pub trait Clock {
    fn now(&self) -> Instant;

    fn sleep(&mut self, duration: Duration);
}

/// The machine's clock.
pub struct Wall;

impl Clock for Wall {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// The service the package only ships the helper for: the operator installs
/// its SDK (ADR-018).
const SVBONY: &str = "svbony-camera";

/// The helper svbony-camera's package ships, in both flavors.
const SVBONY_SDK_INSTALL: &str = "/usr/sbin/rusty-photon-svbony-sdk-install";

/// The drop-in that resets a unit's hardening in the container.
const UNHARDEN: &str = "\
[Service]
NoNewPrivileges=no
ProtectSystem=off
ReadWritePaths=
ProtectHome=no
PrivateTmp=no
ProtectKernelTunables=no
ProtectKernelModules=no
ProtectControlGroups=no
RestrictSUIDSGID=no
LockPersonality=no
RestrictRealtime=no
RestrictAddressFamilies=
PrivateDevices=no
MemoryDenyWriteExecute=no
UMask=0022
";

/// The Debian image. Its stock `policy-rc.d` exits 101, which stops every
/// maintainer script from starting a unit, the very thing the run checks.
const DEBIAN: &str = "\
FROM debian:trixie
RUN apt-get update && apt-get install -y --no-install-recommends \\
    systemd systemd-sysv udev ca-certificates curl \\
    && rm -f /usr/sbin/policy-rc.d
CMD [\"/sbin/init\"]
";

/// The Fedora image, with what the lifecycle needs and none of the services'
/// runtime libraries, which dnf must find from the packages' declarations:
/// systemd as init, udevadm for the camera scriptlets, shadow-utils for the
/// shared user, curl for the probes and pgrep to check an erased service
/// stopped.
const FEDORA: &str = "\
FROM registry.fedoraproject.org/fedora:44
RUN dnf -y install systemd systemd-udev shadow-utils procps-ng curl && dnf clean all
CMD [\"/sbin/init\"]
";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Flavor {
    Deb,
    Rpm,
}

impl Flavor {
    const fn name(self) -> &'static str {
        match self {
            Self::Deb => "deb",
            Self::Rpm => "rpm",
        }
    }

    const fn containerfile(self) -> &'static str {
        match self {
            Self::Deb => DEBIAN,
            Self::Rpm => FEDORA,
        }
    }

    /// The service a package file of this flavor is for. A deb is
    /// `rusty-photon-<svc>_<version>_<arch>.deb`; an rpm is
    /// `rusty-photon-<svc>-<version>-<release>.<arch>.rpm`, where the
    /// version is the first word that starts with a digit, as no service's
    /// name has one.
    fn service_of(self, file: &str) -> Option<String> {
        let rest = file.strip_prefix("rusty-photon-")?;
        let service = match self {
            Self::Deb => rest.strip_suffix(".deb")?.split_once('_')?.0.to_owned(),
            Self::Rpm => {
                let words: Vec<&str> = rest.strip_suffix(".rpm")?.split('-').collect();
                let version = words
                    .iter()
                    .position(|word| word.starts_with(|c: char| c.is_ascii_digit()))?;
                words.get(..version)?.join("-")
            }
        };
        (!service.is_empty()).then_some(service)
    }

    /// The files `service_of` takes for `service`, as a glob, for a message.
    fn pattern(self, service: &str) -> String {
        match self {
            Self::Deb => format!("rusty-photon-{service}_*.deb"),
            Self::Rpm => format!("rusty-photon-{service}-*.rpm"),
        }
    }
}

fn unit(service: &str) -> String {
    format!("rusty-photon-{service}")
}

fn binary(service: &str) -> String {
    format!("/usr/bin/rusty-photon-{service}")
}

fn config(service: &str) -> String {
    format!("/var/lib/rusty-photon/.config/rusty-photon/{service}.json")
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

/// Where svbony-camera's helper installs its SDK.
fn svbony_sdk() -> String {
    format!("{RUNPATH}/libSVBCameraSDK.so")
}

/// How a serial driver's unit settled.
enum Settled {
    /// It answers its port.
    Serving,
    /// It left `active`: it failed on its absent device and systemd restarts
    /// it.
    Retrying,
    /// It stayed active and never answered.
    Silent,
    /// systemd could not say.
    Unreadable,
}

/// A run in progress, in its container.
struct Run<'a, R: Runner, C: Clock> {
    runner: &'a mut R,
    clock: &'a mut C,
    container: String,
    flavor: Flavor,
}

impl<R: Runner, C: Clock> Run<'_, R, C> {
    /// `args` run in the container.
    fn exec(&self, args: &[&str]) -> Cmd {
        let mut all = vec!["exec", self.container.as_str()];
        all.extend_from_slice(args);
        Cmd::new("podman", &all)
    }

    /// An apt-get in the container that asks nothing.
    fn apt_get(&self, args: &[&str]) -> Cmd {
        let mut all = vec![
            "exec",
            "-e",
            "DEBIAN_FRONTEND=noninteractive",
            self.container.as_str(),
            "apt-get",
        ];
        all.extend_from_slice(args);
        Cmd::new("podman", &all)
    }

    /// Whether `args` exits 0 in the container.
    fn ok(&mut self, args: &[&str]) -> bool {
        let cmd = self.exec(args);
        self.runner.code(&cmd).is_ok_and(|code| code == Some(0))
    }

    /// What `args` prints in the container, or nothing when it fails.
    fn read(&mut self, args: &[&str]) -> String {
        let cmd = self.exec(args);
        self.runner.read(&cmd).unwrap_or_default()
    }

    /// Whether `cmd` succeeds, its output but its errors discarded.
    fn quietly(&mut self, cmd: &Cmd) -> bool {
        self.runner.read(cmd).is_ok()
    }

    /// Checks `ready` up to `attempts` times, a second apart.
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

    /// The service's failure, with the end of its journal on stderr.
    fn fail(&mut self, service: &str, reason: &str) -> Error {
        eprintln!("verify-packages: FAIL [{service}]: {reason}");
        let journal = self.read(&["journalctl", "-u", &unit(service), "--no-pager", "-n", "40"]);
        eprintln!("{journal}");
        Error::Failed {
            service: service.to_owned(),
            reason: reason.to_owned(),
        }
    }

    /// Fails `service` unless `ok`.
    fn require(&mut self, ok: bool, service: &str, reason: &str) -> Result<(), Error> {
        if ok {
            Ok(())
        } else {
            Err(self.fail(service, reason))
        }
    }

    fn install(&mut self, service: &str, file: &str) -> Result<(), Error> {
        println!("== {service}: install");
        let path = format!("/dist/{file}");
        let (cmd, tool) = match self.flavor {
            Flavor::Deb => (self.apt_get(&["install", "-y", &path]), "apt-get"),
            // dnf resolves the package's declared dependencies from Fedora's
            // repositories: a failure here is a package that declares too
            // few.
            Flavor::Rpm => (self.exec(&["dnf", "install", "-y", &path]), "dnf"),
        };
        let installed = self.quietly(&cmd);
        self.require(installed, service, &format!("{tool} install failed"))
    }

    fn check(&mut self, service: &str) -> Result<(), Error> {
        let unit = unit(service);
        // The shared layout, which the first package makes and the rest
        // keep.
        let linked = self.ok(&["test", "-L", "/etc/rusty-photon"]);
        self.require(linked, service, "/etc/rusty-photon symlink missing")?;
        let user = self.ok(&["getent", "passwd", "rusty-photon"]);
        self.require(user, service, "rusty-photon user missing")?;

        if self.flavor == Flavor::Rpm {
            // rpm's scriptlets enable the unit and leave it stopped, as
            // Fedora does, where a deb's postinst starts it. From the start
            // here on, the flavors expect the same of each kind of service:
            // a gated unit starts with its condition unmet and a serial
            // driver dies on its absent device, which is no failure yet.
            let enabled = self.ok(&["systemctl", "is-enabled", "--quiet", &unit]);
            self.require(enabled, service, "unit not enabled by the rpm scriptlet")?;
            let active = self.ok(&["systemctl", "is-active", "--quiet", &unit]);
            self.require(
                !active,
                service,
                "unit auto-started on rpm install (scriptlets enable only)",
            )?;
            let _ = self.ok(&["systemctl", "start", &unit]);
        }

        if GATED.contains(&service) {
            return self.gated(service);
        }
        if service == SVBONY {
            self.operator_sdk(service)?;
        }
        if SERIAL.contains(&service) {
            return self.serial(service);
        }
        self.serves(service)
    }

    /// No config, so the unit waits on `ConditionPathExists`: enabled, but
    /// neither active nor failed.
    fn gated(&mut self, service: &str) -> Result<(), Error> {
        let unit = unit(service);
        let enabled = self.ok(&["systemctl", "is-enabled", "--quiet", &unit]);
        self.require(enabled, service, "unit not enabled")?;
        let active = self.ok(&["systemctl", "is-active", "--quiet", &unit]);
        self.require(!active, service, "gated unit is active without a config")?;
        let failed = self.ok(&["systemctl", "is-failed", "--quiet", &unit]);
        self.require(
            !failed,
            service,
            "gated unit failed instead of waiting on ConditionPathExists",
        )?;
        let absent = self.ok(&["test", "!", "-e", &config(service)]);
        self.require(
            absent,
            service,
            "config exists on a fresh install of a gated service",
        )?;
        println!("== {service}: OK (gated on config)");
        Ok(())
    }

    /// The package must withhold the SDK, which would make the checks after
    /// this vacuous; then the run installs it as the postinst tells an
    /// operator to, which also proves the helper's pinned blob still
    /// downloads. It does not start the unit: until the SDK is there the
    /// binary cannot load and systemd restarts it, so the ordinary checks
    /// after this prove a host heals once its operator installs the SDK.
    fn operator_sdk(&mut self, service: &str) -> Result<(), Error> {
        let sdk = svbony_sdk();
        let enabled = self.ok(&["systemctl", "is-enabled", "--quiet", &unit(service)]);
        self.require(enabled, service, "unit not enabled")?;
        let withheld = self.ok(&["test", "!", "-e", &sdk]);
        self.require(
            withheld,
            service,
            &format!("package ships {sdk} — ADR-018 forbids bundling it"),
        )?;
        println!("== {service}: installing the operator-supplied SDK ({SVBONY_SDK_INSTALL})");
        let helper = self.exec(&[SVBONY_SDK_INSTALL]);
        let ran = self.quietly(&helper);
        self.require(ran, service, &format!("{SVBONY_SDK_INSTALL} failed"))?;
        let installed = self.ok(&["test", "-s", &sdk]);
        self.require(installed, service, &format!("helper did not install {sdk}"))
    }

    /// Waits for the service to write its config.
    fn created(&mut self, service: &str) -> Result<(), Error> {
        let config = config(service);
        let created = self.wait_until(15, |run| run.ok(&["test", "-s", &config]));
        self.require(
            created,
            service,
            &format!("config not self-created at {config}"),
        )
    }

    /// A serial driver starts, creates its config and fails on its absent
    /// device, and not before: a loader, user or config problem would end
    /// it before the handshake.
    fn serial(&mut self, service: &str) -> Result<(), Error> {
        let grep = format!(
            "journalctl -u {} --no-pager 2>/dev/null | grep -q 'eager startup handshake'",
            unit(service)
        );
        let tried = self.wait_until(30, |run| run.ok(&["sh", "-c", &grep]));
        self.require(tried, service, "no eager-handshake attempt in the journal")?;
        self.created(service)?;
        let port = packaging::port(service).unwrap_or_default();
        let path = probe_path(service);
        match self.settle(service, &format!("http://127.0.0.1:{port}{path}")) {
            Settled::Serving => println!("== {service}: OK (active, config, port {port})"),
            Settled::Retrying => {
                println!(
                    "== {service}: OK (config self-created; retrying on absent serial device)"
                );
            }
            Settled::Silent => {
                return Err(self.fail(
                    service,
                    &format!("active for 15s but no HTTP response on port {port} ({path})"),
                ));
            }
            // Taken as not active, it would pass the driver as retrying
            // without a check.
            Settled::Unreadable => {
                return Err(self.fail(service, "could not read the unit's ActiveState"));
            }
        }
        Ok(())
    }

    /// How a serial driver's unit settles. A `Type=simple` unit reads
    /// `active` from the moment its process forks, and a driver without its
    /// device lives on a moment past its handshake while the shared opener
    /// retries the port, so one sample would take a doomed process for one
    /// that serves, and probe a port it never bound. So this watches until
    /// an outcome holds: a driver that serves answers its port, and one that
    /// cannot leaves `active` (`activating`, while systemd waits to restart
    /// it, counts). The deadline is the clock's, not a count of probes, as a
    /// probe can take seconds.
    fn settle(&mut self, service: &str, url: &str) -> Settled {
        let unit = unit(service);
        let Some(deadline) = self.clock.now().checked_add(Duration::from_secs(15)) else {
            return Settled::Silent;
        };
        loop {
            let state = self.read(&["systemctl", "show", "-p", "ActiveState", "--value", &unit]);
            match state.trim_end() {
                "" => return Settled::Unreadable,
                "active" => {}
                _ => return Settled::Retrying,
            }
            if self.ok(&["curl", "-fsS", "--max-time", "5", "-o", "/dev/null", url]) {
                return Settled::Serving;
            }
            self.clock.sleep(Duration::from_secs(1));
            // After the sleep: the deadline bounds when a probe may start.
            if self.clock.now() >= deadline {
                return Settled::Silent;
            }
        }
    }

    /// Active, its config written where it writes one, and answering its
    /// port.
    fn serves(&mut self, service: &str) -> Result<(), Error> {
        let unit = unit(service);
        let active = self.wait_until(30, |run| {
            run.ok(&["systemctl", "is-active", "--quiet", &unit])
        });
        self.require(active, service, "unit not active after 30s")?;
        if self_creates_config(service) {
            self.created(service)?;
        }
        let port = packaging::port(service).unwrap_or_default();
        let path = probe_path(service);
        let url = format!("http://127.0.0.1:{port}{path}");
        // `--max-time`: a service that binds its port and never answers is
        // what the probe is for.
        if service == "phd2-guider" {
            // No PHD2 in the container, so its `/health` answers 503, which
            // still shows the listener serves.
            let mut last = String::new();
            let answered = self.wait_until(30, |run| {
                let code = run.read(&[
                    "curl",
                    "-s",
                    "--max-time",
                    "5",
                    "-o",
                    "/dev/null",
                    "-w",
                    "%{http_code}",
                    &url,
                ]);
                last = if code.is_empty() {
                    "000".to_owned()
                } else {
                    code
                };
                last == "200" || last == "503"
            });
            self.require(
                answered,
                service,
                &format!("no HTTP response on port {port} ({path}; last code {last})"),
            )?;
        } else {
            let answered = self.wait_until(30, |run| {
                run.ok(&["curl", "-fsS", "--max-time", "5", "-o", "/dev/null", &url])
            });
            self.require(
                answered,
                service,
                &format!("no HTTP response on port {port} ({path})"),
            )?;
        }
        match service {
            "sentinel" => self.sentinel(service)?,
            "zwo-camera" => {
                self.loads(service, "libASICamera2", &["libEFWFilter", "libEAFFocuser"])?;
            }
            "zwo-focuser" => {
                self.loads(service, "libEAFFocuser", &["libASICamera2", "libEFWFilter"])?;
            }
            SVBONY => self.loads(service, "libSVBCameraSDK", &[])?,
            _ => {}
        }
        println!("== {service}: OK (active, config, port {port})");
        Ok(())
    }

    /// The SDK the service links loads from the RUNPATH, and no other ZWO
    /// SDK does (ADR-014). The zwo packages ship theirs; svbony-camera's
    /// the operator installed.
    fn loads(&mut self, service: &str, own: &str, others: &[&str]) -> Result<(), Error> {
        let ldd = self.read(&["ldd", &binary(service)]);
        self.require(
            ldd.contains(&format!("{RUNPATH}/{own}.so")),
            service,
            &format!("{own}.so does not resolve to {RUNPATH} (RUNPATH)"),
        )?;
        self.require(
            !others.iter().any(|other| ldd.contains(other)),
            service,
            &format!(
                "binary links SDKs it must not ({}; ADR-014 per-device link)",
                others.join(", ")
            ),
        )
    }

    /// The polkit rule that lets sentinel restart the services, where
    /// polkitd reads a vendor's rules (the grant itself is proven on a real
    /// host: the container has no polkitd); and doctor, which rides in
    /// sentinel's package, with its renewal timer.
    fn sentinel(&mut self, service: &str) -> Result<(), Error> {
        let rule = self.ok(&[
            "test",
            "-f",
            "/usr/share/polkit-1/rules.d/50-rusty-photon-sentinel.rules",
        ]);
        self.require(
            rule,
            service,
            "polkit rule not installed at /usr/share/polkit-1/rules.d",
        )?;
        let doctor = self.ok(&["test", "-x", "/usr/bin/rusty-photon-doctor"]);
        self.require(
            doctor,
            service,
            "rusty-photon-doctor not installed at /usr/bin",
        )?;
        // 1 is a finding, which a container may well have; more is a broken
        // tool.
        let diagnose = self.exec(&["rusty-photon-doctor", "--json"]);
        let code = self.runner.code(&diagnose)?;
        self.require(
            code.is_some_and(|code| code <= 1),
            service,
            &format!(
                "rusty-photon-doctor --json exited {}",
                code.map_or_else(|| "on a signal".to_owned(), |code| code.to_string())
            ),
        )?;
        // What the daily timer runs: with nothing due, it exits 0.
        let renews = self.ok(&["rusty-photon-doctor", "tls", "renew"]);
        self.require(
            renews,
            service,
            "rusty-photon-doctor tls renew (nothing due) did not exit 0",
        )?;
        let timer = "rusty-photon-renew.timer";
        let enabled = self.ok(&["systemctl", "is-enabled", "--quiet", timer]);
        self.require(enabled, service, "rusty-photon-renew.timer not enabled")?;
        // The deb's postinst starts the timer; rpm's scriptlet enables it,
        // and it arms at the next boot.
        if self.flavor == Flavor::Deb {
            let active = self.ok(&["systemctl", "is-active", "--quiet", timer]);
            self.require(
                active,
                service,
                "rusty-photon-renew.timer not active (postinst starts it)",
            )?;
        }
        Ok(())
    }

    /// A deb's removal keeps the config and its purge takes it and the
    /// state. An rpm's erase is a removal: the binary and unit go, the
    /// process stops, and the config and state stay, whose deletion
    /// docs/packaging.md leaves to the operator.
    fn remove(&mut self, service: &str) -> Result<(), Error> {
        let unit = unit(service);
        let config = config(service);
        let state = format!("/var/lib/rusty-photon/{service}");
        let binary = binary(service);
        if self.flavor == Flavor::Rpm {
            println!("== {service}: erase");
            // Not every service makes a state directory.
            let had_state = self.ok(&["test", "-d", &state]);
            let erase = self.exec(&["dnf", "remove", "-y", &unit]);
            let erased = self.quietly(&erase);
            self.require(erased, service, "dnf remove failed")?;
            let gone = self.ok(&["test", "!", "-e", &binary]);
            self.require(gone, service, "binary survived erase")?;
            let unit_file = format!("/usr/lib/systemd/system/{unit}.service");
            let gone = self.ok(&["test", "!", "-e", &unit_file]);
            self.require(gone, service, "unit file survived erase")?;
            let running = self.ok(&["pgrep", "-f", &binary]);
            self.require(
                !running,
                service,
                "process still running after erase (pre_uninstall must stop it)",
            )?;
            if self_creates_config(service) {
                let kept = self.ok(&["test", "-f", &config]);
                self.require(
                    kept,
                    service,
                    "config did not survive erase (rpm never purges)",
                )?;
            }
            if had_state {
                let kept = self.ok(&["test", "-d", &state]);
                self.require(
                    kept,
                    service,
                    "state dir did not survive erase (rpm never purges)",
                )?;
            }
        } else {
            println!("== {service}: remove + purge");
            let remove = self.apt_get(&["remove", "-y", &unit]);
            let removed = self.quietly(&remove);
            self.require(removed, service, "apt-get remove failed")?;
            if self_creates_config(service) {
                let kept = self.ok(&["test", "-f", &config]);
                self.require(
                    kept,
                    service,
                    "config did not survive remove (must only go on purge)",
                )?;
            }
            let purge = self.exec(&["dpkg", "--purge", &unit]);
            let purged = self.quietly(&purge);
            self.require(purged, service, "dpkg --purge failed")?;
            let gone = self.ok(&["test", "!", "-e", &config]);
            self.require(gone, service, "config survived purge")?;
            let gone = self.ok(&["test", "!", "-e", &state]);
            self.require(gone, service, "state dir survived purge")?;
        }
        Ok(())
    }

    /// The shared pieces stay: no package's scripts remove them, as Debian
    /// does with a system user.
    fn layout(&mut self) -> Result<(), Error> {
        let kept = [
            (
                &["getent", "passwd", "rusty-photon"][..],
                "shared user removed by uninstall",
            ),
            (
                &["test", "-d", "/var/lib/rusty-photon"],
                "shared home removed by uninstall",
            ),
            (
                &["test", "-L", "/etc/rusty-photon"],
                "/etc/rusty-photon symlink removed by uninstall",
            ),
        ];
        for (args, problem) in kept {
            if !self.ok(args) {
                return Err(Error::Layout(problem.to_owned()));
            }
        }
        Ok(())
    }
}

/// The services to verify, those asked for or every one with a package of
/// `flavor` in `dist`, and each one's package file.
fn packages_in(
    dist: &Path,
    flavor: Flavor,
    asked: Option<&[String]>,
) -> Result<(Vec<String>, Vec<String>), Error> {
    let mut files: Vec<String> = fs::read_dir(dist)
        .map_err(io(dist))?
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    files.sort();
    let services = asked.map_or_else(
        || {
            let mut services: Vec<String> = files
                .iter()
                .filter_map(|file| flavor.service_of(file))
                .collect();
            services.sort();
            services.dedup();
            services
        },
        <[String]>::to_vec,
    );
    if services.is_empty() {
        return Err(refused(format!(
            "no rusty-photon-*.{} packages in {}",
            flavor.name(),
            dist.display()
        )));
    }
    // One package a service: two, an older build left beside a newer, would
    // leave which one is verified to chance.
    let mut packages = Vec::new();
    for service in &services {
        let matching: Vec<&String> = files
            .iter()
            .filter(|file| flavor.service_of(file).as_ref() == Some(service))
            .collect();
        match matching.as_slice() {
            [file] => packages.push((*file).clone()),
            [] => {
                return Err(refused(format!(
                    "no {} in {}",
                    flavor.pattern(service),
                    dist.display()
                )));
            }
            _ => {
                return Err(refused(format!(
                    "multiple {} in {} — remove stale builds first",
                    flavor.pattern(service),
                    dist.display()
                )));
            }
        }
    }
    Ok((services, packages))
}

/// Verifies the packages `request` names and returns how many services
/// passed.
///
/// # Errors
///
/// When a service fails a check, or the run cannot be set up: no podman, no
/// packages, two packages for a service, a service without a port, or a
/// container that does not come up.
pub fn verify(
    runner: &mut impl Runner,
    clock: &mut impl Clock,
    root: &Path,
    request: &Request,
) -> Result<usize, Error> {
    let flavor = if request.rpm {
        Flavor::Rpm
    } else {
        Flavor::Deb
    };
    let podman = runner
        .code(&Cmd::new("podman", &["--version"]))
        .is_ok_and(|code| code == Some(0));
    if !podman {
        return Err(refused("podman not found"));
    }
    let dist = match &request.dist {
        Some(dist) => dist.clone(),
        None => root.join("dist").join(packaging::workspace_version(root)?),
    };
    if !dist.is_dir() {
        return Err(refused(format!(
            "{} not found — run `cargo ci build-packages` first",
            dist.display()
        )));
    }
    let dist = fs::canonicalize(&dist).map_err(io(&dist))?;
    let (services, packages) = packages_in(&dist, flavor, request.services.as_deref())?;
    println!("Verifying ({}): {}", flavor.name(), services.join(" "));
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

    let work = tempfile::tempdir().map_err(io(&std::env::temp_dir()))?;
    let containerfile = work.path().join("Containerfile");
    fs::write(&containerfile, flavor.containerfile()).map_err(io(&containerfile))?;
    let image = format!("localhost/rusty-photon-pkg-verify-{}", flavor.name());
    println!("Building the verification image ({image})...");
    runner.read(&Cmd::new(
        "podman",
        &[
            "build",
            "-q",
            "-t",
            &image,
            "-f",
            &containerfile.to_string_lossy(),
            &work.path().to_string_lossy(),
        ],
    ))?;
    let container = format!("rusty-photon-verify-{}", std::process::id());
    let volume = format!("{}:/dist:ro,Z", dist.display());
    let started = runner.read(&Cmd::new(
        "podman",
        &[
            "run",
            "-d",
            "--name",
            &container,
            "--systemd=always",
            "-v",
            &volume,
            &image,
        ],
    ));
    let mut run = Run {
        runner,
        clock,
        container: container.clone(),
        flavor,
    };
    let result = match started {
        Ok(_) => verify_in(&mut run, &services, &packages, work.path()),
        Err(error) => Err(error.into()),
    };
    if request.keep {
        println!(
            "Container kept: podman exec -it {container} bash (remove: podman rm -f {container})"
        );
    } else {
        let _ = run
            .runner
            .code(&Cmd::new("podman", &["rm", "-f", &container]));
    }
    result.map(|()| services.len())
}

fn verify_in<R: Runner, C: Clock>(
    run: &mut Run<'_, R, C>,
    services: &[String],
    packages: &[String],
    work: &Path,
) -> Result<(), Error> {
    println!("Waiting for systemd in the container...");
    let mut state = String::new();
    let up = run.wait_until(60, |run| {
        // Quiet: until systemd is up, systemctl cannot reach its bus.
        let read = run.read(&[
            "sh",
            "-c",
            "systemctl show -p SystemState --value 2>/dev/null",
        ]);
        read.trim_end().clone_into(&mut state);
        state == "running" || state == "degraded"
    });
    if !up {
        return Err(refused(format!(
            "systemd did not come up in the container (state: {state})"
        )));
    }

    // Before the packages install, as a deb's postinst starts its unit.
    let unharden = work.join("99-container-verify.conf");
    fs::write(&unharden, UNHARDEN).map_err(io(&unharden))?;
    for service in services {
        let dir = format!("/etc/systemd/system/{}.service.d", unit(service));
        let mkdir = run.exec(&["mkdir", "-p", &dir]);
        run.runner.run(&mkdir)?;
        let into = format!("{}:{dir}/99-container-verify.conf", run.container);
        run.runner.run(&Cmd::new(
            "podman",
            &["cp", &unharden.to_string_lossy(), &into],
        ))?;
    }

    if run.flavor == Flavor::Deb {
        println!("Refreshing apt package lists in the container...");
        let update = run.apt_get(&["update", "-qq"]);
        run.runner.run(&update)?;
        // A deb built off Debian declares none of its libraries:
        // dpkg-shlibdeps needs Debian's shlibs database, so `$auto` comes out
        // empty and Depends holds only what the package names itself. Its
        // runtime libraries are installed beforehand. One built on Debian, as
        // the nightly's are, declares them, libc6 among them, which every
        // binary needs, and apt must resolve them. cargo-generate-rpm works
        // an rpm's out on any host.
        let undeclared = packages.iter().any(|file| {
            !run.read(&["dpkg-deb", "-f", &format!("/dist/{file}"), "Depends"])
                .contains("libc6")
        });
        if undeclared {
            eprintln!(
                "verify-packages: WARNING: deb(s) whose Depends names no library \
                 (non-Debian-host build); preinstalling runtime libs"
            );
            let libraries = run.apt_get(&[
                "install",
                "-y",
                "--no-install-recommends",
                "libusb-1.0-0",
                "libudev1",
                "libstdc++6",
            ]);
            run.runner.read(&libraries)?;
        }
    }

    for (service, file) in services.iter().zip(packages) {
        run.install(service, file)?;
        run.check(service)?;
    }
    for service in services {
        run.remove(service)?;
    }
    run.layout()?;
    println!(
        "\nverify-packages: OK ({} {} packages)",
        services.len(),
        run.flavor.name()
    );
    Ok(())
}

// The tools run on Linux; their tests compare Unix paths.
#[cfg(test)]
#[cfg(unix)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use super::*;
    use crate::process::Script;

    /// A clock that moves only when the run sleeps.
    struct Stopped {
        start: Instant,
        slept: Duration,
    }

    impl Clock for Stopped {
        fn now(&self) -> Instant {
            self.start.checked_add(self.slept).unwrap()
        }

        fn sleep(&mut self, duration: Duration) {
            self.slept = self.slept.saturating_add(duration);
        }
    }

    fn container_name() -> String {
        format!("rusty-photon-verify-{}", std::process::id())
    }

    /// `cmd` run in the container.
    fn c(cmd: &str) -> String {
        format!("podman exec {} {cmd}", container_name())
    }

    /// An apt-get run in the container.
    fn apt(args: &str) -> String {
        format!(
            "podman exec -e DEBIAN_FRONTEND=noninteractive {} apt-get {args}",
            container_name()
        )
    }

    /// A repository whose dist holds a deb and an rpm for each of `services`.
    struct Linux {
        dir: tempfile::TempDir,
    }

    impl Linux {
        fn new(services: &[&str]) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let linux = Self { dir };
            fs::write(
                linux.root().join("Cargo.toml"),
                "[workspace]\nmembers = []\n\n[workspace.package]\nversion = \"0.1.0\"\n",
            )
            .unwrap();
            fs::create_dir_all(linux.dist()).unwrap();
            for service in services {
                linux.add(&format!("rusty-photon-{service}_0.1.0-1_amd64.deb"));
                linux.add(&format!("rusty-photon-{service}-0.1.0-1.x86_64.rpm"));
            }
            linux.add("SHA256SUMS.txt");
            linux
        }

        fn root(&self) -> &Path {
            self.dir.path()
        }

        fn dist(&self) -> PathBuf {
            self.root().join("dist/0.1.0")
        }

        fn add(&self, file: &str) {
            fs::write(self.dist().join(file), "").unwrap();
        }

        fn verify(&self, runner: &mut Script, request: &Request) -> Result<usize, Error> {
            let mut clock = Stopped {
                start: Instant::now(),
                slept: Duration::ZERO,
            };
            verify(runner, &mut clock, self.root(), request)
        }

        /// The commands, with the container's name shortened to `<c>`, the
        /// dist to `<dist>` and the run's scratch directory to `<work>`.
        fn shorten(&self, lines: &[String]) -> Vec<String> {
            let dist = fs::canonicalize(self.dist()).unwrap().display().to_string();
            let work = lines
                .iter()
                .find(|line| line.starts_with("podman build"))
                .and_then(|line| line.rsplit(' ').next())
                .unwrap_or("<none>")
                .to_owned();
            lines
                .iter()
                .map(|line| {
                    line.replace(&work, "<work>")
                        .replace(&dist, "<dist>")
                        .replace(&container_name(), "<c>")
                })
                .collect()
        }
    }

    fn request(services: Option<&[&str]>, rpm: bool) -> Request {
        Request {
            services: services.map(|services| services.iter().map(|&s| s.to_owned()).collect()),
            rpm,
            ..Request::default()
        }
    }

    /// A container whose systemd is up and whose debs declare their
    /// dependencies, where every check passes but those a fresh install
    /// fails: no unit has failed and no process outlives its erase.
    fn container_script() -> Script {
        container(Script::default())
    }

    /// [`container_script`] under `script`'s rules, which answer first.
    fn container(script: Script) -> Script {
        script
            .on(&c("sh -c systemctl show -p SystemState"), || {
                Ok("running\n".to_owned())
            })
            .on(&c("dpkg-deb -f"), || {
                Ok("adduser, libc6 (>= 2.34), libudev1 (>= 183)\n".to_owned())
            })
            .on(&c("systemctl is-failed"), || Err(()))
            .on(&c("pgrep"), || Err(()))
    }

    /// As [`container_script`], with rpm's scriptlets: each unit inactive
    /// until the run starts it.
    fn rpm_script() -> Script {
        rpm(Script::default())
    }

    /// [`rpm_script`] under `script`'s rules, which answer first.
    fn rpm(script: Script) -> Script {
        let started = Rc::new(Cell::new(false));
        let start = Rc::clone(&started);
        container(script)
            .on(&c("systemctl start"), move || {
                start.set(true);
                Ok(String::new())
            })
            .on(&c("systemctl is-active --quiet rusty-photon-"), move || {
                if started.get() {
                    Ok(String::new())
                } else {
                    Err(())
                }
            })
    }

    #[test]
    fn finds_the_service_in_each_flavors_file_name() {
        let deb = Flavor::Deb;
        let rpm = Flavor::Rpm;
        assert_eq!(
            deb.service_of("rusty-photon-ppba-driver_0.1.0+nightly.1.gabc_arm64.deb")
                .as_deref(),
            Some("ppba-driver")
        );
        assert_eq!(
            rpm.service_of("rusty-photon-ppba-driver-0.1.0^1.gabc-1.aarch64.rpm")
                .as_deref(),
            Some("ppba-driver")
        );
        assert_eq!(
            deb.service_of("rusty-photon-ppba-driver-0.1.0-1.x86_64.rpm"),
            None
        );
        assert_eq!(rpm.service_of("rusty-photon-0.1.0-1.x86_64.rpm"), None);
        assert_eq!(deb.service_of("SHA256SUMS.txt"), None);
    }

    #[test]
    fn verifies_a_deb_through_install_probe_remove_and_purge() {
        let linux = Linux::new(&["filemonitor"]);
        let mut runner = container_script();
        let count = linux.verify(&mut runner, &request(None, false)).unwrap();
        assert_eq!(count, 1);
        let deb = "/dist/rusty-photon-filemonitor_0.1.0-1_amd64.deb";
        let config = "/var/lib/rusty-photon/.config/rusty-photon/filemonitor.json";
        let dropins = "/etc/systemd/system/rusty-photon-filemonitor.service.d";
        assert_eq!(
            linux.shorten(&runner.lines),
            [
                "podman --version".to_owned(),
                "podman build -q -t localhost/rusty-photon-pkg-verify-deb -f <work>/Containerfile <work>".to_owned(),
                "podman run -d --name <c> --systemd=always -v <dist>:/dist:ro,Z localhost/rusty-photon-pkg-verify-deb".to_owned(),
                "podman exec <c> sh -c systemctl show -p SystemState --value 2>/dev/null".to_owned(),
                format!("podman exec <c> mkdir -p {dropins}"),
                format!("podman cp <work>/99-container-verify.conf <c>:{dropins}/99-container-verify.conf"),
                "podman exec -e DEBIAN_FRONTEND=noninteractive <c> apt-get update -qq".to_owned(),
                format!("podman exec <c> dpkg-deb -f {deb} Depends"),
                format!("podman exec -e DEBIAN_FRONTEND=noninteractive <c> apt-get install -y {deb}"),
                "podman exec <c> test -L /etc/rusty-photon".to_owned(),
                "podman exec <c> getent passwd rusty-photon".to_owned(),
                "podman exec <c> systemctl is-active --quiet rusty-photon-filemonitor".to_owned(),
                format!("podman exec <c> test -s {config}"),
                "podman exec <c> curl -fsS --max-time 5 -o /dev/null http://127.0.0.1:11111/management/apiversions".to_owned(),
                "podman exec -e DEBIAN_FRONTEND=noninteractive <c> apt-get remove -y rusty-photon-filemonitor".to_owned(),
                format!("podman exec <c> test -f {config}"),
                "podman exec <c> dpkg --purge rusty-photon-filemonitor".to_owned(),
                format!("podman exec <c> test ! -e {config}"),
                "podman exec <c> test ! -e /var/lib/rusty-photon/filemonitor".to_owned(),
                "podman exec <c> getent passwd rusty-photon".to_owned(),
                "podman exec <c> test -d /var/lib/rusty-photon".to_owned(),
                "podman exec <c> test -L /etc/rusty-photon".to_owned(),
                "podman rm -f <c>".to_owned(),
            ]
        );
    }

    #[test]
    fn verifies_an_rpm_enabled_not_started_then_erased_keeping_its_config() {
        let linux = Linux::new(&["filemonitor"]);
        let mut runner = rpm_script();
        linux.verify(&mut runner, &request(None, true)).unwrap();
        let lines = linux.shorten(&runner.lines);
        let rpm = "/dist/rusty-photon-filemonitor-0.1.0-1.x86_64.rpm";
        let from = |first: &str| -> Vec<String> {
            lines
                .iter()
                .skip_while(|line| !line.contains(first))
                .cloned()
                .collect()
        };
        assert_eq!(
            from("dnf install")[..6],
            [
                format!("podman exec <c> dnf install -y {rpm}"),
                "podman exec <c> test -L /etc/rusty-photon".to_owned(),
                "podman exec <c> getent passwd rusty-photon".to_owned(),
                "podman exec <c> systemctl is-enabled --quiet rusty-photon-filemonitor".to_owned(),
                "podman exec <c> systemctl is-active --quiet rusty-photon-filemonitor".to_owned(),
                "podman exec <c> systemctl start rusty-photon-filemonitor".to_owned(),
            ]
        );
        assert_eq!(
            from("test -d /var/lib/rusty-photon/filemonitor")[..7],
            [
                "podman exec <c> test -d /var/lib/rusty-photon/filemonitor",
                "podman exec <c> dnf remove -y rusty-photon-filemonitor",
                "podman exec <c> test ! -e /usr/bin/rusty-photon-filemonitor",
                "podman exec <c> test ! -e /usr/lib/systemd/system/rusty-photon-filemonitor.service",
                "podman exec <c> pgrep -f /usr/bin/rusty-photon-filemonitor",
                "podman exec <c> test -f /var/lib/rusty-photon/.config/rusty-photon/filemonitor.json",
                "podman exec <c> test -d /var/lib/rusty-photon/filemonitor",
            ]
        );
        // Fedora's packages are resolved from their declarations alone.
        assert!(
            !lines.iter().any(|line| line.contains("apt-get")),
            "{lines:?}"
        );
    }

    #[test]
    fn an_rpm_its_scriptlet_started_fails() {
        let linux = Linux::new(&["filemonitor"]);
        let mut runner = container_script();
        let error = linux.verify(&mut runner, &request(None, true)).unwrap_err();
        assert_eq!(
            error.to_string(),
            "FAIL [filemonitor]: unit auto-started on rpm install (scriptlets enable only)"
        );
    }

    #[test]
    fn an_erased_rpm_whose_process_lives_on_fails() {
        let linux = Linux::new(&["filemonitor"]);
        let mut runner = rpm(Script::default().on(&c("pgrep"), || Ok(String::new())));
        let error = linux.verify(&mut runner, &request(None, true)).unwrap_err();
        assert_eq!(
            error.to_string(),
            "FAIL [filemonitor]: process still running after erase (pre_uninstall must stop it)"
        );
    }

    #[test]
    fn a_gated_service_waits_on_its_config_and_is_never_probed() {
        let linux = Linux::new(&["plate-solver"]);
        let mut runner = container_script().on(
            &c("systemctl is-active --quiet rusty-photon-plate-solver"),
            || Err(()),
        );
        linux.verify(&mut runner, &request(None, false)).unwrap();
        let lines = linux.shorten(&runner.lines);
        assert!(lines.contains(
            &"podman exec <c> test ! -e /var/lib/rusty-photon/.config/rusty-photon/plate-solver.json"
                .to_owned()
        ));
        assert!(!lines.iter().any(|line| line.contains("curl")), "{lines:?}");
    }

    #[test]
    fn a_gated_unit_active_without_its_config_fails() {
        let linux = Linux::new(&["plate-solver"]);
        let mut runner = container_script();
        let error = linux
            .verify(&mut runner, &request(None, false))
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "FAIL [plate-solver]: gated unit is active without a config"
        );
    }

    #[test]
    fn a_gated_unit_that_failed_fails() {
        let linux = Linux::new(&["plate-solver"]);
        let mut runner = container(
            Script::default()
                .on(&c("systemctl is-failed"), || Ok(String::new()))
                .on(
                    &c("systemctl is-active --quiet rusty-photon-plate-solver"),
                    || Err(()),
                ),
        );
        let error = linux
            .verify(&mut runner, &request(None, false))
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "FAIL [plate-solver]: gated unit failed instead of waiting on ConditionPathExists"
        );
    }

    #[test]
    fn a_serial_driver_retrying_on_its_absent_device_passes_without_a_probe() {
        let linux = Linux::new(&["ppba-driver"]);
        let mut runner = container_script().on(&c("systemctl show -p ActiveState"), || {
            Ok("activating\n".to_owned())
        });
        linux.verify(&mut runner, &request(None, false)).unwrap();
        let lines = linux.shorten(&runner.lines);
        assert!(lines.contains(
            &"podman exec <c> sh -c journalctl -u rusty-photon-ppba-driver --no-pager 2>/dev/null \
              | grep -q 'eager startup handshake'"
                .to_owned()
        ));
        assert!(!lines.iter().any(|line| line.contains("curl")), "{lines:?}");
    }

    #[test]
    fn a_serial_driver_that_settles_serving_passes() {
        let linux = Linux::new(&["ppba-driver"]);
        let mut runner = container_script().on(&c("systemctl show -p ActiveState"), || {
            Ok("active\n".to_owned())
        });
        linux.verify(&mut runner, &request(None, false)).unwrap();
    }

    #[test]
    fn a_serial_driver_active_but_silent_fails_after_fifteen_seconds() {
        let linux = Linux::new(&["ppba-driver"]);
        let mut runner = container_script()
            .on(&c("systemctl show -p ActiveState"), || {
                Ok("active\n".to_owned())
            })
            .on(&c("curl"), || Err(()));
        let error = linux
            .verify(&mut runner, &request(None, false))
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "FAIL [ppba-driver]: active for 15s but no HTTP response on port 11112 \
             (/management/apiversions)"
        );
        let probes = runner
            .lines
            .iter()
            .filter(|line| line.contains("curl"))
            .count();
        assert_eq!(probes, 15);
    }

    #[test]
    fn a_serial_driver_whose_state_cannot_be_read_fails() {
        let linux = Linux::new(&["ppba-driver"]);
        let mut runner = container_script().on(&c("systemctl show -p ActiveState"), || Err(()));
        let error = linux
            .verify(&mut runner, &request(None, false))
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "FAIL [ppba-driver]: could not read the unit's ActiveState"
        );
    }

    #[test]
    fn a_serial_driver_that_never_tries_its_device_fails() {
        let linux = Linux::new(&["ppba-driver"]);
        let mut runner = container_script().on(&c("sh -c journalctl"), || Err(()));
        let error = linux
            .verify(&mut runner, &request(None, false))
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "FAIL [ppba-driver]: no eager-handshake attempt in the journal"
        );
        let attempts = runner
            .lines
            .iter()
            .filter(|line| line.contains("eager startup handshake"))
            .count();
        assert_eq!(attempts, 30);
    }

    #[test]
    fn a_service_that_never_answers_fails_after_thirty_probes() {
        let linux = Linux::new(&["filemonitor"]);
        let mut runner = container_script().on(&c("curl"), || Err(()));
        let error = linux
            .verify(&mut runner, &request(None, false))
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "FAIL [filemonitor]: no HTTP response on port 11111 (/management/apiversions)"
        );
        let probes = runner
            .lines
            .iter()
            .filter(|line| line.contains("curl"))
            .count();
        assert_eq!(probes, 30);
        // Then the journal, and the container goes.
        let lines = linux.shorten(&runner.lines);
        assert_eq!(
            lines[lines.len() - 2..],
            [
                "podman exec <c> journalctl -u rusty-photon-filemonitor --no-pager -n 40",
                "podman rm -f <c>",
            ]
        );
    }

    #[test]
    fn keeps_the_container_when_asked() {
        let linux = Linux::new(&["filemonitor"]);
        let mut runner = container_script();
        let request = Request {
            keep: true,
            ..request(None, false)
        };
        linux.verify(&mut runner, &request).unwrap();
        assert!(
            !runner
                .lines
                .iter()
                .any(|line| line.starts_with("podman rm")),
            "{:?}",
            runner.lines
        );
    }

    #[test]
    fn phd2_guider_passes_on_a_503_and_fails_on_no_answer() {
        let linux = Linux::new(&["phd2-guider"]);
        let mut runner = container_script().on(&c("curl -s "), || Ok("503".to_owned()));
        linux.verify(&mut runner, &request(None, false)).unwrap();
        let mut runner = container_script().on(&c("curl -s "), || Err(()));
        let error = linux
            .verify(&mut runner, &request(None, false))
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "FAIL [phd2-guider]: no HTTP response on port 11130 (/health; last code 000)"
        );
    }

    #[test]
    fn phd2_guider_writes_no_config() {
        let linux = Linux::new(&["phd2-guider"]);
        let mut runner = container_script().on(&c("curl -s "), || Ok("200".to_owned()));
        linux.verify(&mut runner, &request(None, false)).unwrap();
        // Nor waits for one; its purge still checks none is left.
        assert!(
            !runner
                .lines
                .iter()
                .any(|line| line.contains("test -s") || line.contains("test -f")),
            "{:?}",
            runner.lines
        );
    }

    #[test]
    fn a_zwo_service_loads_its_own_blob_from_the_runpath_and_no_other() {
        let linux = Linux::new(&["zwo-camera"]);
        let own = "libASICamera2.so => /usr/lib/rusty-photon/libASICamera2.so (0x7f)\n";
        let mut runner = container_script().on(&c("ldd"), move || Ok(own.to_owned()));
        linux.verify(&mut runner, &request(None, false)).unwrap();

        let both = "libASICamera2.so => /usr/lib/rusty-photon/libASICamera2.so (0x7f)\n\
                    libEAFFocuser.so => /usr/lib/rusty-photon/libEAFFocuser.so (0x7e)\n";
        let mut runner = container_script().on(&c("ldd"), move || Ok(both.to_owned()));
        let error = linux
            .verify(&mut runner, &request(None, false))
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "FAIL [zwo-camera]: binary links SDKs it must not (libEFWFilter, libEAFFocuser; \
             ADR-014 per-device link)"
        );

        let elsewhere = "libASICamera2.so => /usr/local/lib/libASICamera2.so (0x7f)\n";
        let mut runner = container_script().on(&c("ldd"), move || Ok(elsewhere.to_owned()));
        let error = linux
            .verify(&mut runner, &request(None, false))
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "FAIL [zwo-camera]: libASICamera2.so does not resolve to /usr/lib/rusty-photon (RUNPATH)"
        );
    }

    #[test]
    fn svbony_camera_withholds_its_sdk_until_the_operator_installs_it() {
        let linux = Linux::new(&["svbony-camera"]);
        let sdk = "/usr/lib/rusty-photon/libSVBCameraSDK.so";
        let ldd = format!("libSVBCameraSDK.so => {sdk} (0x7f)\n");
        let mut runner = container_script().on(&c("ldd"), move || Ok(ldd.clone()));
        linux.verify(&mut runner, &request(None, false)).unwrap();
        let lines = linux.shorten(&runner.lines);
        let at = |line: &str| {
            lines
                .iter()
                .position(|candidate| candidate == &format!("podman exec <c> {line}"))
                .unwrap()
        };
        assert!(at(&format!("test ! -e {sdk}")) < at("/usr/sbin/rusty-photon-svbony-sdk-install"));
        assert!(at("/usr/sbin/rusty-photon-svbony-sdk-install") < at(&format!("test -s {sdk}")));
        assert!(
            at(&format!("test -s {sdk}"))
                < at("systemctl is-active --quiet rusty-photon-svbony-camera")
        );

        let mut runner = container_script().on(&c(&format!("test ! -e {sdk}")), || Err(()));
        let error = linux
            .verify(&mut runner, &request(None, false))
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("FAIL [svbony-camera]: package ships {sdk} — ADR-018 forbids bundling it")
        );
    }

    #[test]
    fn sentinel_carries_its_polkit_rule_doctor_and_its_renewal_timer() {
        let linux = Linux::new(&["sentinel"]);
        let mut runner = container_script();
        linux.verify(&mut runner, &request(None, false)).unwrap();
        let lines = linux.shorten(&runner.lines);
        for line in [
            "test -f /usr/share/polkit-1/rules.d/50-rusty-photon-sentinel.rules",
            "test -x /usr/bin/rusty-photon-doctor",
            "rusty-photon-doctor --json",
            "rusty-photon-doctor tls renew",
            "systemctl is-enabled --quiet rusty-photon-renew.timer",
            "systemctl is-active --quiet rusty-photon-renew.timer",
        ] {
            assert!(
                lines.contains(&format!("podman exec <c> {line}")),
                "{line} not in {lines:?}"
            );
        }

        let mut runner = container_script().on(&c("rusty-photon-doctor tls renew"), || Err(()));
        let error = linux
            .verify(&mut runner, &request(None, false))
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "FAIL [sentinel]: rusty-photon-doctor tls renew (nothing due) did not exit 0"
        );
    }

    #[test]
    fn an_rpm_arms_sentinels_timer_at_the_next_boot() {
        let linux = Linux::new(&["sentinel"]);
        let mut runner = rpm_script();
        linux.verify(&mut runner, &request(None, true)).unwrap();
        assert!(
            !runner
                .lines
                .iter()
                .any(|line| line.ends_with("is-active --quiet rusty-photon-renew.timer")),
            "{:?}",
            runner.lines
        );
    }

    #[test]
    fn a_deb_built_off_debian_gets_its_runtime_libraries_beforehand() {
        let linux = Linux::new(&["filemonitor"]);
        let libraries = apt("install -y --no-install-recommends libusb-1.0-0 libudev1 libstdc++6");
        let mut runner = container_script();
        linux.verify(&mut runner, &request(None, false)).unwrap();
        assert!(!runner.lines.contains(&libraries), "{:?}", runner.lines);
        let mut runner =
            container(Script::default().on(&c("dpkg-deb -f"), || Ok("adduser\n".to_owned())));
        linux.verify(&mut runner, &request(None, false)).unwrap();
        assert!(runner.lines.contains(&libraries), "{:?}", runner.lines);
    }

    #[test]
    fn a_purge_that_leaves_the_config_fails() {
        let linux = Linux::new(&["filemonitor"]);
        let config = "/var/lib/rusty-photon/.config/rusty-photon/filemonitor.json";
        let mut runner = container_script().on(&c(&format!("test ! -e {config}")), || Err(()));
        let error = linux
            .verify(&mut runner, &request(None, false))
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "FAIL [filemonitor]: config survived purge"
        );
    }

    #[test]
    fn removing_the_shared_home_fails() {
        let linux = Linux::new(&["filemonitor"]);
        let mut runner = container_script().on(&c("test -d /var/lib/rusty-photon"), || Err(()));
        let error = linux
            .verify(&mut runner, &request(None, false))
            .unwrap_err();
        assert_eq!(error.to_string(), "FAIL: shared home removed by uninstall");
    }

    #[test]
    fn a_container_whose_systemd_never_comes_up_is_removed() {
        let linux = Linux::new(&["filemonitor"]);
        let mut runner = Script::default().on(&c("sh -c systemctl show -p SystemState"), || {
            Ok("starting\n".to_owned())
        });
        let error = linux
            .verify(&mut runner, &request(None, false))
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "systemd did not come up in the container (state: starting)"
        );
        let reads = runner
            .lines
            .iter()
            .filter(|line| line.contains("SystemState"))
            .count();
        assert_eq!(reads, 60);
        assert_eq!(
            linux.shorten(&runner.lines).last().unwrap(),
            "podman rm -f <c>"
        );
    }

    #[test]
    fn refuses_without_podman_or_packages() {
        let linux = Linux::new(&[]);
        let mut runner = Script::default().on("podman --version", || Err(()));
        let error = linux
            .verify(&mut runner, &request(None, false))
            .unwrap_err();
        assert_eq!(error.to_string(), "podman not found");
        let mut runner = Script::default();
        let error = linux
            .verify(&mut runner, &request(None, false))
            .unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("no rusty-photon-*.deb packages in "),
            "{error}"
        );
        let request = Request {
            dist: Some(linux.root().join("dist/9.9.9")),
            ..request(None, false)
        };
        let error = linux.verify(&mut runner, &request).unwrap_err();
        assert!(
            error
                .to_string()
                .ends_with("dist/9.9.9 not found — run `cargo ci build-packages` first"),
            "{error}"
        );
    }

    #[test]
    fn refuses_two_packages_for_a_service_or_none() {
        let linux = Linux::new(&["filemonitor"]);
        linux.add("rusty-photon-filemonitor_0.0.9-1_amd64.deb");
        let mut runner = Script::default();
        let error = linux
            .verify(&mut runner, &request(None, false))
            .unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("multiple rusty-photon-filemonitor_*.deb in "),
            "{error}"
        );
        let error = linux
            .verify(&mut runner, &request(Some(&["rp"]), true))
            .unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("no rusty-photon-rp-*.rpm in "),
            "{error}"
        );
        assert!(
            !runner
                .lines
                .iter()
                .any(|line| line.starts_with("podman build")),
            "{:?}",
            runner.lines
        );
    }

    #[test]
    fn refuses_a_service_without_a_port() {
        let linux = Linux::new(&["no-such-service"]);
        let mut runner = Script::default();
        let error = linux
            .verify(&mut runner, &request(None, false))
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "no port for: no-such-service — extend packaging::PORTS"
        );
    }
}
