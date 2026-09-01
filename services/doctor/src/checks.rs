//! The D2 check set (docs/services/doctor.md §Diagnosis).
//!
//! Every check is a pure function over the scanned configs and the gathered
//! platform facts — no network, no writes. Check names are the stable
//! identifiers the report schema carries.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::catalog::{self, CatalogEntry};
use crate::facts::{DnsFacts, Platform, PlatformFacts};
use crate::provision::acme_config::AcmeConfig;
use crate::report::{Check, Mode};
use crate::scan::{
    self, unknown_config_files, ClientAuthView, RpView, SentinelView, ServerBlock, ServiceScan,
    UiHtmxView,
};

/// Everything the checks look at.
pub struct Context {
    pub config_dir: PathBuf,
    pub facts: PlatformFacts,
    pub mode: Mode,
    pub scans: Vec<ServiceScan>,
    /// Device-surface facts: staged by the test seam, gathered from the
    /// host on a real run, `None` when a staged scenario has no hardware
    /// story (the family is then skipped — never probed under a mock).
    pub hardware: Option<rusty_photon_doctor_checks::HardwareFacts>,
}

impl Context {
    /// Scan the config dir and derive the mode from the unit inventory.
    ///
    /// DNS resolvability is deliberately *not* gathered here, unlike the
    /// hardware facts: `dns.unresolvable` runs only on the final report
    /// ([`dns_resolution`], appended beside the aggregation probes), so a
    /// slow or misconfigured resolver never multiplies its timeouts
    /// across the `--fix` fixpoint rounds — staged `facts.dns` rides
    /// along untouched.
    #[must_use]
    pub fn gather(config_dir: PathBuf, mut facts: PlatformFacts) -> Self {
        let mode = if facts.units.is_empty() {
            Mode::ConfigOnly
        } else {
            Mode::Packaged
        };
        let scans: Vec<ServiceScan> = catalog::catalog()
            .iter()
            .map(|entry| scan::scan_service(&config_dir, entry))
            .collect();
        let hardware = facts.hardware.take().or_else(|| {
            facts.probe_hardware.then(|| {
                rusty_photon_doctor_checks::gather(&crate::hardware::probe_request(&scans, &facts))
            })
        });
        Self {
            config_dir,
            facts,
            mode,
            scans,
            hardware,
        }
    }

    fn scan(&self, name: &str) -> Option<&ServiceScan> {
        self.scans.iter().find(|s| s.entry.name == name)
    }

    /// A service takes part in diagnosis when its unit is installed or its
    /// config file exists.
    fn participates(&self, scan: &ServiceScan) -> bool {
        scan.config_present() || self.installed(scan.entry)
    }

    /// The participating service names — the "installed set" the
    /// provisioning pass and `doctor tls issue` issue certificates for.
    #[must_use]
    pub fn installed_services(&self) -> Vec<String> {
        self.scans
            .iter()
            .filter(|s| self.participates(s))
            .map(|s| s.entry.name.to_string())
            .collect()
    }

    fn installed(&self, entry: &CatalogEntry) -> bool {
        self.facts.unit(&entry.unit_name()).is_some()
    }
}

/// Run every check.
#[must_use]
pub fn run_all(ctx: &Context) -> Vec<Check> {
    let mut checks = Vec::new();
    checks.extend(inventory(ctx));
    checks.extend(config_parsing(ctx));
    checks.extend(ports(ctx));
    checks.extend(units_and_privileges(ctx));
    checks.extend(failed_units(ctx));
    checks.extend(name_joins(ctx));
    checks.extend(url_conventions(ctx));
    checks.extend(tls_and_auth(ctx));
    checks.extend(pki_ownership(ctx));
    checks.extend(client_target_joins(ctx));
    checks.extend(fake_mount_join(ctx));
    checks.extend(acme_convergence(ctx));
    checks.extend(rp_platform_defaults(ctx));
    checks.extend(crate::hardware::checks(ctx));
    checks
}

fn svc(scan: &ServiceScan) -> String {
    scan.entry.name.to_string()
}

// ---- Inventory (packaged mode only) ----

fn inventory(ctx: &Context) -> Vec<Check> {
    if ctx.mode != Mode::Packaged {
        return Vec::new();
    }
    let mut checks = Vec::new();
    for scan in &ctx.scans {
        let installed = ctx.installed(scan.entry);
        match (installed, scan.config_present()) {
            (true, false) => {
                // A config-gated service (docs/packaging.md) hard-requires a
                // hand-written config — "start it once" would do nothing.
                // `condition_path` is the Linux/systemd fact naming the exact
                // gate file; Windows (start type Manual) and macOS carry no
                // equivalent fact, so `scan.entry.config_gated` is the
                // portable signal every platform can fall back to.
                let systemd_gate = ctx
                    .facts
                    .unit(&scan.entry.unit_name())
                    .and_then(|u| u.condition_path.as_ref());
                let (detail, suggestion) = match systemd_gate {
                    Some(gate) => (
                        format!(
                            "unit {} is installed but {} does not exist — this \
                             service requires a hand-written config (the unit is \
                             gated on {}) and cannot start without one",
                            scan.entry.unit_name(),
                            scan.config_path.display(),
                            gate.display()
                        ),
                        format!(
                            "this service needs a hand-written config — the unit is \
                             gated on {} — so create that file, then start the unit",
                            gate.display()
                        ),
                    ),
                    None if scan.entry.config_gated => (
                        format!(
                            "unit {} is installed but {} does not exist — this \
                             service has no sensible default config and cannot \
                             start without one",
                            scan.entry.unit_name(),
                            scan.config_path.display()
                        ),
                        format!(
                            "this service has no sensible default — create {} by hand, \
                             then start the unit",
                            scan.config_path.display()
                        ),
                    ),
                    None => (
                        format!(
                            "unit {} is installed but {} does not exist — the service \
                             has never started, or writes its config somewhere \
                             unexpected",
                            scan.entry.unit_name(),
                            scan.config_path.display()
                        ),
                        format!(
                            "start it once so it self-creates its defaults: e.g. `{}`",
                            start_command(ctx.facts.platform, &manager_name(ctx, scan.entry))
                        ),
                    ),
                };
                checks.push(Check::warn(
                    "inventory.unit-without-config",
                    Some(svc(scan)),
                    detail,
                    Some(suggestion),
                ));
            }
            (false, true) => checks.push(Check::warn(
                "inventory.config-without-unit",
                Some(svc(scan)),
                format!(
                    "{} exists but no {} unit is installed — a leftover from a \
                     removed package, or a hand-copied stray",
                    scan.config_path.display(),
                    scan.entry.unit_name()
                ),
                None,
            )),
            (true, true) => checks.push(Check::ok(
                "inventory.unit-and-config",
                Some(svc(scan)),
                format!("unit installed and {} present", scan.config_path.display()),
            )),
            (false, false) => {}
        }
    }
    let known: Vec<String> = catalog::catalog()
        .iter()
        .map(super::catalog::CatalogEntry::config_file)
        .collect();
    for name in unknown_config_files(&ctx.config_dir, &known) {
        checks.push(Check::warn(
            "inventory.unknown-config",
            None,
            format!(
                "{name} in {} matches no packaged service — no service will ever \
                 read it",
                ctx.config_dir.display()
            ),
            Some("rename it to a service's <svc>.json, or remove it".to_string()),
        ));
    }
    checks
}

/// The name the service manager itself knows the unit by — the brew
/// nightly channel's formula name when that is what is installed, else the
/// unit stem. Remediation text must name what the operator can type.
fn manager_name(ctx: &Context, entry: &CatalogEntry) -> String {
    ctx.facts
        .unit(&entry.unit_name())
        .and_then(|u| u.source_name.clone())
        .unwrap_or_else(|| entry.unit_name())
}

/// The platform's way to start a service once, for suggestion text.
fn start_command(platform: Platform, unit: &str) -> String {
    match platform {
        Platform::Linux => format!("systemctl start {unit}"),
        Platform::Windows => format!("Start-Service {unit}"),
        Platform::Macos => format!("brew services start {unit}"),
    }
}

// ---- Config parsing ----

fn config_parsing(ctx: &Context) -> Vec<Check> {
    let mut checks = Vec::new();
    for scan in ctx.scans.iter().filter(|s| ctx.participates(s)) {
        match &scan.raw {
            None => continue,
            Some(Err(scan::ReadError::InvalidJson(e))) => {
                checks.push(Check::fail(
                    "config.json-syntax",
                    Some(svc(scan)),
                    format!(
                        "{} is not valid JSON ({e}) — the service will refuse to \
                         start rather than silently reset it",
                        scan.config_path.display()
                    ),
                    Some("fix the JSON by hand; every field is preserved on disk".to_string()),
                ));
                continue;
            }
            Some(Err(scan::ReadError::Unreadable(e))) => {
                checks.push(Check::fail(
                    "config.unreadable",
                    Some(svc(scan)),
                    format!("{} could not be read: {e}", scan.config_path.display()),
                    Some(
                        "fix the file's permissions or ownership — the service user \
                         must be able to read and rewrite it"
                            .to_string(),
                    ),
                ));
                continue;
            }
            Some(Ok(_)) => {}
        }
        match &scan.server {
            ServerBlock::Invalid(e) => {
                checks.push(Check::fail(
                    "config.server-shape",
                    Some(svc(scan)),
                    format!(
                        "the server block in {} does not parse ({e}) — the service \
                         will refuse to start",
                        scan.config_path.display()
                    ),
                    None,
                ));
                // Every check that reads `server.tls` / `server.auth` needs a
                // parsed block and self-limits without one. Silence there
                // would read as a clean bill of health for the one service
                // whose config doctor understands least, so the cost is
                // stated rather than left to be inferred from absent rows.
                checks.push(Check::warn(
                    "config.checks-skipped",
                    Some(svc(scan)),
                    format!(
                        "{} was not diagnosed for TLS or auth — tls.absent, \
                         auth.absent, tls.paths, tls.expiry, tls.auth-without-tls, \
                         auth.mismatch, every client-target join that resolves \
                         here and (on an ACME install) tls.stale-selfsigned-pointer \
                         and rp.advertised-url need a parsed server block",
                        scan.entry.name
                    ),
                    Some(
                        "fix the server block (config.server-shape names the error) \
                         and re-run; `doctor --fix` will not provision into a block \
                         it cannot parse"
                            .to_string(),
                    ),
                ));
            }
            ServerBlock::Parsed { .. } | ServerBlock::BlockAbsent => checks.push(Check::ok(
                "config.server-shape",
                Some(svc(scan)),
                match &scan.server {
                    ServerBlock::BlockAbsent => {
                        format!(
                            "no server block — defaults apply (port {})",
                            scan.entry.default_port
                        )
                    }
                    _ => format!("server block parses (port {})", scan.effective_port()),
                },
            )),
            ServerBlock::FileAbsent => {}
        }
        checks.extend(known_blocks(scan));
    }
    checks
}

/// The known cross-reference blocks must parse for the join checks to see
/// them; a shape error there is its own diagnosis.
fn known_blocks(scan: &ServiceScan) -> Vec<Check> {
    let result = match scan.entry.name {
        "sentinel" => scan::view::<SentinelView>(scan).map(|r| r.map(|_| ())),
        "rp" => scan::view::<RpView>(scan).map(|r| r.map(|_| ())),
        "ui-htmx" => scan::view::<UiHtmxView>(scan).map(|r| r.map(|_| ())),
        _ => None,
    };
    match result {
        Some(Err(e)) => vec![Check::fail(
            "config.known-blocks",
            Some(svc(scan)),
            format!(
                "a cross-reference block in {} does not parse: {e}",
                scan.config_path.display()
            ),
            None,
        )],
        _ => Vec::new(),
    }
}

// ---- Ports ----

fn ports(ctx: &Context) -> Vec<Check> {
    let mut checks = Vec::new();
    let participants: Vec<&ServiceScan> =
        ctx.scans.iter().filter(|s| ctx.participates(s)).collect();

    let mut by_port: BTreeMap<u16, Vec<&ServiceScan>> = BTreeMap::new();
    for scan in &participants {
        by_port.entry(scan.effective_port()).or_default().push(scan);
    }
    // Ports a fix may not move a service onto: every effective port in use,
    // plus every default a fix already claimed this round.
    let mut claimed: std::collections::BTreeSet<u16> = by_port.keys().copied().collect();
    let mut collided = false;
    for (port, scans) in &by_port {
        // The pattern is the `len() > 1` collision guard: it binds the
        // first member only when at least two services claim the port.
        if let [first, _, ..] = scans.as_slice() {
            collided = true;
            let members = scans
                .iter()
                .map(|s| {
                    let source = match &s.server {
                        ServerBlock::Parsed { .. } => "configured",
                        _ => "default",
                    };
                    format!("{} ({source})", s.entry.name)
                })
                .collect::<Vec<_>>()
                .join(", ");
            // The derivable repair: a configured member whose own catalog
            // default is free goes back to it. A member already at its
            // default, or whose default is taken, is a judgment call — the
            // suggestion text covers it.
            let mut fixes = Vec::new();
            for scan in scans {
                let configured = matches!(&scan.server, ServerBlock::Parsed { .. });
                let default = scan.entry.default_port;
                if configured && default != *port && !claimed.contains(&default) {
                    claimed.insert(default);
                    fixes.push(crate::report::FixOp::SetNumber {
                        service: scan.entry.name.to_string(),
                        pointer: "/server/port".to_string(),
                        value: u64::from(default),
                    });
                }
            }
            checks.push(
                Check::fail(
                    "ports.collision",
                    svc(first),
                    format!(
                        "port {port} is claimed by {} services — {members} — and only \
                         one can bind",
                        scans.len()
                    ),
                    Some(format!(
                        "give each a distinct server.port (defaults: {})",
                        scans
                            .iter()
                            .map(|s| format!("{} {}", s.entry.name, s.entry.default_port))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )),
                )
                .with_fixes(fixes),
            );
        }
    }
    if !collided && !participants.is_empty() {
        let n = by_port.len();
        checks.push(Check::ok(
            "ports.collision",
            None,
            format!(
                "{n} effective port{}, all distinct",
                if n == 1 { "" } else { "s" }
            ),
        ));
    }

    let mut by_discovery: BTreeMap<u16, Vec<&ServiceScan>> = BTreeMap::new();
    for scan in &participants {
        if let Some(port) = scan.discovery_port() {
            by_discovery.entry(port).or_default().push(scan);
        }
    }
    for (port, scans) in &by_discovery {
        // Same shape as the port-collision guard above: at least two
        // responders on the port, with the first bound for attribution.
        if let [first, _, ..] = scans.as_slice() {
            let members = scans
                .iter()
                .map(|s| s.entry.name)
                .collect::<Vec<_>>()
                .join(", ");
            checks.push(Check::fail(
                "ports.discovery-collision",
                svc(first),
                format!(
                    "discovery_port {port} is enabled by {members} — UDP responders \
                     collide; discovery is a per-host opt-in for one driver"
                ),
                Some("remove discovery_port from all but one config".to_string()),
            ));
        }
    }
    checks
}

// ---- Units and privileges (systemd facts) ----

/// The renewal one-shot's unit stem. Sentinel's discovery skips it — a job
/// is not a daemon, and supervising it would restart-loop a failed 3am run
/// — which leaves doctor as the only thing that ever looks at it.
const RENEW_UNIT: &str = "rusty-photon-renew";

/// `units.failed`: the service manager is holding a unit in a failed
/// state. A crashed daemon eventually shows up as a service nobody can
/// reach; a failed **one-shot** shows up as nothing at all — it simply
/// stops doing its job, silently, until someone runs `systemctl
/// list-units` for unrelated reasons. Suggestion-only: doctor starts and
/// resets no units.
fn failed_units(ctx: &Context) -> Vec<Check> {
    let judged: Vec<_> = ctx
        .facts
        .units
        .iter()
        .filter(|unit| unit.failed.is_some())
        .collect();
    if judged.is_empty() {
        // Windows, or a staged scenario with no failure story.
        return Vec::new();
    }
    let failed: Vec<_> = judged
        .iter()
        .filter(|unit| unit.failed == Some(true))
        .collect();
    if failed.is_empty() {
        return vec![Check::ok(
            "units.failed",
            None,
            format!("none of the {} installed units has failed", judged.len()),
        )];
    }
    failed
        .iter()
        .map(|unit| {
            let name = unit.source_name.as_deref().unwrap_or(&unit.name);
            let consequence = if unit.name == RENEW_UNIT {
                " — no certificate is being renewed while it stays that way, which \
                 on an ACME install means every service loses TLS within 90 days"
            } else {
                ""
            };
            Check::fail(
                "units.failed",
                catalog::entry_for_unit(&unit.name).map(|entry| entry.name.to_string()),
                format!(
                    "{name} is in a failed state and stays there until something \
                     clears it{consequence}"
                ),
                Some(failure_suggestion(ctx, name)),
            )
        })
        .collect()
}

fn failure_suggestion(ctx: &Context, unit: &str) -> String {
    match ctx.facts.platform {
        Platform::Macos => format!(
            "`brew services info {unit}` and the service's log say why; \
             `brew services restart {unit}` re-runs it"
        ),
        _ => format!(
            "`journalctl -u {unit} -e` says why; fix that, then `systemctl start \
             {unit}` to re-run a one-shot (or `systemctl reset-failed {unit}` to \
             clear the state for a daemon the timer or a dependency will start)"
        ),
    }
}

fn units_and_privileges(ctx: &Context) -> Vec<Check> {
    let mut checks = Vec::new();
    if ctx.facts.platform != Platform::Linux {
        return checks;
    }
    for unit in &ctx.facts.units {
        let Some(entry) = catalog::entry_for_unit(&unit.name) else {
            continue;
        };
        let Some(path) = &unit.condition_path else {
            continue;
        };
        if !unit.enabled {
            continue;
        }
        if path.exists() {
            checks.push(Check::ok(
                "units.config-gated",
                Some(entry.name.to_string()),
                format!("{} gate {} is satisfied", unit.name, path.display()),
            ));
        } else {
            checks.push(Check::fail(
                "units.config-gated",
                Some(entry.name.to_string()),
                format!(
                    "{} is enabled but its ConditionPathExists gate {} is missing — \
                     installed, enabled, and silently inert",
                    unit.name,
                    path.display()
                ),
                Some(format!(
                    "create {} (the service needs a hand-written config) and start \
                     the unit",
                    path.display()
                )),
            ));
        }
    }
    if ctx.facts.unit("rusty-photon-sentinel").is_some() {
        match ctx.facts.polkit_grants_sentinel_restart {
            Some(true) => checks.push(Check::ok(
                "sentinel.privilege-path",
                Some("sentinel".to_string()),
                "a polkit rule grants sentinel's user manage-units for \
                 rusty-photon-* units"
                    .to_string(),
            )),
            Some(false) => checks.push(Check::fail(
                "sentinel.privilege-path",
                Some("sentinel".to_string()),
                "no polkit rule granting the rusty-photon user \
                 org.freedesktop.systemd1.manage-units for rusty-photon-* units \
                 was found (heuristic scan of the polkit rules directories) — the \
                 packaged sentinel unit runs unprivileged with \
                 NoNewPrivileges=yes, so every restart it attempts will be denied"
                    .to_string(),
                Some(
                    "install the packaged rule (shipped with the sentinel deb/rpm \
                     under /usr/share/polkit-1/rules.d/) or add one under \
                     /etc/polkit-1/rules.d/"
                        .to_string(),
                ),
            )),
            None => {}
        }
    }
    checks
}

// ---- Name joins ----

fn name_joins(ctx: &Context) -> Vec<Check> {
    let mut checks = Vec::new();
    let sentinel_view: Option<SentinelView> =
        ctx.scan("sentinel").and_then(|s| scan::view(s)?.ok());
    let ui_view: Option<UiHtmxView> = ctx.scan("ui-htmx").and_then(|s| scan::view(s)?.ok());

    checks.extend(retired_keys(sentinel_view.as_ref(), ui_view.as_ref()));
    if let Some(sentinel) = &sentinel_view {
        checks.extend(watchdog_joins(ctx, sentinel));
    }
    checks
}

/// Config keys retired by D3s and #569: sentinel's `services` map (sentinel
/// discovers its services) and ui-htmx's whole `drivers` override map (rp's
/// roster is the only device source). Both fail the service's own strict
/// load, so the file is dead weight that keeps the service from starting.
fn retired_keys(sentinel: Option<&SentinelView>, ui: Option<&UiHtmxView>) -> Vec<Check> {
    let mut checks = Vec::new();
    if sentinel.is_some_and(|s| s.services.is_some()) {
        checks.push(
            Check::fail(
                "config.retired-keys",
                Some("sentinel".to_string()),
                "sentinel.json carries the retired services map — sentinel discovers \
                 its services from the platform service manager now, and refuses to \
                 start while the key is present"
                    .to_string(),
                Some(
                    "delete the top-level \"services\" key; supervision needs no \
                     replacement config"
                        .to_string(),
                ),
            )
            .with_fixes(vec![crate::report::FixOp::RemoveKey {
                service: "sentinel".to_string(),
                pointer: "/services".to_string(),
            }]),
        );
    }
    if ui.is_some_and(|u| u.drivers.is_some()) {
        checks.push(
            Check::fail(
                "config.retired-keys",
                Some("ui-htmx".to_string()),
                "ui-htmx.json carries the retired drivers override map — rp's \
                 equipment roster is the only device source now, and ui-htmx \
                 refuses to start while the key is present"
                    .to_string(),
                Some(
                    "delete the top-level \"drivers\" key; devices belong in rp's \
                     equipment roster"
                        .to_string(),
                ),
            )
            .with_fixes(vec![crate::report::FixOp::RemoveKey {
                service: "ui-htmx".to_string(),
                pointer: "/drivers".to_string(),
            }]),
        );
    }
    checks
}

/// The installed rusty-photon units' service names (unit minus the prefix) —
/// what sentinel's discovery will resolve restart names against.
fn discovered_service_names(ctx: &Context) -> Vec<String> {
    ctx.facts
        .units
        .iter()
        .filter_map(|u| u.name.strip_prefix("rusty-photon-"))
        .map(str::to_string)
        .collect()
}

fn watchdog_joins(ctx: &Context, sentinel: &SentinelView) -> Vec<Check> {
    let mut checks = Vec::new();
    if ctx.mode != Mode::Packaged {
        return checks;
    }
    let Some(watchdog) = &sentinel.operation_watchdog else {
        return checks;
    };
    let discovered = discovered_service_names(ctx);
    for (family, operation) in &watchdog.operations {
        let Some(service) = &operation.service else {
            continue;
        };
        if !discovered.iter().any(|name| name == service) {
            checks.push(Check::fail(
                "joins.watchdog-service",
                Some("sentinel".to_string()),
                format!(
                    "operation_watchdog.operations.{family}.service names \
                     \"{service}\", but no rusty-photon-{service} unit is installed \
                     — sentinel's discovery will never resolve it, so the \
                     watchdog's ladder degrades to notify-only"
                ),
                Some(format!("installed services are: {}", discovered.join(", "))),
            ));
        }
    }
    checks
}

// ---- URL conventions ----

fn url_conventions(ctx: &Context) -> Vec<Check> {
    let mut checks = Vec::new();
    let carries_suffix = |url: &str| url.trim_end_matches('/').ends_with("/api/v1");
    let stripped = |url: &str| {
        url.trim_end_matches('/')
            .trim_end_matches("/api/v1")
            .to_string()
    };
    let spurious = |service: &str, field: String, url: &str| {
        Check::warn(
            "urls.spurious-suffix",
            Some(service.to_string()),
            format!(
                "{field} ({url}) carries /api/v1, but this client appends it \
                 itself — requests would double the prefix and 404"
            ),
            Some(format!("use {}", stripped(url))),
        )
    };
    // rp's alpaca_url lives inside the device-usage block doctor checks but
    // does not own (ADR-016 decision 4): suggestion only, never a fix.
    if let Some(rp) = ctx.scan("rp").and_then(|s| scan::view::<RpView>(s)?.ok()) {
        for url in rp.alpaca_urls() {
            if carries_suffix(&url) {
                checks.push(spurious("rp", "an equipment alpaca_url".to_string(), &url));
            }
        }
    }
    checks
}

// ---- TLS and auth ----

/// `tls.ownership`: everything the install reads must belong to the config
/// root's owner. The provisioning pass and every renewal end by aligning
/// the tree (docs/services/doctor.md §Ownership under sudo), so a
/// mismatch means a run that could not — an unprivileged doctor next to
/// material an earlier `sudo` left behind. Material a service reads fails
/// (a key it does not own is a handshake that never happens); anything
/// else in the tree warns, because renewal skips it too. Unix only:
/// Windows and a config root doctor cannot stat have no owner to compare.
fn pki_ownership(ctx: &Context) -> Vec<Check> {
    let Some(ownership) = crate::provision::pki_ownership(&ctx.config_dir) else {
        return Vec::new();
    };
    ownership_checks(ctx, &ownership)
}

/// The judgment half of `tls.ownership`, over an already-gathered tree —
/// which is what makes both the fail and the warn arm reachable without
/// the privileges it takes to create a cross-owned file.
fn ownership_checks(ctx: &Context, ownership: &crate::provision::PkiOwnership) -> Vec<Check> {
    if ownership.examined == 0 {
        return Vec::new();
    }
    let (uid, gid) = (ownership.uid, ownership.gid);
    let (material, strays): (Vec<_>, Vec<_>) = ownership
        .mismatched
        .iter()
        .partition(|entry| entry.essential);
    if material.is_empty() && strays.is_empty() {
        return vec![Check::ok(
            "tls.ownership",
            None,
            format!(
                "all {} pki entries belong to the config root's owner (uid {uid}, \
                 gid {gid})",
                ownership.examined
            ),
        )];
    }

    let mut checks = Vec::new();
    if !material.is_empty() {
        checks.push(Check::fail(
            "tls.ownership",
            None,
            format!(
                "material the services read does not belong to the config root's \
                 owner (uid {uid}, gid {gid}): {} — a service cannot read a key it \
                 does not own, so its next start serves no TLS, and the renewal \
                 timer (which runs as that user) cannot renew it either",
                relative_names(ctx, &material)
            ),
            Some(format!(
                "run `sudo rusty-photon-doctor --fix` — its provisioning pass hands \
                 the tree over — or `sudo chown {uid}:{gid} <path>` per entry"
            )),
        ));
    }
    if !strays.is_empty() {
        checks.push(Check::warn(
            "tls.ownership",
            None,
            format!(
                "the pki tree also holds entries that belong to someone else and \
                 that nothing reads (uid {uid}, gid {gid} expected): {} — renewal \
                 leaves them alone rather than failing over them, so they are \
                 reported here instead",
                relative_names(ctx, &strays)
            ),
            Some(format!(
                "`sudo chown {uid}:{gid} <path>` if they belong to the install, or \
                 delete them; `ca.srl` is openssl's serial counter from a \
                 hand-minted certificate and is safe to remove"
            )),
        ));
    }
    checks
}

/// Mismatching entries as config-root-relative paths, capped so a wholly
/// foreign-owned tree names a few files instead of forty.
fn relative_names(ctx: &Context, entries: &[&crate::provision::OwnershipMismatch]) -> String {
    const SHOWN: usize = 5;
    let mut names: Vec<String> = entries
        .iter()
        .take(SHOWN)
        .map(|entry| {
            entry
                .path
                .strip_prefix(&ctx.config_dir)
                .unwrap_or(&entry.path)
                .display()
                .to_string()
        })
        .collect();
    let more = entries.len().saturating_sub(SHOWN);
    if more > 0 {
        names.push(format!("and {more} more"));
    }
    names.join(", ")
}

fn tls_and_auth(ctx: &Context) -> Vec<Check> {
    let mut checks = Vec::new();
    for scan in ctx.scans.iter().filter(|s| ctx.participates(s)) {
        checks.extend(tls_auth_absent(ctx, scan));
        let Some(server) = scan.server() else {
            continue;
        };
        if let Some(tls) = &server.tls {
            // Per path: empty or absent-on-disk is a failure; a relative
            // path is ungradable — the service resolves it against its own
            // working directory (`TlsConfig::resolved_*_path` only expands
            // `~`), which doctor cannot know, so claiming presence either
            // way would be a guess.
            let mut missing: Vec<String> = Vec::new();
            let mut relative: Vec<String> = Vec::new();
            for (raw, resolved) in [
                (&tls.cert, tls.resolved_cert_path()),
                (&tls.key, tls.resolved_key_path()),
            ] {
                if raw.trim().is_empty() {
                    missing.push("<empty path>".to_string());
                } else if !resolved.is_absolute() {
                    relative.push(raw.clone());
                } else if !resolved.is_file() {
                    missing.push(raw.clone());
                }
            }
            if !missing.is_empty() {
                checks.push(Check::fail(
                    "tls.paths",
                    Some(svc(scan)),
                    format!(
                        "server.tls points at missing material: {} — the service \
                         will refuse to serve at next start",
                        missing.join(", ")
                    ),
                    Some(
                        "generate certs (`doctor tls issue`, or `doctor --fix` to also \
                         wire the config) or fix the paths"
                            .to_string(),
                    ),
                ));
            } else if !relative.is_empty() {
                checks.push(Check::warn(
                    "tls.paths",
                    Some(svc(scan)),
                    format!(
                        "server.tls uses relative paths ({}): the service resolves \
                         them against its own working directory, which doctor \
                         cannot know, so the material cannot be judged",
                        relative.join(", ")
                    ),
                    Some(
                        "use absolute paths — doctor-issued material always is, and \
                         `doctor --fix` writes absolute paths"
                            .to_string(),
                    ),
                ));
            } else {
                checks.push(Check::ok(
                    "tls.paths",
                    Some(svc(scan)),
                    "TLS cert and key exist".to_string(),
                ));
            }
            // Expiry is judged only when tls.paths is clean — a missing or
            // ungradable pair stays tls.paths' concern, and an expiry
            // verdict beside a failing pair would read as contradictory.
            let cert_file = tls.resolved_cert_path();
            if missing.is_empty() && relative.is_empty() {
                checks.push(tls_expiry(ctx, scan, &cert_file));
            }
        }
        if server.auth.is_some() && server.tls.is_none() {
            checks.push(Check::warn(
                "tls.auth-without-tls",
                Some(svc(scan)),
                "server.auth without server.tls sends HTTP Basic credentials in \
                 cleartext on the wire"
                    .to_string(),
                Some("add a server.tls block (ADR-003: Basic auth over TLS)".to_string()),
            ));
        }
    }
    checks.extend(auth_mismatch(ctx));
    checks
}

/// The D6a absent checks: an installed service without a `server.tls` /
/// `server.auth` block serves plain, unauthenticated HTTP. Legal (absent
/// means off — ADR-016 decision 10(d)) and fixable: each check plans the
/// whole-block write the provisioning pass applies. The `auth` plan needs
/// the observatory credential, so it appears only once `pki/credential`
/// exists (under `--fix` the material pass runs first).
fn tls_auth_absent(ctx: &Context, scan: &ServiceScan) -> Vec<Check> {
    if matches!(scan.server, ServerBlock::FileAbsent) {
        // No config file at all — never started, or one that existed was
        // removed. Doctor cannot tell which, and either way there is no
        // file for provisioning to write into (unlike the cases below);
        // see `tls_auth_file_absent`.
        return tls_auth_file_absent(scan);
    }
    if scan.value().is_none() {
        // Unreadable or invalid JSON: the read-level checks own the
        // diagnosis, and provisioning has nothing to write into.
        return Vec::new();
    }
    let (tls_absent, auth_absent, server_key_present) = match &scan.server {
        ServerBlock::Parsed { server, .. } => (server.tls.is_none(), server.auth.is_none(), true),
        // Valid JSON without a server key: the service applies its plain
        // HTTP defaults, so both blocks are absent.
        ServerBlock::BlockAbsent => (true, true, false),
        // An unparseable block is config.server-shape's diagnosis; writing
        // into it would be guesswork — what that costs is reported once,
        // as `config.checks-skipped`, beside that failure. FileAbsent is
        // handled by the early return above and kept in the match so it
        // stays exhaustive if that return is ever removed.
        ServerBlock::Invalid(_) | ServerBlock::FileAbsent => return Vec::new(),
    };
    let name = scan.entry.name;
    let mut checks = Vec::new();
    if tls_absent {
        // On an ACME install the fix points at the shared wildcard pair —
        // never at freshly issued self-signed material, which the flipped
        // fleet's clients could not verify (issue #616). While the pair is
        // missing, conjuring it is renewal's job, so no fix is planned.
        let acme = crate::provision::acme_active(&ctx.config_dir);
        let tls_value = if acme {
            crate::provision::acme_tls_block_value(&ctx.config_dir)
        } else {
            Some(crate::provision::tls_block_value(&ctx.config_dir, name))
        };
        let fixes = match tls_value {
            Some(tls_value) if server_key_present => vec![crate::report::FixOp::SetObject {
                service: name.to_string(),
                pointer: "/server/tls".to_string(),
                value: tls_value,
            }],
            // No server key at all: the block is created whole, keeping the
            // port the service would have defaulted to.
            Some(tls_value) => vec![crate::report::FixOp::SetObject {
                service: name.to_string(),
                pointer: "/server".to_string(),
                value: serde_json::json!({ "port": scan.entry.default_port, "tls": tls_value }),
            }],
            None => Vec::new(),
        };
        let suggestion = if fixes.is_empty() {
            "this is an ACME install (acme.json present) but the wildcard pair is \
             missing — run `doctor tls renew` to obtain it, then `doctor --fix` to \
             wire the config"
        } else if acme {
            "run `doctor --fix` to point server.tls at the ACME wildcard pair \
             (services pick it up at next restart)"
        } else {
            "run `doctor --fix` to issue a certificate and turn TLS on \
             (services pick it up at next restart)"
        };
        checks.push(
            Check::warn(
                "tls.absent",
                Some(svc(scan)),
                format!("{name} has no server.tls block — it serves plain HTTP"),
                Some(suggestion.to_string()),
            )
            .with_fixes(fixes),
        );
    }
    if auth_absent {
        let fixes = plan_auth_block(ctx).map_or_else(Vec::new, |value| {
            vec![crate::report::FixOp::SetObject {
                service: name.to_string(),
                pointer: "/server/auth".to_string(),
                value,
            }]
        });
        checks.push(
            Check::warn(
                "auth.absent",
                Some(svc(scan)),
                format!("{name} has no server.auth block — it answers unauthenticated"),
                Some(
                    "run `doctor --fix` to mint the observatory credential and turn \
                     auth on (services pick it up at next restart)"
                        .to_string(),
                ),
            )
            .with_fixes(fixes),
        );
    }
    checks
}

/// `tls.absent`/`auth.absent` for a self-defaulting installed service with
/// no config file on disk. Doctor cannot tell whether the service has
/// simply never started, or a config that once existed was deleted — either
/// way, the next (re)start serves plain, unauthenticated HTTP, and that is
/// worth surfacing loudly rather than leaving to the generic
/// `inventory.unit-without-config` "never started" reading, which does not
/// say so.
///
/// Config-gated services (`scan.entry.config_gated`) stay out: they hard-
/// require an operator-written file and cannot start without one, so they
/// never reach plain HTTP this way — `inventory.unit-without-config` /
/// `units.config-gated` already own that story.
///
/// Unfixable, unlike the sibling checks above: `--fix` has no file to write
/// a `server.tls`/`server.auth` block into, so no `FixOp` is planned — the
/// suggestion is to hand-write the config, or start the service once so it
/// self-creates one, then re-run `doctor --fix`.
fn tls_auth_file_absent(scan: &ServiceScan) -> Vec<Check> {
    if scan.entry.config_gated {
        return Vec::new();
    }
    let name = scan.entry.name;
    let path = scan.config_path.display();
    let remedy = format!(
        "no {path} to provision — create it yourself (an empty `{{}}` is enough) \
         or start the service once so it self-creates defaults, then run \
         `doctor --fix` to provision server.tls/server.auth into it"
    );
    vec![
        Check::warn(
            "tls.absent",
            Some(svc(scan)),
            format!(
                "{name} has no config file at {path} — it will serve plain HTTP \
                 the next time it starts, whether it has never started or its \
                 config was removed"
            ),
            Some(remedy.clone()),
        ),
        Check::warn(
            "auth.absent",
            Some(svc(scan)),
            format!(
                "{name} has no config file at {path} — it will answer \
                 unauthenticated the next time it starts, whether it has never \
                 started or its config was removed"
            ),
            Some(remedy),
        ),
    ]
}

/// The `server.auth` block value for one service: the observatory username
/// and a fresh Argon2id hash of the minted credential. `None` until the
/// credential exists.
fn plan_auth_block(ctx: &Context) -> Option<serde_json::Value> {
    let password = crate::provision::read_credential(&ctx.config_dir)?;
    match rp_auth::credentials::hash_password(&password) {
        Ok(hash) => Some(serde_json::json!({
            "username": crate::provision::CREDENTIAL_USERNAME,
            "password_hash": hash,
        })),
        Err(e) => {
            tracing::warn!("could not hash the observatory credential: {e}");
            None
        }
    }
}

/// `auth.mismatch`: sentinel's `service_auth` plaintext must verify
/// (Argon2id) against each installed service's `server.auth` hash, or its
/// authenticated probes will 401. Suggestion-only — hand-set credentials
/// are operator intent, so doctor reports the pair and points at
/// `doctor auth rotate`.
fn auth_mismatch(ctx: &Context) -> Vec<Check> {
    let mut checks = Vec::new();
    let Some(sentinel_scan) = ctx.scan("sentinel").filter(|s| ctx.participates(s)) else {
        return checks;
    };
    let Some(sentinel) = scan::view::<SentinelView>(sentinel_scan).and_then(Result::ok) else {
        return checks;
    };
    let Some(client) = sentinel.service_auth else {
        return checks;
    };
    let (Some(username), Some(password)) = (client.username, client.password) else {
        return checks;
    };
    for scan in ctx.scans.iter().filter(|s| ctx.participates(s)) {
        if scan.entry.name == "sentinel" {
            // service_auth is for the supervised peers; sentinel does not
            // probe itself.
            continue;
        }
        let Some(auth) = scan.server().and_then(|s| s.auth.as_ref()) else {
            continue;
        };
        let username_matches = auth.username == username;
        if username_matches && rp_auth::credentials::verify_password(&password, &auth.password_hash)
        {
            continue;
        }
        let what = if username_matches {
            "password does not verify against"
        } else {
            "username does not match"
        };
        checks.push(Check::warn(
            "auth.mismatch",
            Some(svc(scan)),
            format!(
                "sentinel's service_auth {what} {}'s server.auth — its \
                 authenticated probes will get 401s",
                scan.entry.name
            ),
            Some(
                "run `doctor auth rotate` to re-mint the observatory credential \
                 and re-align every copy, or fix the pair by hand"
                    .to_string(),
            ),
        ));
    }
    checks
}

/// `tls.expiry` (D6b): grade an existing configured certificate's
/// `not_after`. Expired or unparseable fails — rustls loads an expired
/// certificate cleanly and only *clients* reject the handshake, so without
/// this check the failure surfaces as every client erroring at night.
/// Inside the renewal window warns. Suggestion-only: renewal belongs on
/// the platform timer, so `--fix` never renews.
fn tls_expiry(ctx: &Context, scan: &ServiceScan, cert_file: &Path) -> Check {
    let suggestion = "run `doctor tls renew` (the platform timer's command) for \
                      doctor-issued material (`doctor tls issue --force` re-issues \
                      it with fresh SANs); the ACME wildcard renews only while \
                      `acme.json` still sits beside the configs — re-run `doctor \
                      tls issue --acme` if it is gone; a certificate doctor did \
                      not issue must be replaced by whatever issued it"
        .to_string();
    let pem = match std::fs::read_to_string(cert_file) {
        Ok(pem) => pem,
        Err(e) => {
            return Check::fail(
                "tls.expiry",
                Some(svc(scan)),
                format!("{} could not be read: {e}", cert_file.display()),
                Some(suggestion),
            )
        }
    };
    let not_after = match crate::provision::expiry::not_after(&pem) {
        Ok(not_after) => not_after,
        Err(e) => {
            return Check::fail(
                "tls.expiry",
                Some(svc(scan)),
                format!(
                    "{} is not a parseable certificate ({e}) — the service \
                     cannot serve it",
                    cert_file.display()
                ),
                Some(suggestion),
            )
        }
    };
    let now = time::OffsetDateTime::now_utc();
    if not_after <= now {
        return Check::fail(
            "tls.expiry",
            Some(svc(scan)),
            format!(
                "{} expired {not_after} — the server still loads it, and every \
                 client rejects the handshake",
                cert_file.display()
            ),
            Some(suggestion),
        );
    }
    let window_days = expiry_window_days(ctx, cert_file);
    // `not_after - now <= window` rearranged through `checked_sub` so the
    // subtraction is total: an underflowing window start means the window
    // opened before representable time — certainly inside it.
    if not_after
        .checked_sub(time::Duration::days(window_days))
        .is_none_or(|window_start| window_start <= now)
    {
        return Check::warn(
            "tls.expiry",
            Some(svc(scan)),
            format!(
                "{} expires {not_after}, inside its {window_days}-day renewal \
                 window",
                cert_file.display()
            ),
            Some(suggestion),
        );
    }
    Check::ok(
        "tls.expiry",
        Some(svc(scan)),
        format!("certificate valid until {not_after}"),
    )
}

/// The warn window: 30 days for self-signed material,
/// `renewal_days_before_expiry` from `acme.json` for the ACME wildcard
/// pair.
fn expiry_window_days(ctx: &Context, cert_file: &Path) -> i64 {
    if cert_file.file_name().is_some_and(|n| n == "acme-cert.pem") {
        if let Ok(config) =
            crate::provision::acme_config::load_acme_config(&ctx.config_dir.join("acme.json"))
        {
            return i64::from(config.renewal_days_before_expiry);
        }
    }
    30
}

// ---- Client-target joins ----
//
// A client's config points a URL (or, for sentinel's monitors, a
// scheme/host/port triple) at another catalog service. These checks join
// that URL against the *named* service's own `server.tls`/`server.auth` —
// the gap #607 named: provisioning upgrades a service's server side, but
// nothing told doctor to look at who points at it.

/// Doctor diagnoses one config directory, so a client→target join only
/// resolves when the URL's host names *this* machine — a different host
/// names a service in a config file doctor cannot see. This covers every
/// client target's shipped default (all loopback). Compared
/// ASCII-case-insensitively: DNS names are case-insensitive, and while a
/// URL host arrives lowercased by the parser, a monitor's discrete
/// `host` field reaches this check exactly as the config spells it.
fn is_loopback_host(host: &str) -> bool {
    ["127.0.0.1", "localhost", "::1"]
        .iter()
        .any(|l| host.eq_ignore_ascii_case(l))
}

/// The one local, participating catalog service a client's `host:port`
/// names, or `None` when the host is not this machine, no service claims
/// the port, the service's own block does not parse or its config file
/// does not exist (`config.server-shape` owns that diagnosis; writing a
/// join verdict against an unreadable or absent block would be
/// guesswork — mirrors `tls_auth_absent`'s identical distinction), or
/// more than one participating service claims the port — an ambiguous
/// join `ports.collision` already reports as its own `fail`, so this
/// self-limits rather than guessing which of the colliding services the
/// client actually meant. A config file that simply omits `server`
/// entirely (`ServerBlock::BlockAbsent`) is not guesswork, though — it is
/// the documented "plain HTTP, no auth, catalog default port" state, so
/// it still resolves.
///
/// On an ACME install one more host shape joins (#805): exactly
/// `<svc>.<domain>`, `domain` read from `acme.json` and `<svc>` the
/// port-matched service's own catalog name — the flip rewrites client
/// URLs onto exactly those names, and the join family must keep judging
/// them (and the `--fix` loop verifying its own rewrites) afterwards. An
/// absent or unreadable `acme.json` keeps the loopback-only shape,
/// mirroring the aggregation probes' domain handling.
pub(crate) fn resolve_join_target<'a>(
    ctx: &'a Context,
    host: &str,
    port: u16,
) -> Option<&'a ServiceScan> {
    let mut matches = ctx.scans.iter().filter(|s| {
        ctx.participates(s)
            && !matches!(s.server, ServerBlock::Invalid(_) | ServerBlock::FileAbsent)
            && s.effective_port() == port
    });
    let target = matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    if is_loopback_host(host) {
        return Some(target);
    }
    let domain = crate::provision::active_acme_config(&ctx.config_dir)?.domain;
    // Case-insensitive for the same reason as `is_loopback_host`: DNS
    // names are, and a monitor's `host` field arrives config-spelled.
    host.eq_ignore_ascii_case(&format!("{}.{domain}", target.entry.name))
        .then_some(target)
}

/// Whether `target`'s configured certificate is the ACME wildcard pair —
/// publicly trusted, so an absent client `ca_cert_path` is not a problem —
/// versus doctor's self-signed CA, which every client must be told to
/// trust explicitly. Mirrors `expiry_window_days`'s file-name convention.
fn target_uses_acme_cert(target: &ServiceScan) -> bool {
    target
        .server()
        .and_then(|s| s.tls.as_ref())
        .is_some_and(|tls| {
            tls.resolved_cert_path()
                .file_name()
                .is_some_and(|n| n == "acme-cert.pem")
        })
}

/// The scheme a target's current TLS state calls for — the single
/// source of truth every scheme-mismatch check and fix compares
/// against, so a garbage/unsupported scheme (e.g. `ftp`) is judged the
/// same way everywhere rather than silently matching by accident.
const fn expected_scheme(target_tls_on: bool) -> &'static str {
    if target_tls_on {
        "https"
    } else {
        "http"
    }
}

/// Parse a client URL into `(scheme, host, port)` — `None` when it does
/// not parse or omits an explicit port (every rusty-photon service URL
/// carries one; a bare default like `https://host/` names nothing in the
/// catalog anyway).
pub(crate) fn parse_target_url(url: &str) -> Option<(String, String, u16)> {
    let parsed = reqwest::Url::parse(url).ok()?;
    let host = parsed.host_str()?.to_string();
    let port = parsed.port()?;
    Some((parsed.scheme().to_string(), host, port))
}

/// Rewrite a URL's scheme, preserving everything after `://` byte-for-byte —
/// the `--fix` value for a client target whose scheme lives inside a full
/// URL string. A parse-and-reserialize round trip (`Url::set_scheme` +
/// `to_string`) would normalize an origin-only URL by appending a trailing
/// `/` (`http://host:port` becomes `http://host:port/`), which several
/// client call sites (e.g. ui-htmx's `sse_proxy`) concatenate a
/// `/`-prefixed path onto without trimming, turning a healthy URL into a
/// double-slashed one that 404s.
fn rewrite_scheme(url: &str, new_scheme: &str) -> Option<String> {
    reqwest::Url::parse(url).ok()?;
    // Split on the literal separator rather than stripping `Url::scheme()`
    // (which the `url` crate lowercases) off the raw string — that would
    // silently fail to strip an input like `HTTP://host:port`.
    let (_, rest) = url.split_once("://")?;
    Some(format!("{new_scheme}://{rest}"))
}

/// Rewrite a URL's host, preserving everything on either side
/// byte-for-byte — the `--fix` value for a loopback client URL against an
/// ACME target, whose wildcard SAN `*.<domain>` can never match a
/// loopback address. Same no-round-trip rationale as [`rewrite_scheme`].
/// `None` when the URL does not parse, omits an explicit port (such a URL
/// never resolves a join in the first place), or its authority does not
/// start with the parsed host exactly as written (e.g. userinfo, or
/// unusual casing) — bailing plans no fix rather than splicing a guess.
fn rewrite_host(url: &str, new_host: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    parsed.port()?;
    let host = parsed.host_str()?;
    let (scheme, rest) = url.split_once("://")?;
    // An IPv6 host serializes bracketed in the URL but `host_str()` may
    // or may not carry the brackets depending on the `url` crate's
    // parse path — accept either spelling of the prefix.
    let after_host = rest
        .strip_prefix(host)
        .or_else(|| rest.strip_prefix(&format!("[{host}]")))?;
    Some(format!("{scheme}://{new_host}{after_host}"))
}

/// Where a client target's address lives in the client's schema — one
/// full URL string, or discrete scheme/host/port fields (sentinel's
/// monitors). The split is what lets [`plan_address`] compose a URL
/// field's scheme and host rewrites into a single written value while
/// fixing discrete fields independently.
enum TargetLocator {
    /// A full URL string field (`https://host:port/path`).
    Url { url: String, pointer: String },
    /// Discrete address fields, parsed out of the client's own schema.
    /// `host_field` is the host field's dotted display name — the URL
    /// shape reuses the transport field's for both legs.
    Parts {
        scheme: String,
        host: String,
        port: u16,
        scheme_pointer: String,
        host_pointer: String,
        host_field: String,
    },
}

/// A client target's credential field: where `joins.client-auth` reads
/// the current value and where its fix writes the observatory credential.
struct AuthTarget {
    /// Dotted display name (`rp.auth`, `monitors[0].auth`, …).
    field: String,
    /// Already-escaped JSON pointer to the credential object.
    pointer: String,
    /// The configured credential, when present.
    current: Option<ClientAuthView>,
}

/// One entry in the client-target registry: a field in `client`'s config
/// that names another catalog service, with the CA-trust and credential
/// pointers the join checks judge and fix. The registry builders
/// ([`client_targets`]) are pure data extraction from the scanned views;
/// [`judge_client_target`] turns each entry into checks, so every
/// current and future client target shares one set of rewrite rules
/// (docs/services/doctor.md §Client-target joins).
struct ClientTarget {
    /// Catalog name of the client service the verdicts file against.
    client: &'static str,
    /// Dotted display name of the pointing field, for check details.
    transport_field: String,
    locator: TargetLocator,
    /// CA-trust field as `(pointer, already_present)` — ui-htmx's
    /// per-target `ca_cert_path`, rp's and sentinel's single top-level
    /// `ca_cert`.
    ca_cert: (String, bool),
    /// `None` for the one target with no per-target credential field:
    /// the watchdog's `rp_url`, whose credential is the shared
    /// `service_auth` pair — `auth.mismatch`'s territory already.
    auth: Option<AuthTarget>,
}

/// Scheme and host divergences between a client's configured address and
/// its resolved target, with the fix ops that converge them — built by
/// [`plan_address`], which knows whether the address lives in one URL
/// string (scheme and host rewrites must compose into a single written
/// value) or in discrete fields (sentinel's monitors).
#[derive(Default)]
struct AddressDivergence {
    problems: Vec<String>,
    fixes: Vec<crate::report::FixOp>,
}

/// Judge `target`'s address as the client declares it — the scheme leg
/// (does it match the target's `server.tls` state) and, on an ACME
/// install, the host leg (#805 gap 2: the wildcard's only SAN is
/// `*.<domain>`, so a loopback host fails hostname verification no
/// matter the scheme). The host fix moves the client onto the target's
/// public name `<svc>.<domain>`; an `acme.json` declaring the staging
/// endpoint withholds that fix while still reporting the break — doctor
/// never converges clients onto a publicly-untrusted certificate (D4 of
/// docs/plans/acme-flip.md).
///
/// `scheme`/`host` arrive pre-parsed from [`judge_client_target`], which
/// already dropped any entry whose URL does not parse.
fn plan_address(
    ctx: &Context,
    client_service: &str,
    client_field: &str,
    locator: &TargetLocator,
    scheme: &str,
    host: &str,
    target: &ServiceScan,
) -> AddressDivergence {
    let mut divergence = AddressDivergence::default();
    let host_field = match locator {
        TargetLocator::Url { .. } => client_field,
        TargetLocator::Parts { host_field, .. } => host_field,
    };
    let target_tls_on = target.server().is_some_and(|s| s.tls.is_some());
    let expected = expected_scheme(target_tls_on);
    let scheme_diverges = !scheme.eq_ignore_ascii_case(expected);
    if scheme_diverges {
        divergence.problems.push(format!(
            "{client_field} uses {scheme}, but {} {} TLS",
            target.entry.name,
            if target_tls_on {
                "serves"
            } else {
                "does not serve"
            }
        ));
    }

    let mut new_host = None;
    if target_tls_on && target_uses_acme_cert(target) && is_loopback_host(host) {
        match crate::provision::active_acme_config(&ctx.config_dir) {
            Some(acme) => {
                let public_name = format!("{}.{}", target.entry.name, acme.domain);
                let staging_clause = if acme.staging {
                    "; acme.json declares the staging endpoint, so `doctor --fix` will \
                     not converge clients onto a publicly-untrusted certificate — \
                     rehearse in a scratch --config-dir, or reissue against production"
                } else {
                    new_host = Some(public_name);
                    ""
                };
                divergence.problems.push(format!(
                    "{host_field} points at {host}, but {}'s ACME wildcard certificate \
                     only matches *.{} names, so hostname verification fails{staging_clause}",
                    target.entry.name, acme.domain
                ));
            }
            // The wildcard pair is what the target serves regardless of
            // whether acme.json still reads, so the break is real either
            // way — report it; only the fix needs the domain. Mirrors
            // the CA leg's report-without-material shape below.
            None => divergence.problems.push(format!(
                "{host_field} points at {host}, but {} serves the ACME wildcard pair, \
                 whose *.<domain> name can never match a loopback host — and acme.json \
                 is missing or unreadable, so the public name cannot be derived for a \
                 fix",
                target.entry.name
            )),
        }
    }

    match locator {
        TargetLocator::Url { url, pointer } => {
            // Scheme and host rewrites compose into one written value —
            // two SetString ops on the same pointer would silently drop
            // whichever applied first.
            let mut value = Some(url.clone());
            if scheme_diverges {
                value = value.and_then(|v| rewrite_scheme(&v, expected));
            }
            if let Some(new_host) = &new_host {
                value = value.and_then(|v| rewrite_host(&v, new_host));
            }
            match value {
                Some(value) if scheme_diverges || new_host.is_some() => {
                    divergence.fixes.push(crate::report::FixOp::SetString {
                        service: client_service.to_string(),
                        pointer: pointer.clone(),
                        value,
                    });
                }
                Some(_) => {}
                // Reachable only when the URL parses but is not in a
                // form the byte-preserving splice can safely modify —
                // the field itself exists, so say what actually blocks
                // the rewrite.
                None => divergence.problems.push(format!(
                    "{client_field}'s URL is not in a form the byte-preserving rewrite \
                     can safely modify (unusual host spelling, or userinfo) — update it \
                     by hand"
                )),
            }
        }
        TargetLocator::Parts {
            scheme_pointer,
            host_pointer,
            ..
        } => {
            if scheme_diverges {
                divergence.fixes.push(crate::report::FixOp::SetString {
                    service: client_service.to_string(),
                    pointer: scheme_pointer.clone(),
                    value: expected.to_string(),
                });
            }
            if let Some(new_host) = new_host {
                divergence.fixes.push(crate::report::FixOp::SetString {
                    service: client_service.to_string(),
                    pointer: host_pointer.clone(),
                    value: new_host,
                });
            }
        }
    }
    divergence
}

/// `joins.client-transport`: the address divergences [`plan_address`]
/// found (scheme, and on an ACME install the loopback host), plus the
/// CA-trust leg judged here — when the scheme matches but `target`'s
/// material is doctor's self-signed CA rather than a publicly-trusted
/// ACME cert, can this client trust it. Every gap breaks each request to
/// `target`, so all grade `fail` (mirrors `tls.paths`: a definite break,
/// not a hardware-style installed/enabled split).
///
/// `ca_cert` is the client schema's CA-trust field as
/// `(pointer, already_present)`.
fn transport_check(
    ctx: &Context,
    client_service: &str,
    client_field: &str,
    target: &ServiceScan,
    address: AddressDivergence,
    ca_cert: (String, bool),
) -> Option<Check> {
    let target_tls_on = target.server().is_some_and(|s| s.tls.is_some());
    let AddressDivergence {
        mut problems,
        mut fixes,
    } = address;

    let (pointer, present) = ca_cert;
    if target_tls_on && !target_uses_acme_cert(target) && !present {
        let field_name = pointer.rsplit('/').next().unwrap_or(pointer.as_str());
        let ca_path = rusty_photon_tls::config::ca_cert_path(&crate::provision::absolute_pki_dir(
            &ctx.config_dir,
        ));
        if ca_path.is_file() {
            problems.push(format!(
                "{} serves a self-signed certificate, but {client_field} has \
                 no {field_name} to trust it",
                target.entry.name
            ));
            fixes.push(crate::report::FixOp::SetString {
                service: client_service.to_string(),
                pointer,
                value: ca_path.to_string_lossy().into_owned(),
            });
        } else {
            problems.push(format!(
                "{} serves a self-signed certificate, but {client_field} has \
                 no {field_name} to trust it, and doctor's own CA material \
                 does not exist yet for `--fix` to wire in",
                target.entry.name
            ));
        }
    }

    if problems.is_empty() {
        return None;
    }
    let suggestion = if fixes.is_empty() {
        format!("fix {client_field} by hand — no machine-applicable fix exists for this yet")
    } else {
        "run `doctor --fix` to align the client with its target's TLS state".to_string()
    };
    Some(
        Check::fail(
            "joins.client-transport",
            Some(client_service.to_string()),
            format!("{} — every request will fail", problems.join("; ")),
            Some(suggestion),
        )
        .with_fixes(fixes),
    )
}

/// The client-side credential value `{username, password}` — the plaintext
/// observatory credential, when minted. Mirrors
/// `provision::plan_service_client_wiring`'s inline shape.
fn plan_client_auth_value(ctx: &Context) -> Option<serde_json::Value> {
    let password = crate::provision::read_credential(&ctx.config_dir)?;
    Some(serde_json::json!({
        "username": crate::provision::CREDENTIAL_USERNAME,
        "password": password,
    }))
}

/// `joins.client-auth`: does `target` require authentication, and if so,
/// can this client supply a credential that verifies against it. Warn,
/// matching `auth.mismatch`'s severity — a wrong or missing credential
/// 401s every request, but (as with that check) a *present* mismatched
/// credential may be intentional, so only the absent case is fix-eligible.
/// `auth_pointer` is `None` only for targets with no credential field to
/// wire a fix into at all; every current caller (ui-htmx's targets, rp's
/// plate-solver/guider clients since issue #620, sentinel's per-monitor
/// `auth`) passes `Some`.
fn credential_check(
    ctx: &Context,
    client_service: &str,
    client_field: &str,
    target: &ServiceScan,
    auth_pointer: Option<&str>,
    current: Option<&ClientAuthView>,
) -> Option<Check> {
    let target_auth = target.server().and_then(|s| s.auth.as_ref())?;
    let credential = current.and_then(|c| Some((c.username.as_deref()?, c.password.as_deref()?)));
    match credential {
        None => {
            let fixes = match (auth_pointer, plan_client_auth_value(ctx)) {
                (Some(pointer), Some(value)) => vec![crate::report::FixOp::SetObject {
                    service: client_service.to_string(),
                    pointer: pointer.to_string(),
                    value,
                }],
                _ => Vec::new(),
            };
            let suggestion = if auth_pointer.is_some() {
                "run `doctor --fix` to wire the observatory credential".to_string()
            } else {
                format!(
                    "{client_service} has no credential field for this target yet — \
                     wiring one needs a config-schema change"
                )
            };
            Some(
                Check::warn(
                    "joins.client-auth",
                    Some(client_service.to_string()),
                    format!(
                        "{} requires authentication, but {client_field} carries no \
                         credential — every request will get 401",
                        target.entry.name
                    ),
                    Some(suggestion),
                )
                .with_fixes(fixes),
            )
        }
        Some((username, password)) => {
            if username == target_auth.username
                && rp_auth::credentials::verify_password(password, &target_auth.password_hash)
            {
                return None;
            }
            Some(Check::warn(
                "joins.client-auth",
                Some(client_service.to_string()),
                format!(
                    "{client_field}'s credential does not verify against {}'s \
                     server.auth — every request will get 401",
                    target.entry.name
                ),
                Some(
                    "run `doctor auth rotate` to re-align every copy, or fix the pair \
                     by hand"
                        .to_string(),
                ),
            ))
        }
    }
}

fn client_target_joins(ctx: &Context) -> Vec<Check> {
    client_targets(ctx)
        .iter()
        .flat_map(|target| judge_client_target(ctx, target))
        .collect()
}

/// The client-target registry: every URL/CA/auth pointer site the join
/// family judges, in one table — ui-htmx's `rp`/`sentinel` targets, rp's
/// plate-solver/guider clients plus the generic equipment roster and
/// dialed plugin registrations, and sentinel's watchdog `rp_url` and
/// Alpaca monitors (docs/services/doctor.md §Client-target joins).
fn client_targets(ctx: &Context) -> Vec<ClientTarget> {
    let mut targets = Vec::new();
    targets.extend(ui_htmx_targets(ctx));
    targets.extend(rp_targets(ctx));
    targets.extend(sentinel_targets(ctx));
    targets
}

/// Resolve one registry entry to its target and run the transport and
/// credential checks against it. An address that does not parse, carries
/// no explicit port, or resolves to no unambiguous local service files
/// no verdict — [`resolve_join_target`]'s own contract.
fn judge_client_target(ctx: &Context, t: &ClientTarget) -> Vec<Check> {
    let mut checks = Vec::new();
    let (scheme, host, port) = match &t.locator {
        TargetLocator::Url { url, .. } => {
            let Some(parsed) = parse_target_url(url) else {
                return checks;
            };
            parsed
        }
        TargetLocator::Parts {
            scheme, host, port, ..
        } => (scheme.clone(), host.clone(), *port),
    };
    let Some(target) = resolve_join_target(ctx, &host, port) else {
        return checks;
    };
    let address = plan_address(
        ctx,
        t.client,
        &t.transport_field,
        &t.locator,
        &scheme,
        &host,
        target,
    );
    checks.extend(transport_check(
        ctx,
        t.client,
        &t.transport_field,
        target,
        address,
        t.ca_cert.clone(),
    ));
    if let Some(auth) = &t.auth {
        checks.extend(credential_check(
            ctx,
            t.client,
            &auth.field,
            target,
            Some(&auth.pointer),
            auth.current.as_ref(),
        ));
    }
    checks
}

/// ui-htmx's `rp` (required) and `sentinel` (optional) targets — both
/// carry `base_url` + `auth` + `ca_cert_path`, so both the transport and
/// credential checks are fully fix-eligible.
fn ui_htmx_targets(ctx: &Context) -> Vec<ClientTarget> {
    let Some(ui_scan) = ctx.scan("ui-htmx").filter(|s| ctx.participates(s)) else {
        return Vec::new();
    };
    let Some(ui) = scan::view::<UiHtmxView>(ui_scan).and_then(Result::ok) else {
        return Vec::new();
    };
    [("rp", ui.rp), ("sentinel", ui.sentinel)]
        .into_iter()
        .filter_map(|(name, target)| {
            let target = target?;
            let url = target.base_url.clone()?;
            Some(ClientTarget {
                client: "ui-htmx",
                transport_field: format!("{name}.base_url"),
                locator: TargetLocator::Url {
                    url,
                    pointer: format!("/{name}/base_url"),
                },
                ca_cert: (
                    format!("/{name}/ca_cert_path"),
                    target
                        .ca_cert_path
                        .as_deref()
                        .is_some_and(|p| !p.is_empty()),
                ),
                auth: Some(AuthTarget {
                    field: format!("{name}.auth"),
                    pointer: format!("/{name}/auth"),
                    current: target.auth,
                }),
            })
        })
        .collect()
}

/// rp's plate-solver/guider clients, the generic equipment roster, and
/// the callback URL of every plugin registration rp dials (the
/// orchestrator's `invoke_url`, an event plugin's `webhook_url`):
/// `docs/services/doctor.md §Client-target joins`. CA trust is `rp`'s
/// single top-level `ca_cert` field (issue #609 / PR #612), shared by
/// every target, so the transport check is fully fix-eligible once that
/// field or its provisioning material exists. Every target also carries
/// its own `auth` field (issue #620: `plate_solver.auth`,
/// `equipment.mount.guiding.auth`; issue #663: every
/// `equipment.<kind>[].auth` / `equipment.mount.auth`; issue #800:
/// `plugins[].auth`), so `joins.client-auth` is fully fix-eligible for
/// all of them, the same "absent gets it, present is operator intent"
/// contract as every other D6a client fix.
fn rp_targets(ctx: &Context) -> Vec<ClientTarget> {
    let Some(rp) = ctx.scan("rp").and_then(|s| scan::view::<RpView>(s)?.ok()) else {
        return Vec::new();
    };
    let ca_cert_present = rp.ca_cert.as_deref().is_some_and(|p| !p.is_empty());
    let mut targets = Vec::new();
    if let Some(url) = rp.mount_guiding_url() {
        targets.push(rp_target(
            "equipment.mount.guiding.url",
            url,
            "/equipment/mount/guiding/url",
            "/equipment/mount/guiding/auth",
            rp.mount_guiding_auth(),
            ca_cert_present,
        ));
    }
    if let Some(ps) = rp.plate_solver.as_ref() {
        if let Some(url) = ps.url.clone() {
            targets.push(rp_target(
                "plate_solver.url",
                url,
                "/plate_solver/url",
                "/plate_solver/auth",
                ps.auth.clone(),
                ca_cert_present,
            ));
        }
    }
    for t in rp
        .equipment_targets()
        .into_iter()
        .chain(rp.plugin_targets())
    {
        targets.push(ClientTarget {
            client: "rp",
            transport_field: t.field.clone(),
            locator: TargetLocator::Url {
                url: t.url,
                pointer: t.url_pointer,
            },
            ca_cert: ("/ca_cert".to_string(), ca_cert_present),
            auth: Some(AuthTarget {
                field: t.field,
                pointer: t.auth_pointer,
                current: t.auth,
            }),
        });
    }
    targets
}

/// One hand-typed rp target (the guider, the plate solver): pointers are
/// static literals, and the credential check shares the transport
/// field's display name. Pointers are never derived from `field`: it is
/// a dotted string for display only, and `.` → `/` naive substitution
/// would mis-segment a pointer whenever a raw `/` or `~` appears inside
/// a path component — the generic roster's [`RpClientTarget`] pointers
/// arrive pre-escaped for the same reason.
fn rp_target(
    field: &str,
    url: String,
    url_pointer: &str,
    auth_pointer: &str,
    auth: Option<ClientAuthView>,
    ca_cert_present: bool,
) -> ClientTarget {
    ClientTarget {
        client: "rp",
        transport_field: field.to_string(),
        locator: TargetLocator::Url {
            url,
            pointer: url_pointer.to_string(),
        },
        ca_cert: ("/ca_cert".to_string(), ca_cert_present),
        auth: Some(AuthTarget {
            field: field.to_string(),
            pointer: auth_pointer.to_string(),
            current: auth,
        }),
    }
}

/// sentinel's other client targets: the operation watchdog's `rp_url`
/// (scheme, host and CA trust only — its credential is the shared
/// `service_auth` pair, already covered by `auth.mismatch`) and each
/// Alpaca monitor (scheme, host, CA trust, plus its own `auth`, which
/// `auth.mismatch` does not see). Every one of them trusts sentinel's
/// single top-level `ca_cert`, so every entry carries the same pointer —
/// the same shape rp's targets share.
fn sentinel_targets(ctx: &Context) -> Vec<ClientTarget> {
    let Some(sentinel) = ctx
        .scan("sentinel")
        .and_then(|s| scan::view::<SentinelView>(s)?.ok())
    else {
        return Vec::new();
    };
    let ca_cert_present = sentinel.ca_cert.as_deref().is_some_and(|p| !p.is_empty());
    let mut targets = Vec::new();
    if let Some(rp_url) = sentinel
        .operation_watchdog
        .as_ref()
        .and_then(|w| w.rp_url.clone())
    {
        targets.push(ClientTarget {
            client: "sentinel",
            transport_field: "operation_watchdog.rp_url".to_string(),
            locator: TargetLocator::Url {
                url: rp_url,
                pointer: "/operation_watchdog/rp_url".to_string(),
            },
            ca_cert: ("/ca_cert".to_string(), ca_cert_present),
            auth: None,
        });
    }
    for (idx, monitor) in sentinel.monitors.iter().enumerate() {
        // No per-monitor ca_cert_path: every monitor trusts sentinel's
        // single top-level `ca_cert`, so that is the field this join
        // reports and fixes.
        targets.push(ClientTarget {
            client: "sentinel",
            transport_field: format!("monitors[{idx}].scheme"),
            locator: TargetLocator::Parts {
                scheme: monitor.scheme.clone(),
                host: monitor.host.clone(),
                port: monitor.port,
                scheme_pointer: format!("/monitors/{idx}/scheme"),
                host_pointer: format!("/monitors/{idx}/host"),
                host_field: format!("monitors[{idx}].host"),
            },
            ca_cert: ("/ca_cert".to_string(), ca_cert_present),
            auth: Some(AuthTarget {
                field: format!("monitors[{idx}].auth"),
                pointer: format!("/monitors/{idx}/auth"),
                current: monitor.auth.clone(),
            }),
        });
    }
    targets
}

// ---- The fake-mount hazard (planetarium-bridge.md § Doctor integration) ----

/// `joins.fake-mount`, the static leg: rp's `equipment.mount.alpaca_url`
/// resolves — by the same loopback-host + port join every client-target
/// check uses — to the installed planetarium-bridge. The bridge is a
/// virtual target-entry device: wiring it in as rp's real mount would
/// defeat every motion safeguard rp believes it has (slews that "just
/// succeed", a mount that is never parked, never at limits), so this is
/// a hard failure, not a warning. The non-loopback case is the
/// aggregation probe's `UniqueID` leg (`crate::aggregate`).
fn fake_mount_join(ctx: &Context) -> Vec<Check> {
    let Some(rp) = ctx.scan("rp").and_then(|s| scan::view::<RpView>(s)?.ok()) else {
        return Vec::new();
    };
    let Some(url) = rp.mount_alpaca_url() else {
        return Vec::new();
    };
    let Some((_, host, port)) = parse_target_url(&url) else {
        return Vec::new();
    };
    let Some(target) = resolve_join_target(ctx, &host, port) else {
        return Vec::new();
    };
    if target.entry.name != "planetarium-bridge" {
        return Vec::new();
    }
    vec![Check::fail(
        "joins.fake-mount",
        Some("rp".to_string()),
        format!(
            "equipment.mount.alpaca_url ({url}) points at planetarium-bridge — a virtual \
             target-entry device, not a mount; slews against it \"just succeed\" without \
             moving anything, so every motion safeguard rp relies on (park, limits, slew \
             completion) is fiction"
        ),
        Some(
            "point equipment.mount.alpaca_url at the real mount driver; planetarium apps \
             connect to the bridge, rp never does"
                .to_string(),
        ),
    )]
}

// ---- ACME convergence (docs/plans/acme-flip.md — #805 gaps 1, 3, 4, 5, 6) ----
//
// Once acme.json exists the install's declared state is ACME (D1): every
// service serves the shared wildcard pair, clients trust the platform
// roots, sentinel probes and rp advertises public `<svc>.<domain>` names,
// and those names resolve on the box. These checks grade what still
// diverges from that state and plan fixes only where the divergent value
// is provably doctor's own material (D2) — everything else is reported
// with the derivable value and left to the operator.

/// The D4 downgrade, appended to a divergence detail when `acme.json`
/// declares the staging endpoint: the break is still reported, but the
/// check grades `warn` and plans no fix — doctor never converges a fleet
/// onto a publicly-untrusted certificate.
const STAGING_CLAUSE: &str = "; acme.json declares the staging endpoint, so `doctor --fix` \
     will not converge the fleet onto a publicly-untrusted certificate — rehearse in a \
     scratch --config-dir, or reissue against production";

/// The suggestion for a staging install's withheld fixes.
const STAGING_SUGGESTION: &str =
    "reissue against the production endpoint, then re-run `doctor --fix`";

fn acme_convergence(ctx: &Context) -> Vec<Check> {
    let Some(acme) = crate::provision::active_acme_config(&ctx.config_dir) else {
        return Vec::new();
    };
    let mut checks = Vec::new();
    // The stale-material checks wait for the wildcard pair: without it
    // the fleet is in renewal-recovery territory (`tls.absent` points at
    // `doctor tls renew`), and the still-serving self-signed material —
    // CA pins included — is all that keeps the install talking.
    if crate::provision::acme_tls_block_value(&ctx.config_dir).is_some() {
        checks.extend(stale_selfsigned_pointers(ctx, &acme));
        checks.extend(stale_ca_pins(ctx, &acme));
    }
    checks.extend(sentinel_probe_domain(ctx, &acme));
    checks.extend(rp_advertised_url(ctx, &acme));
    checks
}

/// `tls.stale-selfsigned-pointer`: a `server.tls` block still points at
/// material other than the wildcard pair the install serves. Doctor's
/// own per-service pair — matched by the exact path strings
/// `tls_block_value` writes — is fix-eligible: the block's `cert`/`key`
/// are rewritten onto the wildcard pair as plain string sets, so the
/// create-if-absent `SetObject` contract stays untouched. A hand-placed
/// path is operator intent: reported with the derivable paths, never
/// rewritten — doctor cannot know it is not valid for the public name.
fn stale_selfsigned_pointers(ctx: &Context, acme: &AcmeConfig) -> Vec<Check> {
    let pki = crate::provision::absolute_pki_dir(&ctx.config_dir);
    let wildcard_cert = crate::provision::acme_config::acme_cert_path(&pki);
    let wildcard_key = crate::provision::acme_config::acme_key_path(&pki);
    let mut checks = Vec::new();
    for scan in ctx.scans.iter().filter(|s| ctx.participates(s)) {
        let Some(tls) = scan.server().and_then(|s| s.tls.as_ref()) else {
            continue;
        };
        // Converged means exactly the pki tree's pair — by path, not by
        // the `acme-cert.pem` file-name convention the trust-model
        // classifiers use: a same-named copy elsewhere is one renewal
        // never rewrites, so it quietly ages out and belongs in the
        // hand-placed arm below.
        if tls.resolved_cert_path() == wildcard_cert && tls.resolved_key_path() == wildcard_key {
            continue; // on the wildcard pair — the flip's end state
        }
        let name = scan.entry.name;
        // Provenance (D2): doctor's own material is the exact path
        // strings it writes — `tls_block_value`'s absolute pair.
        let own = crate::provision::tls_block_value(&ctx.config_dir, name);
        let doctor_pair = own.get("cert").and_then(serde_json::Value::as_str)
            == Some(tls.cert.as_str())
            && own.get("key").and_then(serde_json::Value::as_str) == Some(tls.key.as_str());
        let check = if !doctor_pair {
            Check::warn(
                "tls.stale-selfsigned-pointer",
                Some(svc(scan)),
                format!(
                    "{name}'s server.tls points at {}, not the ACME wildcard pair this \
                     install serves — hand-placed material is operator intent, but the \
                     flipped fleet's clients verify against the platform roots and \
                     dial {name}.{}",
                    tls.cert, acme.domain
                ),
                Some(format!(
                    "point server.tls at {} and {} yourself if the material is stale, \
                     or leave it if it is deliberately valid for that name",
                    wildcard_cert.display(),
                    wildcard_key.display()
                )),
            )
        } else if acme.staging {
            Check::warn(
                "tls.stale-selfsigned-pointer",
                Some(svc(scan)),
                format!(
                    "{name}'s server.tls still points at the doctor-issued self-signed \
                     pair ({}) while the ACME wildcard pair exists{STAGING_CLAUSE}",
                    tls.cert
                ),
                Some(STAGING_SUGGESTION.to_string()),
            )
        } else {
            Check::fail(
                "tls.stale-selfsigned-pointer",
                Some(svc(scan)),
                format!(
                    "{name}'s server.tls still points at the doctor-issued self-signed \
                     pair ({}) while the ACME wildcard pair exists — the flipped \
                     fleet's clients trust the platform roots and reject a \
                     self-signed handshake",
                    tls.cert
                ),
                Some(
                    "run `doctor --fix` to repoint server.tls at the wildcard pair \
                     (the service picks it up at next restart)"
                        .to_string(),
                ),
            )
            .with_fixes(vec![
                crate::report::FixOp::SetString {
                    service: name.to_string(),
                    pointer: "/server/tls/cert".to_string(),
                    value: wildcard_cert.to_string_lossy().into_owned(),
                },
                crate::report::FixOp::SetString {
                    service: name.to_string(),
                    pointer: "/server/tls/key".to_string(),
                    value: wildcard_key.to_string_lossy().into_owned(),
                },
            ])
        };
        checks.push(check);
    }
    checks
}

/// One set CA-trust field: where it lives and what it holds.
struct CaPin {
    client: &'static str,
    /// Dotted display name (`ca_cert`, `rp.ca_cert`, `rp.ca_cert_path`).
    field: String,
    /// Already-escaped JSON pointer, for the `remove-key` fix.
    pointer: String,
    value: String,
}

/// Every set CA-trust field doctor knows: the `CLIENT_WIRING` services'
/// `ca_cert` (the exact fields the provisioning pass wires, so writer
/// and check share one table) plus ui-htmx's per-target `ca_cert_path`.
fn set_ca_pins(ctx: &Context) -> Vec<CaPin> {
    let mut pins = Vec::new();
    for (service, pointer) in crate::provision::ca_cert_pointers() {
        let Some(scan) = ctx.scan(service).filter(|s| ctx.participates(s)) else {
            continue;
        };
        let Some(pin) = scan
            .value()
            .and_then(|v| v.pointer(&pointer))
            .and_then(serde_json::Value::as_str)
            .filter(|p| !p.is_empty())
        else {
            continue;
        };
        pins.push(CaPin {
            client: service,
            field: pointer.trim_start_matches('/').replace('/', "."),
            value: pin.to_string(),
            pointer,
        });
    }
    let ui = ctx
        .scan("ui-htmx")
        .filter(|s| ctx.participates(s))
        .and_then(|s| scan::view::<UiHtmxView>(s)?.ok());
    if let Some(ui) = ui {
        for (name, target) in [("rp", ui.rp), ("sentinel", ui.sentinel)] {
            let Some(pin) = target
                .and_then(|t| t.ca_cert_path)
                .filter(|p| !p.is_empty())
            else {
                continue;
            };
            pins.push(CaPin {
                client: "ui-htmx",
                field: format!("{name}.ca_cert_path"),
                pointer: format!("/{name}/ca_cert_path"),
                value: pin,
            });
        }
    }
    pins
}

/// `tls.stale-ca-pin`: a set CA-trust field on an ACME install. The pin
/// replaces the platform trust roots (`tls_certs_only`), so the client
/// rejects the publicly-trusted wildcard whatever the pin points at.
/// Only the doctor-written `pki/ca.pem` path is fix-eligible
/// (`remove-key`); a foreign pin may be a deliberate private-CA trust
/// and is reported suggestion-only.
fn stale_ca_pins(ctx: &Context, acme: &AcmeConfig) -> Vec<Check> {
    let doctor_ca = rusty_photon_tls::config::ca_cert_path(&crate::provision::absolute_pki_dir(
        &ctx.config_dir,
    ))
    .to_string_lossy()
    .into_owned();
    set_ca_pins(ctx)
        .into_iter()
        .map(|pin| {
            let CaPin {
                client,
                field,
                pointer,
                value,
            } = pin;
            let doctor_pin = value == doctor_ca;
            let mut detail = if doctor_pin {
                format!(
                    "{field} still pins doctor's self-signed CA ({value}) on an ACME \
                     install — the pin replaces the platform trust roots, so {client} \
                     rejects the publicly-trusted wildcard the fleet serves"
                )
            } else {
                format!(
                    "{field} pins {value} on an ACME install — the pin replaces the \
                     platform trust roots, so {client} cannot verify the \
                     publicly-trusted wildcard the fleet serves"
                )
            };
            if acme.staging {
                detail.push_str(STAGING_CLAUSE);
            }
            let suggestion = if !doctor_pin {
                format!(
                    "remove {field} yourself if the pin is stale, or keep it if this \
                     client deliberately trusts a private CA"
                )
            } else if acme.staging {
                STAGING_SUGGESTION.to_string()
            } else {
                "run `doctor --fix` to remove the stale pin — the client then \
                 verifies against the platform trust store"
                    .to_string()
            };
            let check = if acme.staging {
                Check::warn(
                    "tls.stale-ca-pin",
                    Some(client.to_string()),
                    detail,
                    Some(suggestion),
                )
            } else {
                Check::fail(
                    "tls.stale-ca-pin",
                    Some(client.to_string()),
                    detail,
                    Some(suggestion),
                )
            };
            if doctor_pin && !acme.staging {
                check.with_fixes(vec![crate::report::FixOp::RemoveKey {
                    service: client.to_string(),
                    pointer,
                }])
            } else {
                check
            }
        })
        .collect()
}

/// `sentinel.probe-domain`: absent while the install is ACME — the
/// supervision probes then dial bind-derived hosts the wildcard's only
/// SAN `*.<domain>` can never match. The fix writes `acme.json`'s
/// `domain`, the identical value the aggregation probes derive; a
/// present value is operator intent and is left alone.
fn sentinel_probe_domain(ctx: &Context, acme: &AcmeConfig) -> Vec<Check> {
    let Some(scan) = ctx.scan("sentinel").filter(|s| ctx.participates(s)) else {
        return Vec::new();
    };
    if scan.value().is_none() {
        // Absent, unreadable, or invalid file: the read-level checks own
        // that diagnosis, and there is nothing to write into.
        return Vec::new();
    }
    let Some(view) = scan::view::<SentinelView>(scan).and_then(Result::ok) else {
        // config.known-blocks owns the diagnosis.
        return Vec::new();
    };
    if view.probe_domain.as_deref().is_some_and(|d| !d.is_empty()) {
        return Vec::new();
    }
    let domain = &acme.domain;
    let mut detail = format!(
        "sentinel has no probe_domain — on an ACME install its supervision probes \
         must dial <service>.{domain} names (the wildcard's only SAN is \
         *.{domain}), so probes against TLS-serving services fail hostname \
         verification"
    );
    if acme.staging {
        detail.push_str(STAGING_CLAUSE);
        return vec![Check::warn(
            "sentinel.probe-domain",
            Some("sentinel".to_string()),
            detail,
            Some(STAGING_SUGGESTION.to_string()),
        )];
    }
    vec![Check::warn(
        "sentinel.probe-domain",
        Some("sentinel".to_string()),
        detail,
        Some(format!(
            "run `doctor --fix` to write probe_domain = {domain} (sentinel picks it \
             up at next restart)"
        )),
    )
    .with_fixes(vec![crate::report::FixOp::SetString {
        service: "sentinel".to_string(),
        pointer: "/probe_domain".to_string(),
        value: domain.clone(),
    }])]
}

/// `rp.advertised-url`: absent while the install is ACME — the URL rp
/// advertises to orchestrators is derived from its bind address, which
/// the wildcard certificate can never match. The fix writes rp's public
/// name; with no `server` key at all the check still reports, and the
/// `--fix` fixpoint loop converges it one round after `tls.absent`'s
/// fix creates the block.
fn rp_advertised_url(ctx: &Context, acme: &AcmeConfig) -> Vec<Check> {
    let Some(scan) = ctx.scan("rp").filter(|s| ctx.participates(s)) else {
        return Vec::new();
    };
    if scan.value().is_none() {
        return Vec::new();
    }
    let writable = match &scan.server {
        ServerBlock::Parsed { advertised_url, .. } => {
            if advertised_url.is_some() {
                // Present is operator intent — nothing to converge.
                return Vec::new();
            }
            true
        }
        // The service applies its defaults, so the derivable URL carries
        // the catalog port. Fix ops never create intermediate structure,
        // so the write waits for a server block — which the same `--fix`
        // run's TLS fix creates, converging this one round later.
        ServerBlock::BlockAbsent => false,
        // config.server-shape / config.checks-skipped own these.
        ServerBlock::Invalid(_) | ServerBlock::FileAbsent => return Vec::new(),
    };
    let url = format!("https://rp.{}:{}", acme.domain, scan.effective_port());
    let mut detail = format!(
        "rp has no server.advertised_url — the URL it advertises to orchestrators \
         is derived from its bind address, which the ACME wildcard certificate can \
         never match; this install's derivable value is {url}"
    );
    if acme.staging {
        detail.push_str(STAGING_CLAUSE);
        return vec![Check::warn(
            "rp.advertised-url",
            Some("rp".to_string()),
            detail,
            Some(STAGING_SUGGESTION.to_string()),
        )];
    }
    let (suggestion, fixes) = if writable {
        (
            "run `doctor --fix` to write server.advertised_url (rp picks it up at \
             next restart)"
                .to_string(),
            vec![crate::report::FixOp::SetString {
                service: "rp".to_string(),
                pointer: "/server/advertised_url".to_string(),
                value: url,
            }],
        )
    } else {
        (
            "run `doctor --fix` — the write lands once a server block exists (the \
             same run's TLS fix creates one while the wildcard pair is on disk)"
                .to_string(),
            Vec::new(),
        )
    };
    vec![Check::warn(
        "rp.advertised-url",
        Some("rp".to_string()),
        detail,
        Some(suggestion),
    )
    .with_fixes(fixes)]
}

/// `dns.unresolvable` (D5): every participating service's derived
/// `<svc>.<domain>` name must resolve on this box — every client and
/// probe dials those names once the fleet converges. Report-only:
/// `/etc/hosts` is outside doctor's write surface, so the suggestion
/// carries the exact loopback line to paste (public DNS alone would
/// make on-box traffic depend on the WAN link and the DHCP lease —
/// tenets 1 and 2).
///
/// Not part of [`run_all`]: like the aggregation probes, this runs only
/// on the final report (`lib.rs` appends it) — it plans no fixes, and
/// resolving through a slow or misconfigured resolver on every `--fix`
/// fixpoint round would multiply that resolver's timeouts for nothing.
/// Resolvability comes from staged `facts.dns` when the scenario has a
/// DNS story, from the system resolver on a real (`probe_dns`) run,
/// and from neither under a mock without a `dns` object — the check is
/// then skipped, never resolved underneath a staged scenario.
pub(crate) fn dns_resolution(ctx: &Context) -> Vec<Check> {
    // Cap for the detail's name listing, the way `relative_names` caps;
    // the suggestion's hosts line is the deliverable and stays complete.
    const SHOWN: usize = 5;
    let Some(acme) = crate::provision::active_acme_config(&ctx.config_dir) else {
        return Vec::new();
    };
    let dns = match (&ctx.facts.dns, ctx.facts.probe_dns) {
        (Some(staged), _) => staged.clone(),
        (None, true) => gather_dns(ctx, &acme.domain, resolves_on_host),
        (None, false) => return Vec::new(),
    };
    let names: Vec<String> = ctx
        .scans
        .iter()
        .filter(|s| ctx.participates(s))
        .map(|s| format!("{}.{}", s.entry.name, acme.domain))
        .collect();
    if names.is_empty() {
        return Vec::new();
    }
    let unresolvable: Vec<&str> = names
        .iter()
        .map(String::as_str)
        .filter(|name| !dns.resolves(name))
        .collect();
    if unresolvable.is_empty() {
        return vec![Check::ok(
            "dns.unresolvable",
            None,
            format!(
                "all {} derived <service>.{} names resolve on this host",
                names.len(),
                acme.domain
            ),
        )];
    }
    let mut listed: Vec<String> = unresolvable
        .iter()
        .take(SHOWN)
        .map(|name| (*name).to_string())
        .collect();
    let more = unresolvable.len().saturating_sub(SHOWN);
    if more > 0 {
        listed.push(format!("and {more} more"));
    }
    let hosts_file = match ctx.facts.platform {
        Platform::Windows => r"%SystemRoot%\System32\drivers\etc\hosts",
        Platform::Linux | Platform::Macos => "/etc/hosts",
    };
    let mut detail = format!(
        "{} of the {} derived <service>.{} public names do not resolve on this \
         host ({}) — every client and probe dialing them fails before TLS even \
         starts",
        unresolvable.len(),
        names.len(),
        acme.domain,
        listed.join(", ")
    );
    let suggestion = format!(
        "add the loopback entries to {hosts_file} — public DNS alone would make \
         on-box traffic depend on the WAN link and the DHCP lease: \
         `127.0.0.1 {}`",
        unresolvable.join(" ")
    );
    if acme.staging {
        detail.push_str(
            "; acme.json declares the staging endpoint, so this is graded \
             as rehearsal state",
        );
        return vec![Check::warn(
            "dns.unresolvable",
            None,
            detail,
            Some(suggestion),
        )];
    }
    vec![Check::fail(
        "dns.unresolvable",
        None,
        detail,
        Some(suggestion),
    )]
}

/// Derive and resolve the participating services' public names — the
/// resolving half of `dns.unresolvable`, run once per real run, on the
/// final report only. `resolve` is injected so tests never resolve
/// real names; the binary passes [`resolves_on_host`].
fn gather_dns(ctx: &Context, domain: &str, resolve: impl Fn(&str) -> bool) -> DnsFacts {
    let resolvable = ctx
        .scans
        .iter()
        .filter(|s| ctx.participates(s))
        .map(|s| format!("{}.{domain}", s.entry.name))
        .filter(|name| resolve(name))
        .collect();
    DnsFacts { resolvable }
}

/// Whether `name` resolves through the host's resolver — `/etc/hosts`
/// first, DNS behind it: exactly the path every client dial takes.
///
/// Bounded per name: getaddrinfo has no cancellation, so the lookup
/// runs on its own thread and a name that answers nothing within the
/// deadline is judged unresolvable — on a black-holed resolver the
/// diagnosis must still finish (an answer that never comes is a
/// diagnosis, not a hang — the aggregation probes' rule). A stranded
/// lookup thread exits when its getaddrinfo does; doctor is a one-shot
/// process, so at worst a handful outlive the report by seconds.
fn resolves_on_host(name: &str) -> bool {
    use std::net::ToSocketAddrs;
    const RESOLVE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(5);
    let (tx, rx) = std::sync::mpsc::channel();
    let target = (name.to_string(), 0_u16);
    std::thread::spawn(move || {
        let resolved = target
            .to_socket_addrs()
            .is_ok_and(|mut addrs| addrs.next().is_some());
        // The receiver is gone when the deadline already expired; the
        // late answer is then the judged-unresolvable outcome anyway.
        let _ = tx.send(resolved);
    });
    rx.recv_timeout(RESOLVE_DEADLINE).unwrap_or(false)
}

// ---- rp platform defaults ----

fn rp_platform_defaults(ctx: &Context) -> Vec<Check> {
    let mut checks = Vec::new();
    let Some(rp_scan) = ctx.scan("rp") else {
        return checks;
    };
    let Some(rp) = scan::view::<RpView>(rp_scan).and_then(Result::ok) else {
        return checks;
    };
    let Some(dir) = rp.session.and_then(|s| s.data_directory) else {
        return checks;
    };
    let path = Path::new(&dir);
    if path.is_dir() {
        // Packaged Linux: existence is not enough — under systemd rp runs
        // as the rusty-photon user, and a root-owned directory from a
        // sudo'd first run is a classic way to strand session
        // persistence. Judged from ownership and mode (gathered facts),
        // so ACLs are invisible. Dev checkouts keep the existence-only
        // check, and so do macOS/Windows installs — brew services run as
        // the operator and the MSI's services as LocalSystem, so there is
        // no rusty-photon user to judge for.
        let unwritable = (ctx.mode == Mode::Packaged && ctx.facts.platform == Platform::Linux)
            .then_some(ctx.hardware.as_ref())
            .flatten()
            .and_then(|hw| {
                let node = hw.paths.get(&dir)?;
                let user = hw.service_user?;
                let identity = rusty_photon_doctor_checks::Identity {
                    uid: user.uid,
                    gids: vec![user.gid],
                };
                (!identity.can_write_dir(node)).then_some((node.mode, node.uid, node.gid))
            });
        match unwritable {
            Some((mode, uid, gid)) => checks.push(Check::fail(
                "rp.data-directory",
                Some("rp".to_string()),
                format!(
                    "session.data_directory {dir} exists but is not writable by \
                     the {} user (mode {mode:o}, uid {uid}, gid {gid}) — judged \
                     from ownership and mode, so ACLs are invisible to this check",
                    crate::hardware::SERVICE_USER
                ),
                Some(format!(
                    "chown it to the service user: `chown {}: {dir}`",
                    crate::hardware::SERVICE_USER
                )),
            )),
            None => checks.push(Check::ok(
                "rp.data-directory",
                Some("rp".to_string()),
                format!("session.data_directory {dir} exists"),
            )),
        }
    } else {
        checks.push(Check::fail(
            "rp.data-directory",
            Some("rp".to_string()),
            format!(
                "session.data_directory {dir} does not exist — session state \
                 cannot persist, and rp's scaffold default is not valid on every \
                 platform"
            ),
            Some(format!(
                "create it (`mkdir -p {dir}`) or point rp elsewhere"
            )),
        ));
    }
    checks
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[allow(clippy::unreachable)]
mod tests {
    use super::*;
    use rusty_photon_doctor_checks::report::Status;

    fn config_only_ctx(config_dir: &Path) -> Context {
        let facts: PlatformFacts =
            serde_json::from_value(serde_json::json!({ "platform": "linux" })).unwrap();
        Context::gather(config_dir.to_path_buf(), facts)
    }

    #[test]
    fn test_tls_absent_fix_points_at_the_wildcard_pair_on_an_acme_install() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("ppba-driver.json"),
            r#"{ "server": { "port": 11112 } }"#,
        )
        .unwrap();
        std::fs::write(dir.path().join("acme.json"), "{}").unwrap();
        let pki = dir.path().join("pki");
        std::fs::create_dir_all(&pki).unwrap();
        std::fs::write(pki.join("acme-cert.pem"), "cert").unwrap();
        std::fs::write(pki.join("acme-key.pem"), "key").unwrap();
        let ctx = config_only_ctx(dir.path());
        let scan = ctx.scan("ppba-driver").unwrap();
        let checks = tls_auth_absent(&ctx, scan);
        let tls = checks.iter().find(|c| c.name == "tls.absent").unwrap();
        match &tls.fixes[..] {
            [crate::report::FixOp::SetObject { pointer, value, .. }] => {
                assert_eq!(pointer, "/server/tls");
                let cert = value["cert"].as_str().unwrap();
                let key = value["key"].as_str().unwrap();
                assert!(cert.ends_with("acme-cert.pem"), "{cert}");
                assert!(key.ends_with("acme-key.pem"), "{key}");
                assert!(
                    !cert.contains("ppba-driver"),
                    "no per-service self-signed pair on an ACME install: {cert}"
                );
            }
            other => unreachable!("expected one SetObject fix, got {other:?}"),
        }
    }

    #[test]
    fn test_tls_absent_plans_no_fix_while_the_acme_pair_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("ppba-driver.json"),
            r#"{ "server": { "port": 11112 } }"#,
        )
        .unwrap();
        std::fs::write(dir.path().join("acme.json"), "{}").unwrap();
        let ctx = config_only_ctx(dir.path());
        let scan = ctx.scan("ppba-driver").unwrap();
        let checks = tls_auth_absent(&ctx, scan);
        let tls = checks.iter().find(|c| c.name == "tls.absent").unwrap();
        assert!(
            tls.fixes.is_empty(),
            "a missing wildcard pair is renewal's to recover: {:?}",
            tls.fixes
        );
        let suggestion = tls.suggestion.as_deref().unwrap();
        assert!(suggestion.contains("doctor tls renew"), "{suggestion}");
    }

    #[test]
    fn test_tls_expiry_fails_on_an_unreadable_certificate() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = config_only_ctx(dir.path());
        let scan = &ctx.scans[0];
        // A directory at the cert path: read_to_string errors while the
        // path itself exists.
        let check = tls_expiry(&ctx, scan, dir.path());
        assert_eq!(check.status, Status::Fail);
        assert!(
            check.detail.contains("could not be read"),
            "{}",
            check.detail
        );
    }

    #[test]
    fn test_expiry_window_days_reads_the_acme_config_for_the_wildcard() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("acme.json"),
            serde_json::json!({
                "email": "ops@example.com",
                "domain": "observatory.test",
                "dns_provider": "cloudflare",
                "dns_credentials": { "api_token": "tok" },
                "renewal_days_before_expiry": 33,
            })
            .to_string(),
        )
        .unwrap();
        let ctx = config_only_ctx(dir.path());
        assert_eq!(expiry_window_days(&ctx, Path::new("pki/acme-cert.pem")), 33);
        // A self-signed pair keeps the 30-day default even with acme.json.
        assert_eq!(expiry_window_days(&ctx, Path::new("pki/rp.pem")), 30);
    }

    #[test]
    fn test_expiry_window_days_defaults_without_acme_json() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = config_only_ctx(dir.path());
        assert_eq!(expiry_window_days(&ctx, Path::new("pki/acme-cert.pem")), 30);
    }

    #[test]
    fn test_start_command_speaks_each_platforms_language() {
        assert_eq!(
            start_command(Platform::Linux, "rusty-photon-rp"),
            "systemctl start rusty-photon-rp"
        );
        assert_eq!(
            start_command(Platform::Windows, "rusty-photon-rp"),
            "Start-Service rusty-photon-rp"
        );
        assert_eq!(
            start_command(Platform::Macos, "rusty-photon-rp-nightly"),
            "brew services start rusty-photon-rp-nightly"
        );
    }

    // ---- tls.absent / auth.absent on a missing config file ----

    #[test]
    fn test_tls_auth_file_absent_warns_a_self_defaulting_service_unfixably() {
        let dir = tempfile::tempdir().unwrap();
        let entry = catalog::entry("zwo-camera").unwrap();
        let scan = scan::scan_service(dir.path(), entry);
        assert!(matches!(scan.server, ServerBlock::FileAbsent));

        let checks = tls_auth_file_absent(&scan);
        assert_eq!(checks.len(), 2);

        let tls = checks
            .iter()
            .find(|c| c.name == "tls.absent")
            .expect("tls.absent");
        assert_eq!(tls.status, Status::Warn);
        assert!(
            tls.fixes.is_empty(),
            "no config file exists to provision into"
        );
        assert!(
            tls.detail.contains("never started"),
            "should name both possible causes: {}",
            tls.detail
        );

        let auth = checks
            .iter()
            .find(|c| c.name == "auth.absent")
            .expect("auth.absent");
        assert_eq!(auth.status, Status::Warn);
        assert_eq!(
            auth.fixes,
            Vec::<rusty_photon_doctor_checks::report::FixOp>::new()
        );
    }

    /// A packaged context whose units carry a gathered failure state.
    fn failure_ctx(dir: &Path, platform: &str, units: &[(&str, bool)]) -> Context {
        let units: Vec<serde_json::Value> = units
            .iter()
            .map(|(name, failed)| serde_json::json!({ "name": name, "failed": failed }))
            .collect();
        let facts: PlatformFacts = serde_json::from_value(serde_json::json!({
            "platform": platform,
            "units": units,
        }))
        .unwrap();
        Context::gather(dir.to_path_buf(), facts)
    }

    #[test]
    fn test_failed_units_names_the_unit_and_the_service_it_runs() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = failure_ctx(
            dir.path(),
            "linux",
            &[("rusty-photon-rp", true), ("rusty-photon-sentinel", false)],
        );

        let checks = failed_units(&ctx);
        assert_eq!(checks.len(), 1, "{checks:?}");
        assert_eq!(checks[0].status, Status::Fail);
        assert_eq!(checks[0].service.as_deref(), Some("rp"));
        assert!(checks[0].detail.contains("rusty-photon-rp"), "{checks:?}");
        assert!(
            checks[0]
                .suggestion
                .as_ref()
                .unwrap()
                .contains("journalctl"),
            "{:?}",
            checks[0].suggestion
        );
    }

    #[test]
    fn test_a_failed_renewal_job_says_what_it_costs() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = failure_ctx(dir.path(), "linux", &[("rusty-photon-renew", true)]);

        let checks = failed_units(&ctx);
        assert_eq!(checks.len(), 1, "{checks:?}");
        assert_eq!(
            checks[0].service, None,
            "the renewal one-shot is a job, not a catalog service"
        );
        assert!(
            checks[0].detail.contains("renewed"),
            "a failed renewal must name the consequence, not just the state: {}",
            checks[0].detail
        );
    }

    #[test]
    fn test_failed_units_is_ok_when_every_unit_is_healthy_and_silent_when_ungathered() {
        let dir = tempfile::tempdir().unwrap();
        let healthy = failure_ctx(dir.path(), "linux", &[("rusty-photon-rp", false)]);
        let checks = failed_units(&healthy);
        assert_eq!(checks.len(), 1, "{checks:?}");
        assert_eq!(checks[0].status, Status::Ok);

        // Windows gathers no failure state: no row either way.
        let ungathered = packaged_ctx(dir.path(), "windows", "rusty-photon-rp");
        assert!(failed_units(&ungathered).is_empty());
    }

    #[test]
    fn test_a_failed_unit_on_macos_is_pointed_at_brew() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = failure_ctx(dir.path(), "macos", &[("rusty-photon-rp", true)]);
        let suggestion = failed_units(&ctx)[0].suggestion.clone().unwrap();
        assert!(suggestion.contains("brew services"), "{suggestion}");
    }

    fn mismatch(path: &Path, essential: bool) -> crate::provision::OwnershipMismatch {
        crate::provision::OwnershipMismatch {
            path: path.to_path_buf(),
            essential,
        }
    }

    #[test]
    fn test_pki_ownership_is_ok_on_a_tree_this_run_owns() {
        let dir = tempfile::tempdir().unwrap();
        let pki = dir.path().join("pki");
        std::fs::create_dir_all(&pki).unwrap();
        std::fs::write(pki.join("ca.pem"), "cert").unwrap();
        let ctx = config_only_ctx(dir.path());

        let checks = pki_ownership(&ctx);
        if !cfg!(unix) {
            // Windows has no owner to compare material against, so the
            // check says nothing rather than claiming a tree is fine.
            assert!(checks.is_empty(), "{checks:?}");
            return;
        }
        assert_eq!(checks.len(), 1, "{checks:?}");
        assert_eq!(checks[0].name, "tls.ownership");
        assert_eq!(checks[0].status, Status::Ok);
    }

    #[test]
    fn test_pki_ownership_says_nothing_without_material_to_judge() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            pki_ownership(&config_only_ctx(dir.path())).is_empty(),
            "a host that has never been provisioned has no ownership story"
        );
        // Nor about a config root that is not there at all: there is no
        // owner to compare against, which is silence, not a verdict.
        let absent = dir.path().join("never-created");
        assert!(pki_ownership(&config_only_ctx(&absent)).is_empty());
    }

    #[test]
    fn test_pki_ownership_fails_on_material_and_warns_on_strays_separately() {
        let dir = tempfile::tempdir().unwrap();
        let pki = dir.path().join("pki");
        let ctx = config_only_ctx(dir.path());
        let ownership = crate::provision::PkiOwnership {
            uid: 109,
            gid: 114,
            examined: 4,
            mismatched: vec![
                mismatch(&pki.join("zwo-camera-key.pem"), true),
                mismatch(&pki.join("ca.srl"), false),
            ],
        };

        let checks = ownership_checks(&ctx, &ownership);
        assert_eq!(checks.len(), 2, "{checks:?}");
        let failure = checks.iter().find(|c| c.status == Status::Fail).unwrap();
        // Built as a path, not spelled with a separator: the detail renders
        // whatever the platform uses.
        let material = Path::new("pki").join("zwo-camera-key.pem");
        assert!(
            failure.detail.contains(&material.display().to_string()),
            "the failure names the material, relative to the config root: {}",
            failure.detail
        );
        assert!(!failure.detail.contains("ca.srl"), "{}", failure.detail);
        assert!(
            failure
                .suggestion
                .as_ref()
                .unwrap()
                .contains("chown 109:114"),
            "{:?}",
            failure.suggestion
        );

        let stray = checks.iter().find(|c| c.status == Status::Warn).unwrap();
        let srl = Path::new("pki").join("ca.srl");
        assert!(
            stray.detail.contains(&srl.display().to_string()),
            "{}",
            stray.detail
        );
        assert!(
            !stray.detail.contains("zwo-camera-key.pem"),
            "a stray warning must not restate the failure: {}",
            stray.detail
        );
    }

    #[test]
    fn test_pki_ownership_caps_the_names_it_lists() {
        let dir = tempfile::tempdir().unwrap();
        let pki = dir.path().join("pki");
        let ctx = config_only_ctx(dir.path());
        let mismatched: Vec<_> = (0..7)
            .map(|i| mismatch(&pki.join(format!("service-{i}.pem")), true))
            .collect();
        let ownership = crate::provision::PkiOwnership {
            uid: 0,
            gid: 0,
            examined: 7,
            mismatched,
        };

        let checks = ownership_checks(&ctx, &ownership);
        let detail = &checks[0].detail;
        assert!(detail.contains("service-4.pem"), "{detail}");
        assert!(
            detail.contains("and 2 more") && !detail.contains("service-5.pem"),
            "a wholly foreign-owned tree names a few files, not all of them: {detail}"
        );
    }

    #[test]
    fn test_tls_auth_file_absent_stays_silent_for_a_config_gated_service() {
        let dir = tempfile::tempdir().unwrap();
        let entry = catalog::entry("plate-solver").unwrap();
        assert!(entry.config_gated);
        let scan = scan::scan_service(dir.path(), entry);
        assert!(tls_auth_file_absent(&scan).is_empty());
    }

    fn packaged_ctx(dir: &Path, platform: &str, unit: &str) -> Context {
        let facts: PlatformFacts = serde_json::from_value(serde_json::json!({
            "platform": platform,
            "units": [ { "name": unit } ],
        }))
        .unwrap();
        Context::gather(dir.to_path_buf(), facts)
    }

    #[test]
    fn test_tls_and_auth_warns_an_installed_self_defaulting_service_with_no_config() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = packaged_ctx(dir.path(), "linux", "rusty-photon-zwo-camera");
        let checks = tls_and_auth(&ctx);
        assert!(checks
            .iter()
            .any(|c| c.name == "tls.absent" && c.service.as_deref() == Some("zwo-camera")));
        assert!(checks
            .iter()
            .any(|c| c.name == "auth.absent" && c.service.as_deref() == Some("zwo-camera")));
    }

    #[test]
    fn test_tls_and_auth_stays_silent_for_an_installed_config_gated_service_with_no_config() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = packaged_ctx(dir.path(), "linux", "rusty-photon-plate-solver");
        let checks = tls_and_auth(&ctx);
        assert!(!checks
            .iter()
            .any(|c| (c.name == "tls.absent" || c.name == "auth.absent")
                && c.service.as_deref() == Some("plate-solver")));
    }

    /// `condition_path` is a Linux/systemd-only fact — Windows carries no
    /// equivalent, so the remedy must fall back to the portable
    /// `config_gated` catalog flag instead of wrongly claiming the service
    /// self-creates its defaults.
    #[test]
    fn test_inventory_names_a_hand_written_config_for_a_gated_service_on_windows() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = packaged_ctx(dir.path(), "windows", "rusty-photon-plate-solver");
        let checks = inventory(&ctx);
        let check = checks
            .iter()
            .find(|c| {
                c.name == "inventory.unit-without-config"
                    && c.service.as_deref() == Some("plate-solver")
            })
            .expect("inventory.unit-without-config");
        let suggestion = check.suggestion.as_deref().unwrap_or_default();
        assert!(suggestion.contains("no sensible default"), "{suggestion}");
        assert!(
            !suggestion.contains("self-creates"),
            "a config-gated service never self-creates: {suggestion}"
        );
        // The detail text must not blame "never started" on a service that
        // structurally can't start without an operator-written config first.
        assert!(!check.detail.contains("never started"), "{}", check.detail);
        assert!(
            check.detail.contains("cannot start without one"),
            "{}",
            check.detail
        );
    }

    // ---- Client-target joins ----

    fn write_json(dir: &Path, name: &str, value: serde_json::Value) {
        std::fs::write(dir.join(name), value.to_string()).unwrap();
    }

    /// A minted observatory credential on disk — what `--fix`'s
    /// provisioning pass leaves behind before these checks ever run.
    fn stage_pki(dir: &Path, password: &str) {
        std::fs::create_dir_all(dir.join("pki")).unwrap();
        std::fs::write(dir.join("pki/ca.pem"), "stub-ca-pem").unwrap();
        std::fs::write(dir.join("pki/credential"), format!("{password}\n")).unwrap();
    }

    #[test]
    fn test_is_loopback_host() {
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("localhost"));
        assert!(is_loopback_host("::1"));
        // DNS names are case-insensitive, and a monitor's `host` field
        // arrives exactly as the config spells it.
        assert!(is_loopback_host("LOCALHOST"));
        assert!(is_loopback_host("LocalHost"));
        assert!(!is_loopback_host("10.0.0.5"));
        assert!(!is_loopback_host("rig.local"));
    }

    #[test]
    fn test_parse_target_url_requires_an_explicit_port() {
        assert_eq!(
            parse_target_url("https://host:11115/x"),
            Some(("https".to_string(), "host".to_string(), 11115))
        );
        assert!(parse_target_url("https://host/x").is_none());
        assert!(parse_target_url("not a url").is_none());
    }

    #[test]
    fn test_rewrite_scheme_preserves_the_url_verbatim_without_adding_a_trailing_slash() {
        // A parse-and-reserialize round trip would normalize
        // "http://host:port" into "http://host:port/" — several client
        // call sites concatenate a "/"-prefixed path onto the base URL
        // without trimming, so a stray trailing slash would 404 them.
        assert_eq!(
            rewrite_scheme("http://127.0.0.1:11115", "https").unwrap(),
            "https://127.0.0.1:11115"
        );
        assert_eq!(
            rewrite_scheme("https://host:11114/dash?x=1", "http").unwrap(),
            "http://host:11114/dash?x=1"
        );
        assert!(rewrite_scheme("not a url", "https").is_none());
        // `Url::scheme()` lowercases; stripping it off the raw string
        // would silently fail to match an uppercase input scheme.
        assert_eq!(
            rewrite_scheme("HTTP://127.0.0.1:11115", "https").unwrap(),
            "https://127.0.0.1:11115"
        );
    }

    #[test]
    fn test_rewrite_host_preserves_the_url_verbatim_without_adding_a_trailing_slash() {
        // Same hazard as the scheme rewrite: a parse-and-reserialize
        // round trip would append a trailing slash to an origin-only URL.
        assert_eq!(
            rewrite_host("https://127.0.0.1:11115", "rp.pier1.example.com").unwrap(),
            "https://rp.pier1.example.com:11115"
        );
        assert_eq!(
            rewrite_host("http://localhost:11114/dash?x=1", "sentinel.d.io").unwrap(),
            "http://sentinel.d.io:11114/dash?x=1"
        );
        assert!(rewrite_host("not a url", "rp.d.io").is_none());
    }

    #[test]
    fn test_rewrite_host_handles_bracketed_ipv6_and_bails_without_a_port() {
        assert_eq!(
            rewrite_host("https://[::1]:11115/x", "rp.pier1.example.com").unwrap(),
            "https://rp.pier1.example.com:11115/x"
        );
        // A URL with no explicit port never resolves a join, so there is
        // nothing to rewrite it for.
        assert!(rewrite_host("https://127.0.0.1/x", "rp.d.io").is_none());
    }

    /// A parsed `acme.json` in the config root — the flip's declared
    /// target state (D1 of docs/plans/acme-flip.md).
    fn stage_acme(dir: &Path, domain: &str, staging: bool) {
        write_json(
            dir,
            "acme.json",
            serde_json::json!({
                "email": format!("ops@{domain}"),
                "domain": domain,
                "dns_provider": "cloudflare",
                "dns_credentials": { "api_token": "$CF_TOKEN" },
                "staging": staging,
            }),
        );
    }

    #[test]
    fn test_resolve_join_target_joins_the_exact_public_acme_name() {
        let dir = tempfile::tempdir().unwrap();
        write_json(
            dir.path(),
            "ppba-driver.json",
            serde_json::json!({ "server": { "port": 11112 } }),
        );
        stage_acme(dir.path(), "pier1.example.com", false);
        let ctx = config_only_ctx(dir.path());
        let target = resolve_join_target(&ctx, "ppba-driver.pier1.example.com", 11112)
            .expect("the port-matched service's own public name joins");
        assert_eq!(target.entry.name, "ppba-driver");
        // DNS names are case-insensitive; a config-spelled variant still
        // names the same host.
        assert!(resolve_join_target(&ctx, "PPBA-Driver.Pier1.Example.COM", 11112).is_some());
        // Another service's name on this port, a nested subdomain, and a
        // foreign domain are all somebody else's address — never joined.
        assert!(resolve_join_target(&ctx, "sentinel.pier1.example.com", 11112).is_none());
        assert!(resolve_join_target(&ctx, "ppba-driver.rig.pier1.example.com", 11112).is_none());
        assert!(resolve_join_target(&ctx, "ppba-driver.other.example.com", 11112).is_none());
    }

    #[test]
    fn test_resolve_join_target_keeps_the_loopback_only_shape_without_a_readable_acme_json() {
        let dir = tempfile::tempdir().unwrap();
        write_json(
            dir.path(),
            "ppba-driver.json",
            serde_json::json!({ "server": { "port": 11112 } }),
        );
        let ctx = config_only_ctx(dir.path());
        assert!(resolve_join_target(&ctx, "ppba-driver.pier1.example.com", 11112).is_none());
        // Present but unreadable keeps the same shape rather than guess.
        std::fs::write(dir.path().join("acme.json"), "{}").unwrap();
        let ctx = config_only_ctx(dir.path());
        assert!(resolve_join_target(&ctx, "ppba-driver.pier1.example.com", 11112).is_none());
        assert!(resolve_join_target(&ctx, "127.0.0.1", 11112).is_some());
    }

    #[test]
    fn test_an_unparsable_client_url_files_no_verdict() {
        let dir = tempfile::tempdir().unwrap();
        write_json(
            dir.path(),
            "rp.json",
            serde_json::json!({ "server": { "port": 11115,
                "tls": { "cert": "/pki/acme-cert.pem", "key": "/pki/acme-key.pem" } } }),
        );
        write_json(
            dir.path(),
            "ui-htmx.json",
            serde_json::json!({ "server": { "port": 11120 },
                "rp": { "base_url": "not a url" } }),
        );
        let ctx = config_only_ctx(dir.path());
        assert!(client_target_joins(&ctx).is_empty());
    }

    #[test]
    fn test_an_unspliceable_url_reports_the_break_without_a_rewrite() {
        // `Url::host_str()` lowercases, so an uppercase loopback host
        // joins and fires the hostname leg, but the raw string offers no
        // splice point — the break is reported with no fix rather than a
        // guessed rewrite.
        let dir = tempfile::tempdir().unwrap();
        stage_acme(dir.path(), "pier1.example.com", false);
        write_json(
            dir.path(),
            "rp.json",
            serde_json::json!({ "server": { "port": 11115,
                "tls": { "cert": "/pki/acme-cert.pem", "key": "/pki/acme-key.pem" } } }),
        );
        write_json(
            dir.path(),
            "ui-htmx.json",
            serde_json::json!({ "server": { "port": 11120 },
                "rp": { "base_url": "https://LOCALHOST:11115" } }),
        );
        let ctx = config_only_ctx(dir.path());
        let checks = client_target_joins(&ctx);
        let transport = checks
            .iter()
            .find(|c| c.name == "joins.client-transport")
            .expect("the hostname break must still be reported");
        assert!(
            transport.detail.contains("update it by hand"),
            "{}",
            transport.detail
        );
        assert!(transport.fixes.is_empty(), "{:?}", transport.fixes);
    }

    #[test]
    fn test_acme_loopback_url_composes_scheme_and_host_into_one_fix() {
        let dir = tempfile::tempdir().unwrap();
        stage_acme(dir.path(), "pier1.example.com", false);
        write_json(
            dir.path(),
            "rp.json",
            serde_json::json!({ "server": { "port": 11115,
                "tls": { "cert": "/pki/acme-cert.pem", "key": "/pki/acme-key.pem" } } }),
        );
        write_json(
            dir.path(),
            "ui-htmx.json",
            serde_json::json!({ "server": { "port": 11120 },
                "rp": { "base_url": "http://127.0.0.1:11115" } }),
        );
        let ctx = config_only_ctx(dir.path());
        let checks = client_target_joins(&ctx);
        let transport = checks
            .iter()
            .find(|c| c.name == "joins.client-transport")
            .expect("scheme and hostname breaks must be reported");
        assert!(
            transport.detail.contains("hostname verification"),
            "{}",
            transport.detail
        );
        match &transport.fixes[..] {
            [crate::report::FixOp::SetString { pointer, value, .. }] => {
                assert_eq!(pointer, "/rp/base_url");
                // One composed value — two ops on the same pointer would
                // silently drop whichever applied first.
                assert_eq!(value, "https://rp.pier1.example.com:11115");
            }
            other => unreachable!("expected one composed URL rewrite, got {other:?}"),
        }
    }

    #[test]
    fn test_acme_staging_reports_the_loopback_break_without_a_fix() {
        let dir = tempfile::tempdir().unwrap();
        stage_acme(dir.path(), "pier1.example.com", true);
        write_json(
            dir.path(),
            "rp.json",
            serde_json::json!({ "server": { "port": 11115,
                "tls": { "cert": "/pki/acme-cert.pem", "key": "/pki/acme-key.pem" } } }),
        );
        write_json(
            dir.path(),
            "ui-htmx.json",
            serde_json::json!({ "server": { "port": 11120 },
                "rp": { "base_url": "https://127.0.0.1:11115" } }),
        );
        let ctx = config_only_ctx(dir.path());
        let checks = client_target_joins(&ctx);
        let transport = checks
            .iter()
            .find(|c| c.name == "joins.client-transport")
            .expect("the hostname break is real regardless of staging");
        assert!(transport.detail.contains("staging"), "{}", transport.detail);
        assert!(
            transport.fixes.is_empty(),
            "doctor never converges clients onto a publicly-untrusted certificate: {:?}",
            transport.fixes
        );
    }

    #[test]
    fn test_a_case_variant_monitor_host_still_joins_and_gets_the_host_fix() {
        // A monitor's `host` reaches the join exactly as the config
        // spells it (no URL parser lowercases it) — `LOCALHOST` names
        // the same machine and must not silently skip the join.
        let dir = tempfile::tempdir().unwrap();
        stage_acme(dir.path(), "pier1.example.com", false);
        write_json(
            dir.path(),
            "ppba-driver.json",
            serde_json::json!({ "server": { "port": 11112,
                "tls": { "cert": "/pki/acme-cert.pem", "key": "/pki/acme-key.pem" } } }),
        );
        write_json(
            dir.path(),
            "sentinel.json",
            serde_json::json!({ "server": { "port": 11114 },
                "monitors": [ { "type": "alpaca_safety_monitor", "name": "PPBA",
                    "host": "LOCALHOST", "port": 11112, "scheme": "https" } ] }),
        );
        let ctx = config_only_ctx(dir.path());
        let checks = client_target_joins(&ctx);
        let transport = checks
            .iter()
            .find(|c| c.name == "joins.client-transport")
            .expect("a case-variant loopback host must still be judged");
        match &transport.fixes[..] {
            [crate::report::FixOp::SetString { pointer, value, .. }] => {
                assert_eq!(pointer, "/monitors/0/host");
                assert_eq!(value, "ppba-driver.pier1.example.com");
            }
            other => unreachable!("expected one host fix, got {other:?}"),
        }
    }

    #[test]
    fn test_sentinel_monitor_loopback_host_is_moved_to_the_public_name() {
        let dir = tempfile::tempdir().unwrap();
        stage_acme(dir.path(), "pier1.example.com", false);
        write_json(
            dir.path(),
            "ppba-driver.json",
            serde_json::json!({ "server": { "port": 11112,
                "tls": { "cert": "/pki/acme-cert.pem", "key": "/pki/acme-key.pem" } } }),
        );
        write_json(
            dir.path(),
            "sentinel.json",
            serde_json::json!({ "server": { "port": 11114 },
                "monitors": [ { "type": "alpaca_safety_monitor", "name": "PPBA",
                    "host": "127.0.0.1", "port": 11112, "scheme": "https" } ] }),
        );
        let ctx = config_only_ctx(dir.path());
        let checks = client_target_joins(&ctx);
        let transport = checks
            .iter()
            .find(|c| {
                c.name == "joins.client-transport" && c.service.as_deref() == Some("sentinel")
            })
            .expect("the monitor's loopback host must be reported");
        assert!(
            transport.detail.contains("monitors[0].host"),
            "{}",
            transport.detail
        );
        match &transport.fixes[..] {
            [crate::report::FixOp::SetString { pointer, value, .. }] => {
                assert_eq!(pointer, "/monitors/0/host");
                assert_eq!(value, "ppba-driver.pier1.example.com");
            }
            other => unreachable!("expected one host fix, got {other:?}"),
        }
    }

    #[test]
    fn test_ui_htmx_rp_scheme_mismatch_is_flagged_and_fixed() {
        let dir = tempfile::tempdir().unwrap();
        write_json(
            dir.path(),
            "rp.json",
            serde_json::json!({ "server": { "port": 11115,
                "tls": { "cert": "/pki/acme-cert.pem", "key": "/pki/acme-key.pem" } } }),
        );
        write_json(
            dir.path(),
            "ui-htmx.json",
            serde_json::json!({ "server": { "port": 11120 },
                "rp": { "base_url": "http://127.0.0.1:11115" } }),
        );
        let ctx = config_only_ctx(dir.path());
        let checks = client_target_joins(&ctx);
        let transport = checks
            .iter()
            .find(|c| c.name == "joins.client-transport")
            .expect("a transport mismatch must be reported");
        assert_eq!(transport.status, Status::Fail);
        assert!(
            transport.detail.contains("uses http"),
            "{}",
            transport.detail
        );
        match &transport.fixes[..] {
            [crate::report::FixOp::SetString {
                service,
                pointer,
                value,
            }] => {
                assert_eq!(service, "ui-htmx");
                assert_eq!(pointer, "/rp/base_url");
                assert_eq!(value, "https://127.0.0.1:11115");
            }
            other => unreachable!("{other:?}"),
        }
    }

    #[test]
    fn test_an_unsupported_scheme_against_a_plain_http_target_is_flagged_and_fixed() {
        let dir = tempfile::tempdir().unwrap();
        // "ftp" is neither "http" nor "https" — a naive `!= "https"`
        // comparison would treat it as equivalent to "http" and silently
        // accept it against this plain-HTTP (no tls block) target.
        write_json(
            dir.path(),
            "rp.json",
            serde_json::json!({ "server": { "port": 11115 } }),
        );
        write_json(
            dir.path(),
            "ui-htmx.json",
            serde_json::json!({ "server": { "port": 11120 },
                "rp": { "base_url": "ftp://127.0.0.1:11115" } }),
        );
        let ctx = config_only_ctx(dir.path());
        let checks = client_target_joins(&ctx);
        let transport = checks
            .iter()
            .find(|c| c.name == "joins.client-transport")
            .expect("an unsupported scheme must be reported even against a plain-HTTP target");
        assert_eq!(transport.status, Status::Fail);
        match &transport.fixes[..] {
            [crate::report::FixOp::SetString {
                service,
                pointer,
                value,
            }] => {
                assert_eq!(service, "ui-htmx");
                assert_eq!(pointer, "/rp/base_url");
                assert_eq!(value, "http://127.0.0.1:11115");
            }
            other => unreachable!("{other:?}"),
        }
    }

    #[test]
    fn test_ui_htmx_rp_flags_missing_ca_trust_for_a_self_signed_target() {
        let dir = tempfile::tempdir().unwrap();
        stage_pki(dir.path(), "s3cret-pw");
        write_json(
            dir.path(),
            "rp.json",
            serde_json::json!({ "server": { "port": 11115,
                "tls": { "cert": "/pki/rp.pem", "key": "/pki/rp-key.pem" } } }),
        );
        write_json(
            dir.path(),
            "ui-htmx.json",
            serde_json::json!({ "server": { "port": 11120 },
                "rp": { "base_url": "https://127.0.0.1:11115" } }),
        );
        let ctx = config_only_ctx(dir.path());
        let checks = client_target_joins(&ctx);
        let transport = checks
            .iter()
            .find(|c| c.name == "joins.client-transport")
            .expect("missing CA trust must be reported");
        assert_eq!(transport.status, Status::Fail);
        assert!(
            transport.detail.contains("self-signed"),
            "{}",
            transport.detail
        );
        match &transport.fixes[..] {
            [crate::report::FixOp::SetString {
                service,
                pointer,
                value,
            }] => {
                assert_eq!(service, "ui-htmx");
                assert_eq!(pointer, "/rp/ca_cert_path");
                assert!(
                    std::path::Path::new(value).ends_with("pki/ca.pem"),
                    "{value}"
                );
            }
            other => unreachable!("{other:?}"),
        }
    }

    #[test]
    fn test_ui_htmx_empty_ca_cert_path_is_treated_as_absent() {
        let dir = tempfile::tempdir().unwrap();
        stage_pki(dir.path(), "s3cret-pw");
        write_json(
            dir.path(),
            "rp.json",
            serde_json::json!({ "server": { "port": 11115,
                "tls": { "cert": "/pki/rp.pem", "key": "/pki/rp-key.pem" } } }),
        );
        write_json(
            dir.path(),
            "ui-htmx.json",
            serde_json::json!({ "server": { "port": 11120 },
                "rp": { "base_url": "https://127.0.0.1:11115", "ca_cert_path": "" } }),
        );
        let ctx = config_only_ctx(dir.path());
        let checks = client_target_joins(&ctx);
        let transport = checks
            .iter()
            .find(|c| c.name == "joins.client-transport")
            .expect("an empty ca_cert_path must not be mistaken for a working one");
        assert_eq!(transport.status, Status::Fail);
        match &transport.fixes[..] {
            [crate::report::FixOp::SetString { pointer, .. }] => {
                assert_eq!(pointer, "/rp/ca_cert_path");
            }
            other => unreachable!("{other:?}"),
        }
    }

    #[test]
    fn test_ui_htmx_rp_acme_target_needs_no_ca_cert_path() {
        // The post-flip end state: readable acme.json, the target on the
        // wildcard pair, the client on the public name — nothing to say.
        let dir = tempfile::tempdir().unwrap();
        stage_pki(dir.path(), "s3cret-pw");
        stage_acme(dir.path(), "pier1.example.com", false);
        write_json(
            dir.path(),
            "rp.json",
            serde_json::json!({ "server": { "port": 11115,
                "tls": { "cert": "/pki/acme-cert.pem", "key": "/pki/acme-key.pem" } } }),
        );
        write_json(
            dir.path(),
            "ui-htmx.json",
            serde_json::json!({ "server": { "port": 11120 },
                "rp": { "base_url": "https://rp.pier1.example.com:11115" } }),
        );
        let ctx = config_only_ctx(dir.path());
        let checks = client_target_joins(&ctx);
        assert!(
            checks.iter().all(|c| c.name != "joins.client-transport"),
            "a publicly-trusted ACME cert needs no client-side CA: {checks:?}"
        );
    }

    #[test]
    fn test_an_acme_cert_target_without_a_readable_acme_json_still_reports_the_loopback_break() {
        // The wildcard pair is what the target serves regardless of
        // whether acme.json still reads, so a loopback client URL fails
        // hostname verification either way — the break is reported, and
        // only the fix is withheld (no domain to derive the name from).
        let dir = tempfile::tempdir().unwrap();
        write_json(
            dir.path(),
            "rp.json",
            serde_json::json!({ "server": { "port": 11115,
                "tls": { "cert": "/pki/acme-cert.pem", "key": "/pki/acme-key.pem" } } }),
        );
        write_json(
            dir.path(),
            "ui-htmx.json",
            serde_json::json!({ "server": { "port": 11120 },
                "rp": { "base_url": "https://127.0.0.1:11115" } }),
        );
        for staged_acme in [None, Some("not json")] {
            if let Some(content) = staged_acme {
                std::fs::write(dir.path().join("acme.json"), content).unwrap();
            }
            let ctx = config_only_ctx(dir.path());
            let checks = client_target_joins(&ctx);
            let transport = checks
                .iter()
                .find(|c| c.name == "joins.client-transport")
                .unwrap_or_else(|| panic!("break must be reported (acme.json {staged_acme:?})"));
            assert!(
                transport.detail.contains("cannot be derived"),
                "{}",
                transport.detail
            );
            assert!(transport.fixes.is_empty(), "{:?}", transport.fixes);
        }
    }

    #[test]
    fn test_ui_htmx_rp_auth_absent_is_flagged_and_fixed() {
        let dir = tempfile::tempdir().unwrap();
        stage_pki(dir.path(), "s3cret-pw");
        let hash = rp_auth::credentials::hash_password("s3cret-pw").unwrap();
        write_json(
            dir.path(),
            "rp.json",
            serde_json::json!({ "server": { "port": 11115,
                "auth": { "username": "observatory", "password_hash": hash } } }),
        );
        write_json(
            dir.path(),
            "ui-htmx.json",
            serde_json::json!({ "server": { "port": 11120 },
                "rp": { "base_url": "http://127.0.0.1:11115" } }),
        );
        let ctx = config_only_ctx(dir.path());
        let checks = client_target_joins(&ctx);
        let auth = checks
            .iter()
            .find(|c| c.name == "joins.client-auth")
            .expect("a missing credential must be reported");
        assert_eq!(auth.status, Status::Warn);
        match &auth.fixes[..] {
            [crate::report::FixOp::SetObject {
                service,
                pointer,
                value,
            }] => {
                assert_eq!(service, "ui-htmx");
                assert_eq!(pointer, "/rp/auth");
                assert_eq!(value["username"], "observatory");
                assert_eq!(value["password"], "s3cret-pw");
            }
            other => unreachable!("{other:?}"),
        }
    }

    #[test]
    fn test_ui_htmx_rp_auth_mismatch_is_suggestion_only() {
        let dir = tempfile::tempdir().unwrap();
        let hash = rp_auth::credentials::hash_password("correct-pw").unwrap();
        write_json(
            dir.path(),
            "rp.json",
            serde_json::json!({ "server": { "port": 11115,
                "auth": { "username": "observatory", "password_hash": hash } } }),
        );
        write_json(
            dir.path(),
            "ui-htmx.json",
            serde_json::json!({ "server": { "port": 11120 },
                "rp": { "base_url": "http://127.0.0.1:11115",
                        "auth": { "username": "observatory", "password": "wrong-pw" } } }),
        );
        let ctx = config_only_ctx(dir.path());
        let checks = client_target_joins(&ctx);
        let auth = checks
            .iter()
            .find(|c| c.name == "joins.client-auth")
            .expect("a wrong credential must be reported");
        assert_eq!(auth.status, Status::Warn);
        assert!(
            auth.fixes.is_empty(),
            "a present credential is operator intent, never clobbered"
        );
    }

    #[test]
    fn test_ui_htmx_rp_matching_credential_and_scheme_is_silent() {
        // The post-flip end state with auth on: public-name URL against
        // the wildcard pair, verifying credential — nothing to say.
        let dir = tempfile::tempdir().unwrap();
        stage_acme(dir.path(), "pier1.example.com", false);
        let hash = rp_auth::credentials::hash_password("s3cret-pw").unwrap();
        write_json(
            dir.path(),
            "rp.json",
            serde_json::json!({ "server": { "port": 11115,
                "tls": { "cert": "/pki/acme-cert.pem", "key": "/pki/acme-key.pem" },
                "auth": { "username": "observatory", "password_hash": hash } } }),
        );
        write_json(
            dir.path(),
            "ui-htmx.json",
            serde_json::json!({ "server": { "port": 11120 },
                "rp": { "base_url": "https://rp.pier1.example.com:11115",
                        "auth": { "username": "observatory", "password": "s3cret-pw" } } }),
        );
        let ctx = config_only_ctx(dir.path());
        assert!(client_target_joins(&ctx).is_empty());
    }

    #[test]
    fn test_ui_htmx_sentinel_target_absent_is_a_noop() {
        let dir = tempfile::tempdir().unwrap();
        write_json(
            dir.path(),
            "sentinel.json",
            serde_json::json!({ "server": { "port": 11114,
                "tls": { "cert": "/pki/sentinel.pem", "key": "/pki/sentinel-key.pem" } } }),
        );
        write_json(
            dir.path(),
            "ui-htmx.json",
            serde_json::json!({ "server": { "port": 11120 },
                "rp": { "base_url": "http://127.0.0.1:11115" } }),
        );
        let ctx = config_only_ctx(dir.path());
        // rp itself does not participate (no config, no unit), and
        // ui-htmx's optional sentinel block is absent — nothing to join.
        assert!(client_target_joins(&ctx).is_empty());
    }

    #[test]
    fn test_non_loopback_host_is_never_joined() {
        let dir = tempfile::tempdir().unwrap();
        write_json(
            dir.path(),
            "rp.json",
            serde_json::json!({ "server": { "port": 11115,
                "tls": { "cert": "/pki/rp.pem", "key": "/pki/rp-key.pem" } } }),
        );
        write_json(
            dir.path(),
            "ui-htmx.json",
            serde_json::json!({ "server": { "port": 11120 },
                "rp": { "base_url": "http://10.0.0.5:11115" } }),
        );
        let ctx = config_only_ctx(dir.path());
        assert!(client_target_joins(&ctx).is_empty());
    }

    #[test]
    fn test_a_target_with_no_server_block_still_joins_on_its_catalog_default() {
        let dir = tempfile::tempdir().unwrap();
        // rp.json has no "server" key at all — it applies its documented
        // plain-HTTP, no-auth, catalog-default-port (11115) behavior. That
        // is a known state, not guesswork, so the join must still resolve.
        write_json(dir.path(), "rp.json", serde_json::json!({}));
        write_json(
            dir.path(),
            "ui-htmx.json",
            serde_json::json!({ "server": { "port": 11120 },
                "rp": { "base_url": "https://127.0.0.1:11115" } }),
        );
        let ctx = config_only_ctx(dir.path());
        let checks = client_target_joins(&ctx);
        let transport = checks
            .iter()
            .find(|c| c.name == "joins.client-transport")
            .expect("a plain-HTTP-by-default target against an https client must be reported");
        assert_eq!(transport.status, Status::Fail);
        assert!(
            transport.detail.contains("uses https"),
            "{}",
            transport.detail
        );
    }

    #[test]
    fn test_an_ambiguous_port_collision_is_never_joined() {
        let dir = tempfile::tempdir().unwrap();
        // rp and sentinel both claim port 11115 — ports.collision reports
        // that on its own; the join must not guess which one ui-htmx meant.
        write_json(
            dir.path(),
            "rp.json",
            serde_json::json!({ "server": { "port": 11115,
                "tls": { "cert": "/pki/rp.pem", "key": "/pki/rp-key.pem" } } }),
        );
        write_json(
            dir.path(),
            "sentinel.json",
            serde_json::json!({ "server": { "port": 11115 } }),
        );
        write_json(
            dir.path(),
            "ui-htmx.json",
            serde_json::json!({ "server": { "port": 11120 },
                "rp": { "base_url": "http://127.0.0.1:11115" } }),
        );
        let ctx = config_only_ctx(dir.path());
        assert!(client_target_joins(&ctx).is_empty());
    }

    #[test]
    fn test_rp_plate_solver_scheme_mismatch_is_flagged_and_fixed() {
        let dir = tempfile::tempdir().unwrap();
        write_json(
            dir.path(),
            "plate-solver.json",
            serde_json::json!({ "server": { "port": 11131,
                "tls": { "cert": "/pki/acme-cert.pem", "key": "/pki/acme-key.pem" } } }),
        );
        write_json(
            dir.path(),
            "rp.json",
            serde_json::json!({ "server": { "port": 11115 },
                "plate_solver": { "url": "http://localhost:11131" } }),
        );
        let ctx = config_only_ctx(dir.path());
        let checks = client_target_joins(&ctx);
        let transport = checks
            .iter()
            .find(|c| c.name == "joins.client-transport")
            .expect("a scheme mismatch must be reported");
        assert_eq!(transport.status, Status::Fail);
        match &transport.fixes[..] {
            [crate::report::FixOp::SetString {
                service,
                pointer,
                value,
            }] => {
                assert_eq!(service, "rp");
                assert_eq!(pointer, "/plate_solver/url");
                assert_eq!(value, "https://localhost:11131");
            }
            other => unreachable!("{other:?}"),
        }
    }

    #[test]
    fn test_rp_plate_solver_flags_missing_ca_trust_for_a_self_signed_target() {
        let dir = tempfile::tempdir().unwrap();
        stage_pki(dir.path(), "s3cret-pw");
        write_json(
            dir.path(),
            "plate-solver.json",
            serde_json::json!({ "server": { "port": 11131,
                "tls": { "cert": "/pki/plate-solver.pem", "key": "/pki/plate-solver-key.pem" } } }),
        );
        write_json(
            dir.path(),
            "rp.json",
            serde_json::json!({ "server": { "port": 11115 },
                "plate_solver": { "url": "https://localhost:11131" } }),
        );
        let ctx = config_only_ctx(dir.path());
        let checks = client_target_joins(&ctx);
        let transport = checks
            .iter()
            .find(|c| c.name == "joins.client-transport")
            .expect("missing CA trust must be reported");
        assert_eq!(transport.status, Status::Fail);
        assert!(
            transport.detail.contains("self-signed"),
            "{}",
            transport.detail
        );
        match &transport.fixes[..] {
            [crate::report::FixOp::SetString {
                service,
                pointer,
                value,
            }] => {
                assert_eq!(service, "rp");
                assert_eq!(pointer, "/ca_cert");
                assert!(
                    std::path::Path::new(value).ends_with("pki/ca.pem"),
                    "{value}"
                );
            }
            other => unreachable!("{other:?}"),
        }
    }

    #[test]
    fn test_rp_plate_solver_reports_missing_ca_trust_even_without_local_ca_pem() {
        let dir = tempfile::tempdir().unwrap();
        // No stage_pki: doctor's own pki/ca.pem does not exist on this
        // config dir, so the gap can only be reported, never fixed.
        write_json(
            dir.path(),
            "plate-solver.json",
            serde_json::json!({ "server": { "port": 11131,
                "tls": { "cert": "/pki/plate-solver.pem", "key": "/pki/plate-solver-key.pem" } } }),
        );
        write_json(
            dir.path(),
            "rp.json",
            serde_json::json!({ "server": { "port": 11115 },
                "plate_solver": { "url": "https://localhost:11131" } }),
        );
        let ctx = config_only_ctx(dir.path());
        let checks = client_target_joins(&ctx);
        let transport = checks
            .iter()
            .find(|c| c.name == "joins.client-transport")
            .expect("missing CA trust must be reported even when ca.pem is absent");
        assert_eq!(transport.status, Status::Fail);
        assert!(
            transport.detail.contains("self-signed") && transport.detail.contains("ca_cert"),
            "{}",
            transport.detail
        );
        assert!(
            transport.fixes.is_empty(),
            "no fix is possible without doctor's own CA material: {:?}",
            transport.fixes
        );
    }

    #[test]
    fn test_rp_ca_cert_already_present_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        stage_pki(dir.path(), "s3cret-pw");
        write_json(
            dir.path(),
            "plate-solver.json",
            serde_json::json!({ "server": { "port": 11131,
                "tls": { "cert": "/pki/plate-solver.pem", "key": "/pki/plate-solver-key.pem" } } }),
        );
        write_json(
            dir.path(),
            "rp.json",
            serde_json::json!({ "server": { "port": 11115 }, "ca_cert": "/pki/ca.pem",
                "plate_solver": { "url": "https://localhost:11131" } }),
        );
        let ctx = config_only_ctx(dir.path());
        assert!(client_target_joins(&ctx).is_empty());
    }

    #[test]
    fn test_rp_empty_ca_cert_is_treated_as_absent() {
        let dir = tempfile::tempdir().unwrap();
        stage_pki(dir.path(), "s3cret-pw");
        write_json(
            dir.path(),
            "plate-solver.json",
            serde_json::json!({ "server": { "port": 11131,
                "tls": { "cert": "/pki/plate-solver.pem", "key": "/pki/plate-solver-key.pem" } } }),
        );
        write_json(
            dir.path(),
            "rp.json",
            serde_json::json!({ "server": { "port": 11115 }, "ca_cert": "",
                "plate_solver": { "url": "https://localhost:11131" } }),
        );
        let ctx = config_only_ctx(dir.path());
        let checks = client_target_joins(&ctx);
        let transport = checks
            .iter()
            .find(|c| c.name == "joins.client-transport")
            .expect("an empty ca_cert must not be mistaken for a working one");
        assert_eq!(transport.status, Status::Fail);
        match &transport.fixes[..] {
            [crate::report::FixOp::SetString { pointer, .. }] => {
                assert_eq!(pointer, "/ca_cert");
            }
            other => unreachable!("{other:?}"),
        }
    }

    #[test]
    fn test_rp_guider_auth_absent_is_flagged_and_fixed() {
        // issue #620: `equipment.mount.guiding.auth` is now a real
        // config field, so an absent credential is fully fix-eligible —
        // same contract as ui-htmx's client targets.
        let dir = tempfile::tempdir().unwrap();
        stage_pki(dir.path(), "s3cret-pw");
        let hash = rp_auth::credentials::hash_password("s3cret-pw").unwrap();
        write_json(
            dir.path(),
            "phd2-guider.json",
            serde_json::json!({ "server": { "port": 11130,
                "auth": { "username": "observatory", "password_hash": hash } } }),
        );
        write_json(
            dir.path(),
            "rp.json",
            serde_json::json!({ "server": { "port": 11115 },
                "equipment": { "mount": { "alpaca_url": "http://localhost:11117",
                                           "guiding": { "url": "http://localhost:11130" } } } }),
        );
        let ctx = config_only_ctx(dir.path());
        let checks = client_target_joins(&ctx);
        let auth = checks
            .iter()
            .find(|c| c.name == "joins.client-auth")
            .expect("a missing credential must be reported");
        assert_eq!(auth.status, Status::Warn);
        assert!(
            auth.detail.contains("equipment.mount.guiding.url"),
            "{}",
            auth.detail
        );
        match &auth.fixes[..] {
            [crate::report::FixOp::SetObject {
                service,
                pointer,
                value,
            }] => {
                assert_eq!(service, "rp");
                assert_eq!(pointer, "/equipment/mount/guiding/auth");
                assert_eq!(value["username"], "observatory");
                assert_eq!(value["password"], "s3cret-pw");
            }
            other => unreachable!("{other:?}"),
        }
    }

    #[test]
    fn test_rp_guider_auth_mismatch_is_suggestion_only() {
        let dir = tempfile::tempdir().unwrap();
        stage_pki(dir.path(), "s3cret-pw");
        let hash = rp_auth::credentials::hash_password("correct-pw").unwrap();
        write_json(
            dir.path(),
            "phd2-guider.json",
            serde_json::json!({ "server": { "port": 11130,
                "auth": { "username": "observatory", "password_hash": hash } } }),
        );
        write_json(
            dir.path(),
            "rp.json",
            serde_json::json!({ "server": { "port": 11115 },
                "equipment": { "mount": { "alpaca_url": "http://localhost:11117",
                                           "guiding": { "url": "http://localhost:11130",
                                                        "auth": { "username": "observatory", "password": "wrong-pw" } } } } }),
        );
        let ctx = config_only_ctx(dir.path());
        let checks = client_target_joins(&ctx);
        let auth = checks
            .iter()
            .find(|c| c.name == "joins.client-auth")
            .expect("a wrong credential must be reported");
        assert_eq!(auth.status, Status::Warn);
        assert!(
            auth.fixes.is_empty(),
            "a present credential is operator intent, never clobbered"
        );
    }

    #[test]
    fn test_rp_guider_matching_credential_and_scheme_is_silent() {
        let dir = tempfile::tempdir().unwrap();
        let hash = rp_auth::credentials::hash_password("s3cret-pw").unwrap();
        write_json(
            dir.path(),
            "phd2-guider.json",
            serde_json::json!({ "server": { "port": 11130,
                "auth": { "username": "observatory", "password_hash": hash } } }),
        );
        write_json(
            dir.path(),
            "rp.json",
            serde_json::json!({ "server": { "port": 11115 }, "ca_cert": "/pki/ca.pem",
                "equipment": { "mount": { "alpaca_url": "http://localhost:11117",
                                           "guiding": { "url": "http://localhost:11130",
                                                        "auth": { "username": "observatory", "password": "s3cret-pw" } } } } }),
        );
        let ctx = config_only_ctx(dir.path());
        assert!(client_target_joins(&ctx).is_empty());
    }

    #[test]
    fn test_rp_plate_solver_auth_absent_is_flagged_and_fixed() {
        let dir = tempfile::tempdir().unwrap();
        stage_pki(dir.path(), "s3cret-pw");
        let hash = rp_auth::credentials::hash_password("s3cret-pw").unwrap();
        write_json(
            dir.path(),
            "plate-solver.json",
            serde_json::json!({ "server": { "port": 11131,
                "auth": { "username": "observatory", "password_hash": hash } } }),
        );
        write_json(
            dir.path(),
            "rp.json",
            serde_json::json!({ "server": { "port": 11115 },
                "plate_solver": { "url": "http://localhost:11131" } }),
        );
        let ctx = config_only_ctx(dir.path());
        let checks = client_target_joins(&ctx);
        let auth = checks
            .iter()
            .find(|c| c.name == "joins.client-auth")
            .expect("a missing credential must be reported");
        assert_eq!(auth.status, Status::Warn);
        match &auth.fixes[..] {
            [crate::report::FixOp::SetObject {
                service,
                pointer,
                value,
            }] => {
                assert_eq!(service, "rp");
                assert_eq!(pointer, "/plate_solver/auth");
                assert_eq!(value["username"], "observatory");
                assert_eq!(value["password"], "s3cret-pw");
            }
            other => unreachable!("{other:?}"),
        }
    }

    #[test]
    fn test_rp_plate_solver_auth_mismatch_is_suggestion_only() {
        let dir = tempfile::tempdir().unwrap();
        stage_pki(dir.path(), "s3cret-pw");
        let hash = rp_auth::credentials::hash_password("correct-pw").unwrap();
        write_json(
            dir.path(),
            "plate-solver.json",
            serde_json::json!({ "server": { "port": 11131,
                "auth": { "username": "observatory", "password_hash": hash } } }),
        );
        write_json(
            dir.path(),
            "rp.json",
            serde_json::json!({ "server": { "port": 11115 },
                "plate_solver": { "url": "http://localhost:11131",
                                   "auth": { "username": "observatory", "password": "wrong-pw" } } }),
        );
        let ctx = config_only_ctx(dir.path());
        let checks = client_target_joins(&ctx);
        let auth = checks
            .iter()
            .find(|c| c.name == "joins.client-auth")
            .expect("a wrong credential must be reported");
        assert_eq!(auth.status, Status::Warn);
        assert!(
            auth.fixes.is_empty(),
            "a present credential is operator intent, never clobbered"
        );
    }

    #[test]
    fn test_rp_plate_solver_matching_credential_and_scheme_is_silent() {
        let dir = tempfile::tempdir().unwrap();
        let hash = rp_auth::credentials::hash_password("s3cret-pw").unwrap();
        write_json(
            dir.path(),
            "plate-solver.json",
            serde_json::json!({ "server": { "port": 11131,
                "auth": { "username": "observatory", "password_hash": hash } } }),
        );
        write_json(
            dir.path(),
            "rp.json",
            serde_json::json!({ "server": { "port": 11115 }, "ca_cert": "/pki/ca.pem",
                "plate_solver": { "url": "http://localhost:11131",
                                   "auth": { "username": "observatory", "password": "s3cret-pw" } } }),
        );
        let ctx = config_only_ctx(dir.path());
        assert!(client_target_joins(&ctx).is_empty());
    }

    #[test]
    fn test_rp_equipment_mount_scheme_and_auth_are_flagged_and_fixed() {
        // issue #663: the mount's own alpaca_url (equipment.mount.alpaca_url,
        // a singular object — distinct from equipment.mount.guiding.url)
        // gets the same scheme/auth join treatment as plate_solver/guiding.
        // The target's cert is named as the ACME wildcard pair so only the
        // scheme mismatch is in play here (no CA-trust gap), mirroring
        // test_rp_plate_solver_scheme_mismatch_is_flagged_and_fixed.
        let dir = tempfile::tempdir().unwrap();
        stage_pki(dir.path(), "s3cret-pw");
        let hash = rp_auth::credentials::hash_password("s3cret-pw").unwrap();
        write_json(
            dir.path(),
            "star-adventurer-gti.json",
            serde_json::json!({ "server": { "port": 11117,
                "tls": { "cert": "/pki/acme-cert.pem", "key": "/pki/acme-key.pem" },
                "auth": { "username": "observatory", "password_hash": hash } } }),
        );
        write_json(
            dir.path(),
            "rp.json",
            serde_json::json!({ "server": { "port": 11115 },
                "equipment": { "mount": { "alpaca_url": "http://localhost:11117" } } }),
        );
        let ctx = config_only_ctx(dir.path());
        let checks = client_target_joins(&ctx);

        let transport = checks
            .iter()
            .find(|c| c.name == "joins.client-transport")
            .expect("a scheme mismatch on the mount's own connection must be reported");
        assert_eq!(transport.status, Status::Fail);
        match &transport.fixes[..] {
            [crate::report::FixOp::SetString {
                service,
                pointer,
                value,
            }] => {
                assert_eq!(service, "rp");
                assert_eq!(pointer, "/equipment/mount/alpaca_url");
                assert_eq!(value, "https://localhost:11117");
            }
            other => unreachable!("{other:?}"),
        }

        let auth = checks
            .iter()
            .find(|c| c.name == "joins.client-auth")
            .expect("a missing credential on the mount's own connection must be reported");
        assert_eq!(auth.status, Status::Warn);
        match &auth.fixes[..] {
            [crate::report::FixOp::SetObject {
                service, pointer, ..
            }] => {
                assert_eq!(service, "rp");
                assert_eq!(pointer, "/equipment/mount/auth");
            }
            other => unreachable!("{other:?}"),
        }
    }

    #[test]
    fn test_rp_equipment_camera_array_entries_are_flagged_and_fixed() {
        // issue #663: every equipment.<kind>[].alpaca_url entry (cameras
        // here) is joined the same way, indexed by its position. The
        // target's cert is named as the ACME wildcard pair so only the
        // scheme mismatch is in play here (no CA-trust gap).
        let dir = tempfile::tempdir().unwrap();
        stage_pki(dir.path(), "s3cret-pw");
        let hash = rp_auth::credentials::hash_password("s3cret-pw").unwrap();
        write_json(
            dir.path(),
            "zwo-camera.json",
            serde_json::json!({ "server": { "port": 11122,
                "tls": { "cert": "/pki/acme-cert.pem", "key": "/pki/acme-key.pem" },
                "auth": { "username": "observatory", "password_hash": hash } } }),
        );
        write_json(
            dir.path(),
            "rp.json",
            serde_json::json!({ "server": { "port": 11115 },
                "equipment": { "cameras": [
                    { "id": "main", "alpaca_url": "http://localhost:11122" }
                ] } }),
        );
        let ctx = config_only_ctx(dir.path());
        let checks = client_target_joins(&ctx);

        let transport = checks
            .iter()
            .find(|c| c.name == "joins.client-transport")
            .expect("a scheme mismatch on a camera entry must be reported");
        assert_eq!(transport.status, Status::Fail);
        match &transport.fixes[..] {
            [crate::report::FixOp::SetString {
                service,
                pointer,
                value,
            }] => {
                assert_eq!(service, "rp");
                assert_eq!(pointer, "/equipment/cameras/0/alpaca_url");
                assert_eq!(value, "https://localhost:11122");
            }
            other => unreachable!("{other:?}"),
        }

        let auth = checks
            .iter()
            .find(|c| c.name == "joins.client-auth")
            .expect("a missing credential on a camera entry must be reported");
        assert_eq!(auth.status, Status::Warn);
        match &auth.fixes[..] {
            [crate::report::FixOp::SetObject {
                service, pointer, ..
            }] => {
                assert_eq!(service, "rp");
                assert_eq!(pointer, "/equipment/cameras/0/auth");
            }
            other => unreachable!("{other:?}"),
        }
    }

    #[test]
    fn test_rp_equipment_target_matching_credential_and_scheme_is_silent() {
        let dir = tempfile::tempdir().unwrap();
        let hash = rp_auth::credentials::hash_password("s3cret-pw").unwrap();
        write_json(
            dir.path(),
            "zwo-camera.json",
            serde_json::json!({ "server": { "port": 11122,
                "auth": { "username": "observatory", "password_hash": hash } } }),
        );
        write_json(
            dir.path(),
            "rp.json",
            serde_json::json!({ "server": { "port": 11115 }, "ca_cert": "/pki/ca.pem",
                "equipment": { "cameras": [
                    { "id": "main", "alpaca_url": "http://localhost:11122",
                      "auth": { "username": "observatory", "password": "s3cret-pw" } }
                ] } }),
        );
        let ctx = config_only_ctx(dir.path());
        assert!(client_target_joins(&ctx).is_empty());
    }

    #[test]
    fn test_rp_equipment_kind_with_a_slash_gets_an_escaped_fix_pointer() {
        // Regression for the JSON-pointer-escaping gap found in adversarial
        // review of issue #663: `equipment` is opaque, so a stray '/' in a
        // kind key must not mis-segment the fix pointer (RFC 6901).
        let dir = tempfile::tempdir().unwrap();
        write_json(
            dir.path(),
            "zwo-camera.json",
            serde_json::json!({ "server": { "port": 11122,
                "tls": { "cert": "/pki/acme-cert.pem", "key": "/pki/acme-key.pem" } } }),
        );
        write_json(
            dir.path(),
            "rp.json",
            serde_json::json!({ "server": { "port": 11115 },
                "equipment": { "weird/kind": [ { "alpaca_url": "http://localhost:11122" } ] } }),
        );
        let ctx = config_only_ctx(dir.path());
        let checks = client_target_joins(&ctx);
        let transport = checks
            .iter()
            .find(|c| c.name == "joins.client-transport")
            .expect("a scheme mismatch on the odd-keyed entry must be reported");
        match &transport.fixes[..] {
            [crate::report::FixOp::SetString { pointer, .. }] => {
                assert_eq!(pointer, "/equipment/weird~1kind/0/alpaca_url");
            }
            other => unreachable!("{other:?}"),
        }
    }

    // ---- rp's plugin registrations (issue #800) ----
    //
    // The callback-URL joins: until rp's invoke and webhook clients gained
    // CA trust and a credential, TLS- or auth-enabling a plugin silently
    // broke every session start (orchestrator) or every event delivery
    // (event), and no check said so. These pin that both registrations now
    // join their target the way every other rp client target does.

    #[test]
    fn test_rp_orchestrator_plugin_scheme_and_auth_are_flagged_and_fixed() {
        let dir = tempfile::tempdir().unwrap();
        stage_pki(dir.path(), "s3cret-pw");
        let hash = rp_auth::credentials::hash_password("s3cret-pw").unwrap();
        write_json(
            dir.path(),
            "calibrator-flats.json",
            serde_json::json!({ "server": { "port": 11170,
                "tls": { "cert": "/pki/acme-cert.pem", "key": "/pki/acme-key.pem" },
                "auth": { "username": "observatory", "password_hash": hash } } }),
        );
        write_json(
            dir.path(),
            "rp.json",
            serde_json::json!({ "server": { "port": 11115 },
                "plugins": [ { "name": "calibrator-flats", "type": "orchestrator",
                               "invoke_url": "http://localhost:11170/invoke" } ] }),
        );
        let ctx = config_only_ctx(dir.path());
        let checks = client_target_joins(&ctx);

        let transport = checks
            .iter()
            .find(|c| c.name == "joins.client-transport")
            .expect("a scheme mismatch against a TLS-on plugin must be reported");
        assert_eq!(transport.status, Status::Fail);
        assert!(
            transport.detail.contains("plugins.0.invoke_url"),
            "{}",
            transport.detail
        );
        match &transport.fixes[..] {
            [crate::report::FixOp::SetString {
                service,
                pointer,
                value,
            }] => {
                assert_eq!(service, "rp");
                assert_eq!(pointer, "/plugins/0/invoke_url");
                assert_eq!(value, "https://localhost:11170/invoke");
            }
            other => unreachable!("{other:?}"),
        }

        let auth = checks
            .iter()
            .find(|c| c.name == "joins.client-auth")
            .expect("a missing plugin credential must be reported");
        assert_eq!(auth.status, Status::Warn);
        match &auth.fixes[..] {
            [crate::report::FixOp::SetObject {
                service,
                pointer,
                value,
            }] => {
                assert_eq!(service, "rp");
                assert_eq!(pointer, "/plugins/0/auth");
                assert_eq!(value["username"], "observatory");
                assert_eq!(value["password"], "s3cret-pw");
            }
            other => unreachable!("{other:?}"),
        }
    }

    #[test]
    fn test_rp_orchestrator_plugin_matching_credential_and_scheme_is_silent() {
        let dir = tempfile::tempdir().unwrap();
        let hash = rp_auth::credentials::hash_password("s3cret-pw").unwrap();
        write_json(
            dir.path(),
            "calibrator-flats.json",
            serde_json::json!({ "server": { "port": 11170,
                "auth": { "username": "observatory", "password_hash": hash } } }),
        );
        write_json(
            dir.path(),
            "rp.json",
            serde_json::json!({ "server": { "port": 11115 }, "ca_cert": "/pki/ca.pem",
                "plugins": [ { "name": "calibrator-flats", "type": "orchestrator",
                               "invoke_url": "http://localhost:11170/invoke",
                               "auth": { "username": "observatory", "password": "s3cret-pw" } } ] }),
        );
        let ctx = config_only_ctx(dir.path());
        assert!(client_target_joins(&ctx).is_empty());
    }

    // Only the registrations rp dials are walked: a tool provider is
    // reached over MCP and authenticates however its author chose, so
    // joining its `invoke_url`-shaped key would file a transport verdict
    // on a URL rp never calls and offer to write a credential rp never
    // reads.
    #[test]
    fn test_an_undialed_plugin_with_an_invoke_url_is_not_joined() {
        let dir = tempfile::tempdir().unwrap();
        stage_pki(dir.path(), "s3cret-pw");
        let hash = rp_auth::credentials::hash_password("s3cret-pw").unwrap();
        write_json(
            dir.path(),
            "calibrator-flats.json",
            serde_json::json!({ "server": { "port": 11170,
                "tls": { "cert": "/pki/acme-cert.pem", "key": "/pki/acme-key.pem" },
                "auth": { "username": "observatory", "password_hash": hash } } }),
        );
        write_json(
            dir.path(),
            "rp.json",
            serde_json::json!({ "server": { "port": 11115 },
                "plugins": [ { "name": "some-tool-provider", "type": "tool_provider",
                               "invoke_url": "http://localhost:11170/invoke" } ] }),
        );
        let ctx = config_only_ctx(dir.path());
        assert!(client_target_joins(&ctx).is_empty());
    }

    // An event plugin's `webhook_url` is the other registration rp dials,
    // so it joins on the same terms as the orchestrator's `invoke_url` —
    // both the scheme and the credential are fix-eligible.
    #[test]
    fn test_rp_event_plugin_webhook_url_is_joined() {
        let dir = tempfile::tempdir().unwrap();
        stage_pki(dir.path(), "s3cret-pw");
        let hash = rp_auth::credentials::hash_password("s3cret-pw").unwrap();
        write_json(
            dir.path(),
            "calibrator-flats.json",
            serde_json::json!({ "server": { "port": 11170,
                "tls": { "cert": "/pki/acme-cert.pem", "key": "/pki/acme-key.pem" },
                "auth": { "username": "observatory", "password_hash": hash } } }),
        );
        write_json(
            dir.path(),
            "rp.json",
            serde_json::json!({ "server": { "port": 11115 },
                "plugins": [ { "name": "image-analyzer", "type": "event",
                               "webhook_url": "http://localhost:11170/webhook",
                               "subscribes_to": ["exposure_complete"] } ] }),
        );
        let ctx = config_only_ctx(dir.path());
        let checks = client_target_joins(&ctx);

        let transport = checks
            .iter()
            .find(|c| c.name == "joins.client-transport")
            .expect("a scheme mismatch against a TLS-on plugin must be reported");
        assert_eq!(transport.status, Status::Fail);
        assert!(
            transport.detail.contains("plugins.0.webhook_url"),
            "{}",
            transport.detail
        );
        match &transport.fixes[..] {
            [crate::report::FixOp::SetString {
                service,
                pointer,
                value,
            }] => {
                assert_eq!(service, "rp");
                assert_eq!(pointer, "/plugins/0/webhook_url");
                assert_eq!(value, "https://localhost:11170/webhook");
            }
            other => unreachable!("{other:?}"),
        }

        let auth = checks
            .iter()
            .find(|c| c.name == "joins.client-auth")
            .expect("a missing plugin credential must be reported");
        assert_eq!(auth.status, Status::Warn);
        match &auth.fixes[..] {
            [crate::report::FixOp::SetObject {
                service,
                pointer,
                value,
            }] => {
                assert_eq!(service, "rp");
                assert_eq!(pointer, "/plugins/0/auth");
                assert_eq!(value["username"], "observatory");
                assert_eq!(value["password"], "s3cret-pw");
            }
            other => unreachable!("{other:?}"),
        }
    }

    // doctor joins by registration type alone and does not re-implement
    // rp's deliverability rule: rp refuses to start on an event
    // registration carrying no `subscribes_to`, so an entry doctor meets
    // on a running rig is one rp dials. A second copy of that rule here
    // could only drift from it — and did, before rp made the state
    // impossible.
    #[test]
    fn test_rp_event_plugin_is_joined_without_re_checking_deliverability() {
        let dir = tempfile::tempdir().unwrap();
        stage_pki(dir.path(), "s3cret-pw");
        let hash = rp_auth::credentials::hash_password("s3cret-pw").unwrap();
        write_json(
            dir.path(),
            "calibrator-flats.json",
            serde_json::json!({ "server": { "port": 11170,
                "tls": { "cert": "/pki/acme-cert.pem", "key": "/pki/acme-key.pem" },
                "auth": { "username": "observatory", "password_hash": hash } } }),
        );
        write_json(
            dir.path(),
            "rp.json",
            serde_json::json!({ "server": { "port": 11115 },
                "plugins": [ { "name": "image-analyzer", "type": "event",
                               "webhook_url": "http://localhost:11170/webhook" } ] }),
        );
        let ctx = config_only_ctx(dir.path());
        let checks = client_target_joins(&ctx);

        let transport = checks
            .iter()
            .find(|c| c.name == "joins.client-transport")
            .expect("an event registration is joined on its type alone");
        assert!(
            transport.detail.contains("plugins.0.webhook_url"),
            "{}",
            transport.detail
        );
    }

    #[test]
    fn test_sentinel_monitor_scheme_and_auth_are_flagged_and_fixed() {
        let dir = tempfile::tempdir().unwrap();
        stage_pki(dir.path(), "s3cret-pw");
        let hash = rp_auth::credentials::hash_password("s3cret-pw").unwrap();
        write_json(
            dir.path(),
            "ppba-driver.json",
            serde_json::json!({ "server": { "port": 11112,
                "tls": { "cert": "/pki/acme-cert.pem", "key": "/pki/acme-key.pem" },
                "auth": { "username": "observatory", "password_hash": hash } } }),
        );
        write_json(
            dir.path(),
            "sentinel.json",
            serde_json::json!({ "server": { "port": 11114 },
                "monitors": [ { "type": "alpaca_safety_monitor", "name": "PPBA",
                                 "host": "localhost", "port": 11112, "scheme": "http" } ] }),
        );
        let ctx = config_only_ctx(dir.path());
        let checks = client_target_joins(&ctx);
        let transport = checks
            .iter()
            .find(|c| c.name == "joins.client-transport")
            .expect("a monitor scheme mismatch must be reported");
        assert_eq!(transport.status, Status::Fail);
        match &transport.fixes[..] {
            [crate::report::FixOp::SetString {
                service,
                pointer,
                value,
            }] => {
                assert_eq!(service, "sentinel");
                assert_eq!(pointer, "/monitors/0/scheme");
                assert_eq!(value, "https");
            }
            other => unreachable!("{other:?}"),
        }
        let auth = checks
            .iter()
            .find(|c| c.name == "joins.client-auth")
            .expect("a missing per-monitor credential must be reported");
        match &auth.fixes[..] {
            [crate::report::FixOp::SetObject {
                service,
                pointer,
                value,
            }] => {
                assert_eq!(service, "sentinel");
                assert_eq!(pointer, "/monitors/0/auth");
                assert_eq!(value["username"], "observatory");
                assert_eq!(value["password"], "s3cret-pw");
            }
            other => unreachable!("{other:?}"),
        }
    }

    #[test]
    fn test_sentinel_watchdog_rp_url_scheme_is_flagged_without_a_duplicate_auth_check() {
        let dir = tempfile::tempdir().unwrap();
        let hash = rp_auth::credentials::hash_password("x").unwrap();
        write_json(
            dir.path(),
            "rp.json",
            serde_json::json!({ "server": { "port": 11115,
                "tls": { "cert": "/pki/acme-cert.pem", "key": "/pki/acme-key.pem" },
                "auth": { "username": "observatory", "password_hash": hash } } }),
        );
        write_json(
            dir.path(),
            "sentinel.json",
            serde_json::json!({ "server": { "port": 11114 },
                "operation_watchdog": { "rp_url": "http://localhost:11115" } }),
        );
        let ctx = config_only_ctx(dir.path());
        let checks = client_target_joins(&ctx);
        let transport = checks
            .iter()
            .find(|c| c.name == "joins.client-transport")
            .expect("the watchdog's scheme mismatch must be reported");
        match &transport.fixes[..] {
            [crate::report::FixOp::SetString {
                service,
                pointer,
                value,
            }] => {
                assert_eq!(service, "sentinel");
                assert_eq!(pointer, "/operation_watchdog/rp_url");
                assert_eq!(value, "https://localhost:11115");
            }
            other => unreachable!("{other:?}"),
        }
        // rp's auth requirement is `auth.mismatch`'s job (sentinel's
        // shared `service_auth`), not this join's.
        assert!(
            checks.iter().all(|c| c.name != "joins.client-auth"),
            "{checks:?}"
        );
    }

    #[test]
    fn test_sentinel_monitor_flags_missing_ca_trust_for_a_self_signed_target() {
        let dir = tempfile::tempdir().unwrap();
        stage_pki(dir.path(), "s3cret-pw");
        write_json(
            dir.path(),
            "ppba-driver.json",
            serde_json::json!({ "server": { "port": 11112,
                "tls": { "cert": "/pki/ppba-driver.pem", "key": "/pki/ppba-driver-key.pem" } } }),
        );
        write_json(
            dir.path(),
            "sentinel.json",
            serde_json::json!({ "server": { "port": 11114 },
                "monitors": [ { "type": "alpaca_safety_monitor", "name": "PPBA",
                                 "host": "localhost", "port": 11112, "scheme": "https" } ] }),
        );
        let ctx = config_only_ctx(dir.path());
        let checks = client_target_joins(&ctx);
        let transport = checks
            .iter()
            .find(|c| c.name == "joins.client-transport")
            .expect("a read-only run must report the monitor's missing CA trust");
        assert_eq!(transport.status, Status::Fail);
        assert!(
            transport.detail.contains("self-signed"),
            "{}",
            transport.detail
        );
        match &transport.fixes[..] {
            [crate::report::FixOp::SetString {
                service,
                pointer,
                value,
            }] => {
                assert_eq!(service, "sentinel");
                assert_eq!(pointer, "/ca_cert");
                assert!(
                    std::path::Path::new(value).ends_with("pki/ca.pem"),
                    "{value}"
                );
            }
            other => unreachable!("{other:?}"),
        }
    }

    #[test]
    fn test_sentinel_watchdog_flags_missing_ca_trust_for_a_self_signed_rp() {
        let dir = tempfile::tempdir().unwrap();
        stage_pki(dir.path(), "s3cret-pw");
        write_json(
            dir.path(),
            "rp.json",
            serde_json::json!({ "server": { "port": 11115,
                "tls": { "cert": "/pki/rp.pem", "key": "/pki/rp-key.pem" } } }),
        );
        write_json(
            dir.path(),
            "sentinel.json",
            serde_json::json!({ "server": { "port": 11114 },
                "operation_watchdog": { "rp_url": "https://localhost:11115" } }),
        );
        let ctx = config_only_ctx(dir.path());
        let checks = client_target_joins(&ctx);
        let transport = checks
            .iter()
            .find(|c| c.name == "joins.client-transport")
            .expect("a read-only run must report the watchdog's missing CA trust");
        assert_eq!(transport.status, Status::Fail);
        match &transport.fixes[..] {
            [crate::report::FixOp::SetString {
                service, pointer, ..
            }] => {
                assert_eq!(service, "sentinel");
                assert_eq!(pointer, "/ca_cert");
            }
            other => unreachable!("{other:?}"),
        }
    }

    #[test]
    fn test_sentinel_ca_cert_already_present_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        stage_pki(dir.path(), "s3cret-pw");
        write_json(
            dir.path(),
            "ppba-driver.json",
            serde_json::json!({ "server": { "port": 11112,
                "tls": { "cert": "/pki/ppba-driver.pem", "key": "/pki/ppba-driver-key.pem" } } }),
        );
        write_json(
            dir.path(),
            "sentinel.json",
            serde_json::json!({ "server": { "port": 11114 }, "ca_cert": "/pki/ca.pem",
                "monitors": [ { "type": "alpaca_safety_monitor", "name": "PPBA",
                                 "host": "localhost", "port": 11112, "scheme": "https" } ] }),
        );
        let ctx = config_only_ctx(dir.path());
        assert!(client_target_joins(&ctx).is_empty());
    }

    #[test]
    fn test_sentinel_empty_ca_cert_is_treated_as_absent() {
        let dir = tempfile::tempdir().unwrap();
        stage_pki(dir.path(), "s3cret-pw");
        write_json(
            dir.path(),
            "ppba-driver.json",
            serde_json::json!({ "server": { "port": 11112,
                "tls": { "cert": "/pki/ppba-driver.pem", "key": "/pki/ppba-driver-key.pem" } } }),
        );
        write_json(
            dir.path(),
            "sentinel.json",
            serde_json::json!({ "server": { "port": 11114 }, "ca_cert": "",
                "monitors": [ { "type": "alpaca_safety_monitor", "name": "PPBA",
                                 "host": "localhost", "port": 11112, "scheme": "https" } ] }),
        );
        let ctx = config_only_ctx(dir.path());
        let checks = client_target_joins(&ctx);
        let transport = checks
            .iter()
            .find(|c| c.name == "joins.client-transport")
            .expect("an empty ca_cert must not be mistaken for a working one");
        assert_eq!(transport.status, Status::Fail);
        match &transport.fixes[..] {
            [crate::report::FixOp::SetString { pointer, .. }] => {
                assert_eq!(pointer, "/ca_cert");
            }
            other => unreachable!("{other:?}"),
        }
    }

    #[test]
    fn test_sentinel_acme_target_needs_no_ca_trust() {
        // The post-flip end state, monitor shape: readable acme.json,
        // the target on the wildcard pair, the monitor on the public
        // name — nothing to say.
        let dir = tempfile::tempdir().unwrap();
        stage_pki(dir.path(), "s3cret-pw");
        stage_acme(dir.path(), "pier1.example.com", false);
        write_json(
            dir.path(),
            "ppba-driver.json",
            serde_json::json!({ "server": { "port": 11112,
                "tls": { "cert": "/pki/acme-cert.pem", "key": "/pki/acme-key.pem" } } }),
        );
        write_json(
            dir.path(),
            "sentinel.json",
            serde_json::json!({ "server": { "port": 11114 },
                "monitors": [ { "type": "alpaca_safety_monitor", "name": "PPBA",
                                 "host": "ppba-driver.pier1.example.com", "port": 11112,
                                 "scheme": "https" } ] }),
        );
        let ctx = config_only_ctx(dir.path());
        assert!(
            client_target_joins(&ctx).is_empty(),
            "a publicly-trusted wildcard needs no ca_cert"
        );
    }

    #[test]
    fn test_fake_mount_static_join_is_a_hard_failure() {
        let dir = tempfile::tempdir().unwrap();
        write_json(
            dir.path(),
            "planetarium-bridge.json",
            serde_json::json!({ "server": { "port": 11126 },
                "device": { "unique_id": "bridge-uid" } }),
        );
        write_json(
            dir.path(),
            "rp.json",
            serde_json::json!({ "server": { "port": 11115 },
                "equipment": { "mount": { "alpaca_url": "http://127.0.0.1:11126" } } }),
        );
        let ctx = config_only_ctx(dir.path());
        let checks = fake_mount_join(&ctx);
        match &checks[..] {
            [check] => {
                assert_eq!(check.name, "joins.fake-mount");
                assert_eq!(check.status, Status::Fail);
                assert_eq!(check.service.as_deref(), Some("rp"));
                assert!(
                    check.detail.contains("planetarium-bridge"),
                    "{}",
                    check.detail
                );
                assert!(
                    check.fixes.is_empty(),
                    "not fixable by rewriting — {checks:?}"
                );
            }
            other => unreachable!("{other:?}"),
        }
    }

    #[test]
    fn test_fake_mount_join_is_silent_for_a_real_mount_target() {
        let dir = tempfile::tempdir().unwrap();
        write_json(
            dir.path(),
            "planetarium-bridge.json",
            serde_json::json!({ "server": { "port": 11126 } }),
        );
        write_json(
            dir.path(),
            "gti-mount.json",
            serde_json::json!({ "server": { "port": 11117 } }),
        );
        write_json(
            dir.path(),
            "rp.json",
            serde_json::json!({ "server": { "port": 11115 },
                "equipment": { "mount": { "alpaca_url": "http://127.0.0.1:11117" } } }),
        );
        let ctx = config_only_ctx(dir.path());
        assert!(fake_mount_join(&ctx).is_empty());
    }

    #[test]
    fn test_fake_mount_join_skips_a_non_loopback_mount_url() {
        // The rig addresses services by host name — the static join is
        // deliberately loopback-only (the UniqueID probe leg owns this
        // case, crate::aggregate).
        let dir = tempfile::tempdir().unwrap();
        write_json(
            dir.path(),
            "planetarium-bridge.json",
            serde_json::json!({ "server": { "port": 11126 } }),
        );
        write_json(
            dir.path(),
            "rp.json",
            serde_json::json!({ "server": { "port": 11115 },
                "equipment": { "mount": {
                    "alpaca_url": "https://planetarium-bridge.rig.example:11126" } } }),
        );
        let ctx = config_only_ctx(dir.path());
        assert!(fake_mount_join(&ctx).is_empty());
    }

    // ---- ACME convergence (#805 slice 3) ----

    /// The wildcard pair on disk — the stale-material checks' gate.
    fn stage_wildcard_pair(dir: &Path) {
        let pki = dir.join("pki");
        std::fs::create_dir_all(&pki).unwrap();
        std::fs::write(pki.join("acme-cert.pem"), "cert").unwrap();
        std::fs::write(pki.join("acme-key.pem"), "key").unwrap();
    }

    /// A context whose staged facts carry a DNS story.
    fn dns_ctx(config_dir: &Path, resolvable: &[&str]) -> Context {
        let facts: PlatformFacts = serde_json::from_value(serde_json::json!({
            "platform": "linux",
            "dns": { "resolvable": resolvable },
        }))
        .unwrap();
        Context::gather(config_dir.to_path_buf(), facts)
    }

    fn named<'a>(checks: &'a [Check], name: &str) -> Vec<&'a Check> {
        checks.iter().filter(|c| c.name == name).collect()
    }

    #[test]
    fn test_a_doctor_issued_tls_pointer_is_repointed_at_the_wildcard_pair() {
        let dir = tempfile::tempdir().unwrap();
        stage_acme(dir.path(), "pier1.example.com", false);
        stage_wildcard_pair(dir.path());
        let own = crate::provision::tls_block_value(dir.path(), "ppba-driver");
        write_json(
            dir.path(),
            "ppba-driver.json",
            serde_json::json!({ "server": { "port": 11112, "tls": own } }),
        );
        let ctx = config_only_ctx(dir.path());
        let checks = acme_convergence(&ctx);
        let stale = named(&checks, "tls.stale-selfsigned-pointer");
        let check = stale.first().unwrap();
        assert_eq!(check.status, Status::Fail);
        match &check.fixes[..] {
            [crate::report::FixOp::SetString {
                pointer: cert_pointer,
                value: cert,
                ..
            }, crate::report::FixOp::SetString {
                pointer: key_pointer,
                value: key,
                ..
            }] => {
                assert_eq!(cert_pointer, "/server/tls/cert");
                assert!(cert.ends_with("acme-cert.pem"), "{cert}");
                assert_eq!(key_pointer, "/server/tls/key");
                assert!(key.ends_with("acme-key.pem"), "{key}");
            }
            other => unreachable!("expected two SetString fixes, got {other:?}"),
        }
    }

    #[test]
    fn test_a_wildcard_tls_pointer_is_already_converged_and_silent() {
        let dir = tempfile::tempdir().unwrap();
        stage_acme(dir.path(), "pier1.example.com", false);
        stage_wildcard_pair(dir.path());
        let wildcard = crate::provision::acme_tls_block_value(dir.path()).unwrap();
        write_json(
            dir.path(),
            "ppba-driver.json",
            serde_json::json!({ "server": { "port": 11112, "tls": wildcard } }),
        );
        let ctx = config_only_ctx(dir.path());
        let checks = acme_convergence(&ctx);
        assert!(named(&checks, "tls.stale-selfsigned-pointer").is_empty());
    }

    #[test]
    fn test_a_same_named_pair_outside_the_pki_tree_is_still_divergence() {
        // Converged is a path judgment, not the acme-cert.pem file-name
        // convention: a copy elsewhere is one renewal never rewrites, so
        // it quietly ages out.
        let dir = tempfile::tempdir().unwrap();
        stage_acme(dir.path(), "pier1.example.com", false);
        stage_wildcard_pair(dir.path());
        write_json(
            dir.path(),
            "ppba-driver.json",
            serde_json::json!({ "server": { "port": 11112, "tls": {
                "cert": "/operator/acme-cert.pem", "key": "/operator/acme-key.pem" } } }),
        );
        let ctx = config_only_ctx(dir.path());
        let checks = acme_convergence(&ctx);
        let stale = named(&checks, "tls.stale-selfsigned-pointer");
        let check = stale.first().unwrap();
        assert_eq!(check.status, Status::Warn);
        assert!(check.fixes.is_empty(), "{:?}", check.fixes);
    }

    #[test]
    fn test_a_hand_placed_tls_pointer_is_reported_without_a_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        stage_acme(dir.path(), "pier1.example.com", false);
        stage_wildcard_pair(dir.path());
        write_json(
            dir.path(),
            "ppba-driver.json",
            serde_json::json!({ "server": { "port": 11112, "tls": {
                "cert": "/operator/custom.pem", "key": "/operator/custom-key.pem" } } }),
        );
        let ctx = config_only_ctx(dir.path());
        let checks = acme_convergence(&ctx);
        let stale = named(&checks, "tls.stale-selfsigned-pointer");
        let check = stale.first().unwrap();
        assert_eq!(check.status, Status::Warn);
        assert!(check.fixes.is_empty(), "{:?}", check.fixes);
        assert!(check.detail.contains("operator intent"), "{}", check.detail);
        let suggestion = check.suggestion.as_deref().unwrap();
        assert!(suggestion.contains("acme-cert.pem"), "{suggestion}");
    }

    #[test]
    fn test_a_staging_acme_json_withholds_the_tls_repoint() {
        let dir = tempfile::tempdir().unwrap();
        stage_acme(dir.path(), "pier1.example.com", true);
        stage_wildcard_pair(dir.path());
        let own = crate::provision::tls_block_value(dir.path(), "ppba-driver");
        write_json(
            dir.path(),
            "ppba-driver.json",
            serde_json::json!({ "server": { "port": 11112, "tls": own } }),
        );
        let ctx = config_only_ctx(dir.path());
        let checks = acme_convergence(&ctx);
        let stale = named(&checks, "tls.stale-selfsigned-pointer");
        let check = stale.first().unwrap();
        assert_eq!(check.status, Status::Warn);
        assert!(check.fixes.is_empty(), "{:?}", check.fixes);
        assert!(check.detail.contains("staging"), "{}", check.detail);
    }

    #[test]
    fn test_the_stale_material_checks_wait_for_the_wildcard_pair() {
        let dir = tempfile::tempdir().unwrap();
        stage_acme(dir.path(), "pier1.example.com", false);
        let own = crate::provision::tls_block_value(dir.path(), "ppba-driver");
        write_json(
            dir.path(),
            "ppba-driver.json",
            serde_json::json!({ "server": { "port": 11112, "tls": own } }),
        );
        let doctor_ca =
            rusty_photon_tls::config::ca_cert_path(&crate::provision::absolute_pki_dir(dir.path()))
                .to_string_lossy()
                .into_owned();
        write_json(
            dir.path(),
            "rp.json",
            serde_json::json!({ "server": { "port": 11115 }, "ca_cert": doctor_ca }),
        );
        let ctx = config_only_ctx(dir.path());
        let checks = acme_convergence(&ctx);
        assert!(named(&checks, "tls.stale-selfsigned-pointer").is_empty());
        assert!(named(&checks, "tls.stale-ca-pin").is_empty());
    }

    #[test]
    fn test_a_doctor_written_ca_pin_gets_a_remove_key_fix() {
        let dir = tempfile::tempdir().unwrap();
        stage_acme(dir.path(), "pier1.example.com", false);
        stage_wildcard_pair(dir.path());
        let doctor_ca =
            rusty_photon_tls::config::ca_cert_path(&crate::provision::absolute_pki_dir(dir.path()))
                .to_string_lossy()
                .into_owned();
        write_json(
            dir.path(),
            "rp.json",
            serde_json::json!({ "server": { "port": 11115 }, "ca_cert": doctor_ca }),
        );
        let ctx = config_only_ctx(dir.path());
        let checks = acme_convergence(&ctx);
        let pins = named(&checks, "tls.stale-ca-pin");
        let check = pins.first().unwrap();
        assert_eq!(check.status, Status::Fail);
        match &check.fixes[..] {
            [crate::report::FixOp::RemoveKey { service, pointer }] => {
                assert_eq!(service, "rp");
                assert_eq!(pointer, "/ca_cert");
            }
            other => unreachable!("expected one RemoveKey fix, got {other:?}"),
        }
    }

    #[test]
    fn test_a_foreign_ca_pin_is_reported_without_a_fix() {
        let dir = tempfile::tempdir().unwrap();
        stage_acme(dir.path(), "pier1.example.com", false);
        stage_wildcard_pair(dir.path());
        write_json(
            dir.path(),
            "sentinel.json",
            serde_json::json!({ "server": { "port": 11114 },
                                 "ca_cert": "/etc/ssl/corp-root.pem" }),
        );
        let ctx = config_only_ctx(dir.path());
        let checks = acme_convergence(&ctx);
        let pins = named(&checks, "tls.stale-ca-pin");
        let check = pins.first().unwrap();
        assert_eq!(check.status, Status::Fail);
        assert!(check.fixes.is_empty(), "{:?}", check.fixes);
        let suggestion = check.suggestion.as_deref().unwrap();
        assert!(suggestion.contains("private CA"), "{suggestion}");
    }

    #[test]
    fn test_a_nested_ca_pin_is_judged_at_its_own_pointer() {
        let dir = tempfile::tempdir().unwrap();
        stage_acme(dir.path(), "pier1.example.com", false);
        stage_wildcard_pair(dir.path());
        let doctor_ca =
            rusty_photon_tls::config::ca_cert_path(&crate::provision::absolute_pki_dir(dir.path()))
                .to_string_lossy()
                .into_owned();
        write_json(
            dir.path(),
            "planetarium-bridge.json",
            serde_json::json!({ "server": { "port": 11126 },
                                 "rp": { "ca_cert": doctor_ca } }),
        );
        let ctx = config_only_ctx(dir.path());
        let checks = acme_convergence(&ctx);
        let pins = named(&checks, "tls.stale-ca-pin");
        let check = pins.first().unwrap();
        assert!(check.detail.contains("rp.ca_cert"), "{}", check.detail);
        match &check.fixes[..] {
            [crate::report::FixOp::RemoveKey { service, pointer }] => {
                assert_eq!(service, "planetarium-bridge");
                assert_eq!(pointer, "/rp/ca_cert");
            }
            other => unreachable!("expected one RemoveKey fix, got {other:?}"),
        }
    }

    #[test]
    fn test_a_ui_htmx_target_ca_pin_is_judged() {
        let dir = tempfile::tempdir().unwrap();
        stage_acme(dir.path(), "pier1.example.com", false);
        stage_wildcard_pair(dir.path());
        let doctor_ca =
            rusty_photon_tls::config::ca_cert_path(&crate::provision::absolute_pki_dir(dir.path()))
                .to_string_lossy()
                .into_owned();
        write_json(
            dir.path(),
            "ui-htmx.json",
            serde_json::json!({ "server": { "port": 11120 },
                                 "rp": { "base_url": "https://rp.pier1.example.com:11115",
                                          "ca_cert_path": doctor_ca } }),
        );
        let ctx = config_only_ctx(dir.path());
        let checks = acme_convergence(&ctx);
        let pins = named(&checks, "tls.stale-ca-pin");
        let check = pins.first().unwrap();
        match &check.fixes[..] {
            [crate::report::FixOp::RemoveKey { service, pointer }] => {
                assert_eq!(service, "ui-htmx");
                assert_eq!(pointer, "/rp/ca_cert_path");
            }
            other => unreachable!("expected one RemoveKey fix, got {other:?}"),
        }
    }

    #[test]
    fn test_a_staging_acme_json_downgrades_the_ca_pin() {
        let dir = tempfile::tempdir().unwrap();
        stage_acme(dir.path(), "pier1.example.com", true);
        stage_wildcard_pair(dir.path());
        let doctor_ca =
            rusty_photon_tls::config::ca_cert_path(&crate::provision::absolute_pki_dir(dir.path()))
                .to_string_lossy()
                .into_owned();
        write_json(
            dir.path(),
            "rp.json",
            serde_json::json!({ "server": { "port": 11115 }, "ca_cert": doctor_ca }),
        );
        let ctx = config_only_ctx(dir.path());
        let checks = acme_convergence(&ctx);
        let pins = named(&checks, "tls.stale-ca-pin");
        let check = pins.first().unwrap();
        assert_eq!(check.status, Status::Warn);
        assert!(check.fixes.is_empty(), "{:?}", check.fixes);
        assert!(check.detail.contains("staging"), "{}", check.detail);
    }

    #[test]
    fn test_sentinels_missing_probe_domain_gets_the_domain_fix() {
        // Deliberately no wildcard pair: the probe-domain and
        // advertised-url writes do not wait for it.
        let dir = tempfile::tempdir().unwrap();
        stage_acme(dir.path(), "pier1.example.com", false);
        write_json(
            dir.path(),
            "sentinel.json",
            serde_json::json!({ "server": { "port": 11114 } }),
        );
        let ctx = config_only_ctx(dir.path());
        let checks = acme_convergence(&ctx);
        let rows = named(&checks, "sentinel.probe-domain");
        let check = rows.first().unwrap();
        assert_eq!(check.status, Status::Warn);
        match &check.fixes[..] {
            [crate::report::FixOp::SetString {
                service,
                pointer,
                value,
            }] => {
                assert_eq!(service, "sentinel");
                assert_eq!(pointer, "/probe_domain");
                assert_eq!(value, "pier1.example.com");
            }
            other => unreachable!("expected one SetString fix, got {other:?}"),
        }
    }

    #[test]
    fn test_a_present_probe_domain_is_operator_intent() {
        let dir = tempfile::tempdir().unwrap();
        stage_acme(dir.path(), "pier1.example.com", false);
        write_json(
            dir.path(),
            "sentinel.json",
            serde_json::json!({ "server": { "port": 11114 },
                                 "probe_domain": "rig.example.net" }),
        );
        let ctx = config_only_ctx(dir.path());
        assert!(named(&acme_convergence(&ctx), "sentinel.probe-domain").is_empty());
    }

    #[test]
    fn test_a_staging_acme_json_withholds_the_probe_domain_write() {
        let dir = tempfile::tempdir().unwrap();
        stage_acme(dir.path(), "pier1.example.com", true);
        write_json(
            dir.path(),
            "sentinel.json",
            serde_json::json!({ "server": { "port": 11114 } }),
        );
        let ctx = config_only_ctx(dir.path());
        let checks = acme_convergence(&ctx);
        let rows = named(&checks, "sentinel.probe-domain");
        let check = rows.first().unwrap();
        assert!(check.fixes.is_empty(), "{:?}", check.fixes);
        assert!(check.detail.contains("staging"), "{}", check.detail);
    }

    #[test]
    fn test_an_unusable_sentinel_config_gets_no_probe_domain_verdict() {
        let dir = tempfile::tempdir().unwrap();
        stage_acme(dir.path(), "pier1.example.com", false);
        // Invalid JSON: nothing to write into.
        std::fs::write(dir.path().join("sentinel.json"), "{ not json").unwrap();
        let ctx = config_only_ctx(dir.path());
        assert!(named(&acme_convergence(&ctx), "sentinel.probe-domain").is_empty());
        // A view-shape error: config.known-blocks owns the diagnosis.
        write_json(
            dir.path(),
            "sentinel.json",
            serde_json::json!({ "server": { "port": 11114 },
                                 "operation_watchdog": { "operations": "not a map" } }),
        );
        let ctx = config_only_ctx(dir.path());
        assert!(named(&acme_convergence(&ctx), "sentinel.probe-domain").is_empty());
    }

    #[test]
    fn test_rps_missing_advertised_url_gets_the_public_name_fix() {
        let dir = tempfile::tempdir().unwrap();
        stage_acme(dir.path(), "pier1.example.com", false);
        write_json(
            dir.path(),
            "rp.json",
            serde_json::json!({ "server": { "port": 4711 } }),
        );
        let ctx = config_only_ctx(dir.path());
        let checks = acme_convergence(&ctx);
        let rows = named(&checks, "rp.advertised-url");
        let check = rows.first().unwrap();
        assert_eq!(check.status, Status::Warn);
        match &check.fixes[..] {
            [crate::report::FixOp::SetString {
                service,
                pointer,
                value,
            }] => {
                assert_eq!(service, "rp");
                assert_eq!(pointer, "/server/advertised_url");
                assert_eq!(value, "https://rp.pier1.example.com:4711");
            }
            other => unreachable!("expected one SetString fix, got {other:?}"),
        }
    }

    #[test]
    fn test_a_present_advertised_url_is_silent() {
        let dir = tempfile::tempdir().unwrap();
        stage_acme(dir.path(), "pier1.example.com", false);
        write_json(
            dir.path(),
            "rp.json",
            serde_json::json!({ "server": { "port": 11115,
                "advertised_url": "https://rp.pier1.example.com:11115" } }),
        );
        let ctx = config_only_ctx(dir.path());
        assert!(named(&acme_convergence(&ctx), "rp.advertised-url").is_empty());
    }

    #[test]
    fn test_a_missing_server_block_still_reports_the_advertised_url_gap() {
        let dir = tempfile::tempdir().unwrap();
        stage_acme(dir.path(), "pier1.example.com", false);
        write_json(dir.path(), "rp.json", serde_json::json!({}));
        let ctx = config_only_ctx(dir.path());
        let checks = acme_convergence(&ctx);
        let rows = named(&checks, "rp.advertised-url");
        let check = rows.first().unwrap();
        assert!(check.fixes.is_empty(), "{:?}", check.fixes);
        assert!(
            check.detail.contains("https://rp.pier1.example.com:11115"),
            "the catalog-default port names the derivable value: {}",
            check.detail
        );
        let suggestion = check.suggestion.as_deref().unwrap();
        assert!(suggestion.contains("server block"), "{suggestion}");
    }

    #[test]
    fn test_a_staging_acme_json_withholds_the_advertised_url_write() {
        let dir = tempfile::tempdir().unwrap();
        stage_acme(dir.path(), "pier1.example.com", true);
        write_json(
            dir.path(),
            "rp.json",
            serde_json::json!({ "server": { "port": 11115 } }),
        );
        let ctx = config_only_ctx(dir.path());
        let checks = acme_convergence(&ctx);
        let rows = named(&checks, "rp.advertised-url");
        let check = rows.first().unwrap();
        assert!(check.fixes.is_empty(), "{:?}", check.fixes);
        assert!(check.detail.contains("staging"), "{}", check.detail);
    }

    #[test]
    fn test_dns_judgment_is_skipped_without_a_dns_story() {
        let dir = tempfile::tempdir().unwrap();
        stage_acme(dir.path(), "pier1.example.com", false);
        write_json(
            dir.path(),
            "ppba-driver.json",
            serde_json::json!({ "server": { "port": 11112 } }),
        );
        let ctx = config_only_ctx(dir.path());
        assert!(ctx.facts.dns.is_none());
        assert!(dns_resolution(&ctx).is_empty());
    }

    #[test]
    fn test_all_resolving_public_names_report_ok() {
        let dir = tempfile::tempdir().unwrap();
        stage_acme(dir.path(), "pier1.example.com", false);
        write_json(
            dir.path(),
            "ppba-driver.json",
            serde_json::json!({ "server": { "port": 11112 } }),
        );
        let ctx = dns_ctx(dir.path(), &["ppba-driver.pier1.example.com"]);
        let checks = dns_resolution(&ctx);
        assert_eq!(checks.first().unwrap().status, Status::Ok);
    }

    #[test]
    fn test_unresolvable_public_names_fail_with_the_hosts_line() {
        let dir = tempfile::tempdir().unwrap();
        stage_acme(dir.path(), "pier1.example.com", false);
        write_json(
            dir.path(),
            "ppba-driver.json",
            serde_json::json!({ "server": { "port": 11112 } }),
        );
        write_json(
            dir.path(),
            "rp.json",
            serde_json::json!({ "server": { "port": 11115 } }),
        );
        let ctx = dns_ctx(dir.path(), &["rp.pier1.example.com"]);
        let checks = dns_resolution(&ctx);
        let check = checks.first().unwrap();
        assert_eq!(check.status, Status::Fail);
        assert!(
            check.detail.contains("ppba-driver.pier1.example.com"),
            "{}",
            check.detail
        );
        let suggestion = check.suggestion.as_deref().unwrap();
        assert!(
            suggestion.contains("`127.0.0.1 ppba-driver.pier1.example.com`"),
            "the resolvable name must stay off the hosts line: {suggestion}"
        );
        assert!(suggestion.contains("/etc/hosts"), "{suggestion}");
    }

    #[test]
    fn test_dns_matching_is_case_insensitive() {
        let dir = tempfile::tempdir().unwrap();
        stage_acme(dir.path(), "pier1.example.com", false);
        write_json(
            dir.path(),
            "ppba-driver.json",
            serde_json::json!({ "server": { "port": 11112 } }),
        );
        let ctx = dns_ctx(dir.path(), &["PPBA-Driver.Pier1.Example.COM"]);
        assert_eq!(dns_resolution(&ctx).first().unwrap().status, Status::Ok);
    }

    #[test]
    fn test_a_staging_acme_json_downgrades_dns_to_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        stage_acme(dir.path(), "pier1.example.com", true);
        write_json(
            dir.path(),
            "ppba-driver.json",
            serde_json::json!({ "server": { "port": 11112 } }),
        );
        let ctx = dns_ctx(dir.path(), &[]);
        let checks = dns_resolution(&ctx);
        let check = checks.first().unwrap();
        assert_eq!(check.status, Status::Warn);
        assert!(check.detail.contains("staging"), "{}", check.detail);
    }

    #[test]
    fn test_more_than_five_unresolvable_names_are_capped_in_the_detail() {
        let dir = tempfile::tempdir().unwrap();
        stage_acme(dir.path(), "pier1.example.com", false);
        for service in [
            "ppba-driver",
            "qhy-focuser",
            "dsd-fp2",
            "rp",
            "sentinel",
            "ui-htmx",
        ] {
            write_json(
                dir.path(),
                &format!("{service}.json"),
                serde_json::json!({}),
            );
        }
        let ctx = dns_ctx(dir.path(), &[]);
        let checks = dns_resolution(&ctx);
        let check = checks.first().unwrap();
        assert!(check.detail.contains("and 1 more"), "{}", check.detail);
        let suggestion = check.suggestion.as_deref().unwrap();
        assert!(
            suggestion.contains("ui-htmx.pier1.example.com"),
            "the hosts line stays complete: {suggestion}"
        );
    }

    #[test]
    fn test_gather_dns_derives_only_participating_names() {
        let dir = tempfile::tempdir().unwrap();
        stage_acme(dir.path(), "pier1.example.com", false);
        write_json(
            dir.path(),
            "ppba-driver.json",
            serde_json::json!({ "server": { "port": 11112 } }),
        );
        let ctx = config_only_ctx(dir.path());
        let asked = std::cell::RefCell::new(Vec::new());
        let dns = gather_dns(&ctx, "pier1.example.com", |name| {
            asked.borrow_mut().push(name.to_string());
            true
        });
        assert_eq!(dns.resolvable, vec!["ppba-driver.pier1.example.com"]);
        assert_eq!(asked.into_inner(), vec!["ppba-driver.pier1.example.com"]);
    }

    #[test]
    fn test_a_probe_run_without_acme_resolves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        write_json(
            dir.path(),
            "ppba-driver.json",
            serde_json::json!({ "server": { "port": 11112 } }),
        );
        let mut facts: PlatformFacts =
            serde_json::from_value(serde_json::json!({ "platform": "linux" })).unwrap();
        facts.probe_dns = true;
        let ctx = Context::gather(dir.path().to_path_buf(), facts);
        assert!(dns_resolution(&ctx).is_empty());
    }

    #[test]
    fn test_a_probe_run_with_no_participants_resolves_nothing() {
        // The (probe, no staged story) arm with nothing to derive: the
        // real resolver closure runs over zero names, so the test stays
        // deterministic without ever touching the network.
        let dir = tempfile::tempdir().unwrap();
        stage_acme(dir.path(), "pier1.example.com", false);
        let mut facts: PlatformFacts =
            serde_json::from_value(serde_json::json!({ "platform": "linux" })).unwrap();
        facts.probe_dns = true;
        let ctx = Context::gather(dir.path().to_path_buf(), facts);
        assert!(dns_resolution(&ctx).is_empty());
    }

    #[test]
    fn test_resolves_on_host_is_deterministic_for_known_names() {
        assert!(
            resolves_on_host("localhost"),
            "localhost resolves on every supported platform"
        );
        // Not the empty string: Windows' getaddrinfo resolves "" to the
        // local host. A single label past DNS's 63-octet limit is
        // rejected by every platform's resolver stack instead.
        assert!(
            !resolves_on_host(&"a".repeat(300)),
            "an oversized DNS label can never resolve"
        );
    }
}
