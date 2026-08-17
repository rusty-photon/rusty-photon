//! Aggregation over the per-service doctors (docs/services/doctor.md
//! §Aggregation — the two probe paths).
//!
//! For every installed unit whose run state is known, exactly one probe
//! runs: an **active** Alpaca-class service is asked over HTTP for its
//! configured devices (it already enumerated its hardware at startup); an
//! **inactive** unit's own binary is run as `doctor --json` and the
//! returned checks merge into the report. Units whose staged facts carry
//! no run state have no aggregation story and are skipped — which is also
//! what keeps every pre-D5 staged scenario meaning what it meant.
//!
//! Both probes are bounded (a short HTTP timeout, a generous shell-out
//! one), and an answer that never comes is a diagnosis, not a crash.

use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use tracing::debug;

use crate::checks::Context;
use crate::facts::UnitFacts;
use crate::report::{Check, Report};
use crate::scan::ServiceScan;
use rusty_photon_server_config::doctor_toml::ServerClass;

/// The active-unit probe's whole-request deadline. A healthy service
/// answers its management API in milliseconds and a dead port refuses at
/// once (Windows takes ~2 s to report a loopback refusal), so this bound
/// only decides how long an operator waits on a service that accepts the
/// connection and never answers — and how much headroom a *loaded* host
/// gets before a live service is misreported as dead. Measured on a
/// 4-vCPU Windows host under CPU contention, a fresh process's request
/// to a live loopback listener took up to ~4.8 s end to end (name
/// resolution, the `localhost` → `::1` → `127.0.0.1` fallback that
/// costs 300 ms there, connect, response), so 5 s produced false
/// "does not answer" failures; 15 s keeps 3× headroom over that.
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);

/// The inactive-unit probe: a per-service doctor may run an SDK bus scan,
/// which takes seconds — but never a minute.
const SHELL_OUT_TIMEOUT: Duration = Duration::from_mins(1);

/// What one unit's probe contributes to the report.
enum Probe<'a> {
    /// Active Alpaca-class service → `GET /management/v1/configureddevices`.
    Devices(&'a ServiceScan),
    /// Installed-but-inactive unit → `<binary> doctor --json`.
    ShellOut(&'a ServiceScan, &'a UnitFacts),
}

/// Run the aggregation probes for every installed unit with a known run
/// state. Pure fan-out over [`Probe`]; returns no checks (and builds no
/// runtime) on a host with nothing to probe — a dev checkout diagnosis
/// stays exactly what it was.
#[must_use]
pub fn checks(ctx: &Context) -> Vec<Check> {
    let probes: Vec<Probe> = ctx
        .scans
        .iter()
        .filter_map(|scan| {
            let unit = ctx.facts.unit(&scan.entry.unit_name())?;
            match unit.active {
                None => None,
                Some(true) => match scan.entry.class {
                    // Non-Alpaca services expose no management API; the
                    // config-side checks cover them fully.
                    ServerClass::Core | ServerClass::Advertising => None,
                    ServerClass::Alpaca => Some(Probe::Devices(scan)),
                },
                Some(false) => Some(Probe::ShellOut(scan, unit)),
            }
        })
        .collect();
    let fake_mount = fake_mount_probe_target(ctx);
    if probes.is_empty() && fake_mount.is_none() {
        return Vec::new();
    }

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            return vec![Check::warn(
                "service.doctor-probe",
                None,
                format!("could not start the probe runtime: {e}"),
                None,
            )];
        }
    };
    let acme_domain = acme_probe_domain(&ctx.config_dir);
    runtime.block_on(async {
        let mut checks = Vec::new();
        for probe in probes {
            match probe {
                Probe::Devices(scan) => {
                    checks.push(probe_devices(ctx, scan, acme_domain.as_deref()).await);
                }
                Probe::ShellOut(scan, unit) => {
                    checks.extend(probe_shell_out(ctx, scan, unit).await);
                }
            }
        }
        if let Some(fake_mount) = fake_mount {
            checks.extend(probe_fake_mount(ctx, &fake_mount, acme_domain.as_deref()).await);
        }
        checks
    })
}

/// The domain under which every service's public name lives on an ACME
/// install: `acme.json`'s `domain`, the base of the wildcard the fleet
/// serves. `None` on a self-signed install, and when `acme.json` is
/// present but unreadable — the probes then keep the self-signed shape
/// rather than guess at names.
fn acme_probe_domain(config_dir: &std::path::Path) -> Option<String> {
    if !crate::provision::acme_active(config_dir) {
        return None;
    }
    match crate::provision::acme_config::load_acme_config(&config_dir.join("acme.json")) {
        Ok(acme) => Some(acme.domain),
        Err(e) => {
            debug!("acme.json is present but unreadable ({e}); probing localhost");
            None
        }
    }
}

/// The host the active-service probe dials: on an ACME install the
/// service's public name — the wildcard's only SAN is `*.<domain>`, which
/// can never match `localhost` — and on a self-signed install
/// `localhost`, a SAN of every doctor-issued certificate. The same split
/// sentinel's `probe_domain` config key answers for its service probes.
fn probe_host(acme_domain: Option<&str>, service: &str) -> String {
    acme_domain.map_or_else(
        || "localhost".to_string(),
        |domain| format!("{service}.{domain}"),
    )
}

/// What the fake-mount probe needs, gathered from the static scans.
struct FakeMountProbe {
    /// rp's `equipment.mount.alpaca_url`, probed as rp itself would
    /// connect.
    url: String,
    /// The locally-scanned bridge config's `device.unique_id`.
    bridge_unique_id: String,
    /// rp's `equipment.mount.auth` client credential, if any.
    auth: Option<crate::scan::ClientAuthView>,
}

/// The `UniqueID` leg of `joins.fake-mount` (planetarium-bridge.md §
/// Doctor integration): the static port join (`crate::checks`) only
/// resolves loopback URLs, but a rig config addresses services by host
/// name (`<svc>.rig.rustyphoton.io`), where the port join is silently
/// skipped by design. This leg asks the configured mount itself —
/// `GET /management/v1/configureddevices` on rp's
/// `equipment.mount.alpaca_url` — and fails when any reported device's
/// `UniqueID` is the locally-installed bridge's minted `device.unique_id`.
/// Skipped when the static leg already resolved the URL to the bridge
/// (one check, not two) and silent when the mount does not answer —
/// liveness is `service.devices`' story, not this hazard's.
fn fake_mount_probe_target(ctx: &Context) -> Option<FakeMountProbe> {
    let rp = ctx
        .scans
        .iter()
        .find(|s| s.entry.name == "rp")
        .and_then(|s| crate::scan::view::<crate::scan::RpView>(s)?.ok())?;
    let url = rp.mount_alpaca_url()?;
    let bridge_unique_id = ctx
        .scans
        .iter()
        .find(|s| s.entry.name == "planetarium-bridge")
        .and_then(|s| crate::scan::view::<crate::scan::BridgeView>(s)?.ok())
        .and_then(|bridge| bridge.device?.unique_id)
        .filter(|id| !id.is_empty())?;
    let (_, host, port) = crate::checks::parse_target_url(&url)?;
    if crate::checks::resolve_join_target(ctx, &host, port)
        .is_some_and(|target| target.entry.name == "planetarium-bridge")
    {
        return None;
    }
    Some(FakeMountProbe {
        url,
        bridge_unique_id,
        auth: rp.mount_auth(),
    })
}

async fn probe_fake_mount(
    ctx: &Context,
    probe: &FakeMountProbe,
    acme_domain: Option<&str>,
) -> Vec<Check> {
    let origin = match reqwest::Url::parse(&probe.url) {
        Ok(parsed) => match parsed.origin().ascii_serialization() {
            origin if origin != "null" => origin,
            _ => return Vec::new(),
        },
        Err(_) => return Vec::new(),
    };
    let url = format!("{origin}/management/v1/configureddevices");
    debug!(
        url,
        "probing the configured mount for the fake-mount hazard"
    );

    // Trust for an https mount follows the install's TLS story, as the
    // devices probe does: doctor's own CA on a self-signed install (the
    // fleet's material is doctor-issued), the platform store on an ACME
    // one (the wildcard is publicly trusted; doctor's CA would reject it).
    let ca_path = (probe.url.starts_with("https") && acme_domain.is_none()).then(|| {
        rusty_photon_tls::config::ca_cert_path(&crate::provision::pki_dir(&ctx.config_dir))
    });
    let Ok(client) = rusty_photon_tls::client::build_reqwest_client(ca_path.as_deref()) else {
        return Vec::new();
    };
    let mut request = client.get(&url).timeout(HTTP_TIMEOUT);
    // Present the credential rp itself would: its own equipment.mount.auth,
    // falling back to the observatory credential. Every credential problem
    // is joins.client-auth's story; an unanswerable or 401ing mount stays
    // silent here.
    let fallback = crate::provision::read_credential(&ctx.config_dir);
    if let Some(auth) = &probe.auth {
        if let (Some(username), Some(password)) = (&auth.username, &auth.password) {
            request = request.basic_auth(username, Some(password));
        }
    } else if let Some(password) = &fallback {
        request = request.basic_auth(crate::provision::CREDENTIAL_USERNAME, Some(password));
    }

    let Ok(response) = request.send().await else {
        return Vec::new();
    };
    if !response.status().is_success() {
        return Vec::new();
    }
    let Ok(management) = response.json::<ManagementResponse>().await else {
        return Vec::new();
    };
    let Some(device) = management
        .value
        .iter()
        .find(|d| d.unique_id == probe.bridge_unique_id)
    else {
        return Vec::new();
    };
    vec![Check::fail(
        "joins.fake-mount",
        Some("rp".to_string()),
        format!(
            "equipment.mount.alpaca_url ({url_cfg}) answers with UniqueID {unique_id} — that \
             is the installed planetarium-bridge ({device_type} \"{name}\"), a virtual \
             target-entry device, not a mount; slews against it \"just succeed\" without \
             moving anything, so every motion safeguard rp relies on (park, limits, slew \
             completion) is fiction",
            url_cfg = probe.url,
            unique_id = device.unique_id,
            device_type = device.device_type,
            name = device.name,
        ),
        Some(
            "point equipment.mount.alpaca_url at the real mount driver; planetarium apps \
             connect to the bridge, rp never does"
                .to_string(),
        ),
    )]
}

/// The subset of an Alpaca management response the inventory reads.
#[derive(Debug, Deserialize)]
struct ManagementResponse {
    #[serde(rename = "Value", default)]
    value: Vec<ConfiguredDevice>,
}

#[derive(Debug, Deserialize)]
struct ConfiguredDevice {
    #[serde(rename = "DeviceName", default)]
    name: String,
    #[serde(rename = "DeviceType", default)]
    device_type: String,
    #[serde(rename = "DeviceNumber", default)]
    number: u32,
    /// Read by the fake-mount probe only; the inventory ignores it.
    #[serde(rename = "UniqueID", default)]
    unique_id: String,
}

/// The warn for a probe client that could not be built. With a CA to
/// load, the failure is the missing-trust-root story and the pki fix
/// applies; with none to load (ACME install, plain-HTTP probe) it is not
/// a pki problem, and the pki suggestion would mislead.
fn client_build_warn(service: Option<String>, had_ca: bool, e: &impl std::fmt::Display) -> Check {
    if had_ca {
        Check::warn(
            "service.devices",
            service,
            format!(
                "the service serves TLS but doctor could not load its trust root: {e} \
                 — the probe was skipped"
            ),
            Some("run `rusty-photon-doctor tls issue` to (re)create the pki tree".to_string()),
        )
    } else {
        Check::warn(
            "service.devices",
            service,
            format!("doctor could not build its probe client: {e} — the probe was skipped"),
            None,
        )
    }
}

/// Ask an active Alpaca service for its configured devices, following the
/// service's own config: HTTPS when its `server.tls` is set, the
/// observatory credential when its `server.auth` is on. The dialled host
/// and the root of trust come as a pair from the install's TLS story:
/// `<service>.<domain>` verified by the platform store on an ACME
/// install, `localhost` verified by doctor's own CA on a self-signed one.
async fn probe_devices(ctx: &Context, scan: &ServiceScan, acme_domain: Option<&str>) -> Check {
    let service = Some(scan.entry.name.to_string());
    let port = scan.effective_port();
    let tls_on = scan.server().is_some_and(|s| s.tls.is_some());
    let auth_on = scan.server().is_some_and(|s| s.auth.is_some());
    let scheme = if tls_on { "https" } else { "http" };
    let host = probe_host(acme_domain, scan.entry.name);
    let url = format!("{scheme}://{host}:{port}/management/v1/configureddevices");
    debug!(service = scan.entry.name, url, "probing the active service");

    let ca_path = (tls_on && acme_domain.is_none()).then(|| {
        rusty_photon_tls::config::ca_cert_path(&crate::provision::pki_dir(&ctx.config_dir))
    });
    let client = match rusty_photon_tls::client::build_reqwest_client(ca_path.as_deref()) {
        Ok(client) => client,
        Err(e) => return client_build_warn(service, ca_path.is_some(), &e),
    };
    let credential = if auth_on {
        crate::provision::read_credential(&ctx.config_dir)
    } else {
        None
    };
    let mut request = client.get(&url).timeout(HTTP_TIMEOUT);
    if let Some(password) = &credential {
        request = request.basic_auth(crate::provision::CREDENTIAL_USERNAME, Some(password));
    }

    let response = match request.send().await {
        Ok(response) => response,
        Err(e) => {
            return Check::fail(
                "service.devices",
                service,
                format!(
                    "the unit is active but {url} does not answer: {}",
                    describe_send_error(e, HTTP_TIMEOUT)
                ),
                Some(
                    "an active service that cannot answer its own port fails at night \
                     — restart it and check its logs"
                        .to_string(),
                ),
            );
        }
    };
    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        let held = if credential.is_some() {
            "the observatory credential was rejected"
        } else {
            "doctor holds no credential for it (no pki/credential)"
        };
        return Check::warn(
            "service.devices",
            service,
            format!(
                "the service is alive but its management API answered {status} — {held}, \
                 so liveness is proven but the device inventory is not"
            ),
            Some("run `rusty-photon-doctor --fix` to align the observatory credential".to_string()),
        );
    }
    if !status.is_success() {
        return Check::fail(
            "service.devices",
            service,
            format!("the management API answered HTTP {status}"),
            Some("check the service's logs".to_string()),
        );
    }
    match response.json::<ManagementResponse>().await {
        Ok(management) => Check::ok(
            "service.devices",
            service,
            describe_devices(&management.value),
        ),
        Err(e) => Check::fail(
            "service.devices",
            service,
            format!("the management API answered but its payload did not parse: {e}"),
            Some("check the service's logs".to_string()),
        ),
    }
}

/// Why a probe request failed, with the cause chain reqwest's `Display`
/// leaves out: on its own the error reads "error sending request for url
/// (...)" whether the port refused the connection, the TLS handshake
/// failed, or the request outran its deadline — and those call for
/// different repairs. The deadline case names `deadline` — the bound the
/// request was sent with — so "nothing listens" and "answers, but not
/// within the bound" read differently. The URL is dropped from the text —
/// the check's detail already names it.
fn describe_send_error(e: reqwest::Error, deadline: Duration) -> String {
    use std::fmt::Write as _;
    let e = e.without_url();
    let mut text = e.to_string();
    let mut source = std::error::Error::source(&e);
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    if e.is_timeout() {
        let _ = write!(
            text,
            " (no answer within {})",
            humantime::format_duration(deadline)
        );
    }
    text
}

fn describe_devices(devices: &[ConfiguredDevice]) -> String {
    if devices.is_empty() {
        return "the service reports 0 configured devices".to_string();
    }
    let listed: Vec<String> = devices
        .iter()
        .map(|d| format!("{} \"{}\" (#{})", d.device_type, d.name, d.number))
        .collect();
    format!(
        "the service reports {} configured device(s): {}",
        devices.len(),
        listed.join(", ")
    )
}

/// Run an inactive unit's own binary as `doctor --json --config <file>` and
/// merge the returned checks. Every way the probe itself can go wrong is a
/// `warn` under `service.doctor-probe` — most of them are the version-skew
/// signature (a binary from before D5 does not know the subcommand), and an
/// old binary is not a broken rig.
async fn probe_shell_out(ctx: &Context, scan: &ServiceScan, unit: &UnitFacts) -> Vec<Check> {
    let name = scan.entry.name;
    let Some(binary) = &unit.binary_path else {
        return vec![Check::warn(
            "service.doctor-probe",
            Some(name.to_string()),
            "the unit is installed but its service manager entry records no binary path, \
             so its own doctor could not be asked",
            None,
        )];
    };
    let config = ctx.config_dir.join(scan.entry.config_file());
    run_child_doctor(name, binary, &config, SHELL_OUT_TIMEOUT).await
}

/// Run `<binary> doctor --json --config <config>` bounded by `timeout` and
/// interpret the outcome. The timeout is a parameter so tests can exercise
/// the timeout arm without waiting out the production bound.
async fn run_child_doctor(
    name: &str,
    binary: &std::path::Path,
    config: &std::path::Path,
    timeout: Duration,
) -> Vec<Check> {
    let service = Some(name.to_string());
    debug!(service = name, binary = %binary.display(), "running the per-service doctor");

    let mut command = tokio::process::Command::new(binary);
    command
        .arg("doctor")
        .arg("--json")
        .arg("--config")
        .arg(config)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let output = match tokio::time::timeout(timeout, command.output()).await {
        Err(_elapsed) => {
            return vec![Check::warn(
                "service.doctor-probe",
                service,
                format!(
                    "{} doctor did not answer within {} and was stopped",
                    binary.display(),
                    humantime::format_duration(timeout)
                ),
                None,
            )];
        }
        Ok(Err(e)) => {
            return vec![Check::warn(
                "service.doctor-probe",
                service,
                format!("could not run {} doctor: {e}", binary.display()),
                None,
            )];
        }
        Ok(Ok(output)) => output,
    };

    match serde_json::from_slice::<Report>(&output.stdout) {
        Ok(child) if !child.checks.is_empty() => merge_child_checks(child, name),
        Ok(_) => vec![Check::warn(
            "service.doctor-probe",
            service,
            "the per-service doctor returned a report with no checks",
            None,
        )],
        Err(_) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let trail = stderr.lines().next().unwrap_or("").trim();
            vec![Check::warn(
                "service.doctor-probe",
                service,
                format!(
                    "{} did not produce a doctor report (exit {}{}{}) — commonly a \
                     binary from before the doctor subcommand (update the service \
                     package); a crash or non-JSON output on stdout lands here too, \
                     and then the exit code and stderr above are the clue",
                    binary.display(),
                    output
                        .status
                        .code()
                        .map_or_else(|| "?".to_string(), |c| c.to_string()),
                    if trail.is_empty() { "" } else { "; stderr: " },
                    trail,
                ),
                None,
            )]
        }
    }
}

/// The merge itself: the child's checks join the aggregate report scoped to
/// the emitting service (the child self-scopes at the report level, not per
/// check). Statuses — including `Unknown` from a newer binary — carry over
/// untouched, so the child's failures fail the aggregate exit code.
fn merge_child_checks(child: Report, service: &str) -> Vec<Check> {
    child
        .checks
        .into_iter()
        .map(|mut check| {
            check.service.get_or_insert_with(|| service.to_string());
            check
        })
        .collect()
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::report::Status;

    /// Stage an executable that hangs far longer than any test timeout: a
    /// `.cmd` on Windows, a `chmod +x` shell script elsewhere (the same two
    /// shapes the BDD aggregation steps stage as stub binaries).
    ///
    /// Neither body leaves a grandchild behind. `kill_on_drop` reaches only
    /// the direct child — the interpreter — and a surviving grandchild keeps
    /// its inherited copy of the probe's stdout/stderr pipes open for its
    /// whole lifetime, so those pipes never reach EOF. Tokio backs child
    /// stdio with blocking reads on Windows, and a read that cannot be
    /// cancelled parks a blocking-pool thread that the runtime's drop then
    /// waits out — far past the timeout under test. Redirecting the
    /// grandchild's output is not enough: it only reassigns the std handles,
    /// while the pipe handles stay inheritable and come along regardless.
    /// So `exec` replaces the shell outright, and the `.cmd` spins inside
    /// `cmd.exe` on the internal `for /l` (step 0 never reaches its bound)
    /// rather than shelling out to `ping` or `timeout` for the delay.
    fn stage_hanging_binary(dir: &std::path::Path) -> std::path::PathBuf {
        #[cfg(windows)]
        {
            let path = dir.join("hang.cmd");
            std::fs::write(&path, "@for /l %%i in (1,0,2) do @rem\r\n").unwrap();
            path
        }
        #[cfg(not(windows))]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = dir.join("hang.sh");
            std::fs::write(&path, "#!/bin/sh\nexec sleep 60\n").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        }
    }

    #[tokio::test]
    async fn test_run_child_doctor_timeout_warns_with_humantime_bound() {
        let dir = tempfile::tempdir().unwrap();
        let binary = stage_hanging_binary(dir.path());
        let config = dir.path().join("svc.json");
        std::fs::write(&config, "{}").unwrap();

        let checks = run_child_doctor("svc", &binary, &config, Duration::from_millis(50)).await;

        assert_eq!(checks.len(), 1, "{checks:?}");
        assert_eq!(checks[0].name, "service.doctor-probe");
        assert_eq!(checks[0].status, Status::Warn);
        assert_eq!(checks[0].service.as_deref(), Some("svc"));
        assert!(
            checks[0]
                .detail
                .ends_with("doctor did not answer within 50ms and was stopped"),
            "{}",
            checks[0].detail
        );
    }

    #[test]
    fn test_merge_scopes_unscoped_child_checks_to_the_service() {
        let child: Report = serde_json::from_str(
            r#"{
                "mode": "service",
                "service": "ppba-driver",
                "checks": [
                    { "name": "config.full-shape", "status": "fail", "detail": "unknown key" },
                    { "name": "already.scoped", "service": "other", "status": "ok" }
                ]
            }"#,
        )
        .unwrap();
        let merged = merge_child_checks(child, "ppba-driver");
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].service.as_deref(), Some("ppba-driver"));
        assert_eq!(merged[0].status, Status::Fail);
        assert_eq!(
            merged[1].service.as_deref(),
            Some("other"),
            "a check the child already scoped keeps its scope"
        );
    }

    #[test]
    fn test_merge_preserves_unknown_statuses_from_newer_binaries() {
        let child: Report = serde_json::from_str(
            r#"{ "checks": [ { "name": "novel.check", "status": "degraded" } ] }"#,
        )
        .unwrap();
        let merged = merge_child_checks(child, "rp");
        assert_eq!(merged[0].status, Status::Unknown);
    }

    #[tokio::test]
    async fn test_describe_send_error_names_the_refused_connection() {
        // Port 1 sits outside every OS's dynamic range and is privileged
        // on Unix, so nothing here can be listening: the probe is refused.
        let e = reqwest::Client::new()
            .get("http://127.0.0.1:1/management/v1/configureddevices")
            .send()
            .await
            .unwrap_err();
        let text = describe_send_error(e, HTTP_TIMEOUT);
        assert!(text.starts_with("error sending request"), "{text}");
        assert!(
            text.contains("tcp connect error"),
            "the cause chain names the failing phase: {text}"
        );
        assert!(
            !text.contains("no answer within"),
            "a refusal is not a deadline: {text}"
        );
    }

    #[tokio::test]
    async fn test_describe_send_error_names_the_deadline_on_a_timeout() {
        // A listener that accepts and never answers, so only the
        // request deadline ends the wait.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            // Hold the accepted socket open forever without answering;
            // dropping it would reset the connection and turn the
            // timeout under test into a connection error.
            let Ok((_socket, _)) = listener.accept().await else {
                return;
            };
            std::future::pending::<()>().await;
        });
        // One value for the request's deadline and the description, as
        // the probe pairs them — the text must name the bound that fired.
        let deadline = Duration::from_millis(50);
        let e = reqwest::Client::new()
            .get(format!("http://{addr}/management/v1/configureddevices"))
            .timeout(deadline)
            .send()
            .await
            .unwrap_err();
        assert!(e.is_timeout(), "{e:?}");
        let text = describe_send_error(e, deadline);
        assert!(text.contains("timed out"), "{text}");
        assert!(text.ends_with("(no answer within 50ms)"), "{text}");
    }

    #[test]
    fn test_describe_devices_lists_type_name_and_number() {
        assert_eq!(
            describe_devices(&[]),
            "the service reports 0 configured devices"
        );
        let devices = vec![
            ConfiguredDevice {
                name: "QHY178M".to_string(),
                device_type: "Camera".to_string(),
                number: 0,
                unique_id: "qhy-uid".to_string(),
            },
            ConfiguredDevice {
                name: "EAF".to_string(),
                device_type: "Focuser".to_string(),
                number: 1,
                unique_id: "eaf-uid".to_string(),
            },
        ];
        let text = describe_devices(&devices);
        assert!(
            text.contains("2 configured device(s)")
                && text.contains("Camera \"QHY178M\" (#0)")
                && text.contains("Focuser \"EAF\" (#1)"),
            "{text}"
        );
    }

    #[test]
    fn test_client_build_warn_with_a_ca_suggests_the_pki_tree() {
        let check = client_build_warn(Some("ppba-driver".to_string()), true, &"boom");
        assert_eq!(check.status, Status::Warn);
        assert_eq!(check.service.as_deref(), Some("ppba-driver"));
        assert!(
            check.detail.contains("could not load its trust root: boom"),
            "{}",
            check.detail
        );
        assert!(
            check.suggestion.unwrap().contains("pki tree"),
            "the CA-loading failure keeps the pki suggestion"
        );
    }

    #[test]
    fn test_client_build_warn_without_a_ca_gives_no_pki_guidance() {
        let check = client_build_warn(Some("ppba-driver".to_string()), false, &"boom");
        assert_eq!(check.status, Status::Warn);
        assert!(
            check
                .detail
                .contains("could not build its probe client: boom"),
            "{}",
            check.detail
        );
        assert_eq!(check.suggestion, None);
    }

    #[test]
    fn test_probe_host_is_localhost_without_an_acme_domain() {
        assert_eq!(probe_host(None, "ppba-driver"), "localhost");
    }

    #[test]
    fn test_probe_host_is_the_public_name_under_the_acme_domain() {
        assert_eq!(
            probe_host(Some("pier1.example.com"), "filemonitor"),
            "filemonitor.pier1.example.com"
        );
    }

    #[test]
    fn test_acme_probe_domain_is_none_without_acme_json() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(acme_probe_domain(dir.path()), None);
    }

    #[test]
    fn test_acme_probe_domain_reads_the_domain_from_acme_json() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("acme.json"),
            r#"{ "email": "op@example.com", "domain": "pier1.example.com",
                 "dns_provider": "cloudflare", "dns_credentials": {} }"#,
        )
        .unwrap();
        assert_eq!(
            acme_probe_domain(dir.path()).as_deref(),
            Some("pier1.example.com")
        );
    }

    #[test]
    fn test_acme_probe_domain_is_none_when_acme_json_is_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("acme.json"), "not json").unwrap();
        assert_eq!(acme_probe_domain(dir.path()), None);
    }

    #[test]
    fn test_management_response_parses_permissively() {
        let m: ManagementResponse = serde_json::from_str(
            r#"{ "Value": [ { "DeviceName": "x", "DeviceType": "Camera",
                              "DeviceNumber": 0, "UniqueID": "u" } ],
                 "ClientTransactionID": 7, "ServerTransactionID": 9 }"#,
        )
        .unwrap();
        assert_eq!(m.value.len(), 1);
        let empty: ManagementResponse = serde_json::from_str("{}").unwrap();
        assert!(empty.value.is_empty());
    }
}
