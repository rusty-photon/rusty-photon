//! rusty-photon-doctor CLI (docs/services/doctor.md §CLI contract).
//!
//! One-shot: diagnose (and repair, with --fix), print, exit — plus the
//! provisioning subcommands (`tls issue`, `auth rotate`,
//! `auth hash-password`). Exit 0 = no failing check (warnings allowed;
//! post-fix state on a --fix run) / the subcommand succeeded, 1 = at least
//! one failure, 2 = doctor itself could not run.

// Curated test-scope allow list — documented in the root Cargo.toml [workspace.lints] block.
#![cfg_attr(
    test,
    allow(
        clippy::needless_pass_by_ref_mut,
        clippy::needless_pass_by_value,
        clippy::unused_async,
        clippy::used_underscore_binding,
        clippy::significant_drop_tightening,
        clippy::significant_drop_in_scrutinee,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        clippy::cast_possible_wrap,
        clippy::suboptimal_flops,
        clippy::too_many_lines,
        clippy::option_if_let_else,
        clippy::match_same_arms,
        clippy::float_cmp,
        clippy::similar_names,
        clippy::struct_excessive_bools,
    )
)]

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use doctor::facts::PlatformFacts;
use doctor::report::Report;
use tracing::debug;
use tracing_subscriber::EnvFilter;

/// Diagnoses and repairs a multi-service rusty-photon install: config
/// files, ports, cross-service wiring, TLS, and the observatory
/// credential. A default run is read-only.
#[derive(Debug, Parser)]
#[command(name = "rusty-photon-doctor", version)]
struct Cli {
    /// Config directory to diagnose. Default: /etc/rusty-photon when the
    /// packaged symlink exists (Unix), else the platform config directory
    /// the services themselves use. Scopes the pki tree too.
    #[arg(long, global = true)]
    config_dir: Option<PathBuf>,
    /// Emit the `DoctorReport` JSON instead of the human-readable report.
    #[arg(long, global = true)]
    json: bool,
    /// Apply the machine-applicable fixes and the provisioning pass
    /// (certs, credential, TLS/auth-on), re-diagnose, and report the
    /// post-fix state.
    #[arg(long)]
    fix: bool,
    /// Test affordance: read platform facts from a JSON file instead of
    /// querying the host's service manager.
    #[cfg(feature = "mock")]
    #[arg(long, global = true)]
    platform_facts: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Certificate provisioning.
    Tls {
        #[command(subcommand)]
        command: TlsCommand,
    },
    /// Observatory credential management.
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
}

#[derive(Debug, Subcommand)]
enum TlsCommand {
    /// Create the CA (if absent) and a certificate pair for each installed
    /// service that lacks one, under `<config-root>/pki`. Configs are
    /// never touched — that is `--fix`'s provisioning pass.
    Issue(Box<IssueArgs>),
    /// One-shot renewal for a platform scheduler: re-issue every
    /// self-signed pair inside its 30-day window from the existing CA
    /// (never the CA itself), and re-order the ACME wildcard pair when
    /// acme.json exists and the pair is missing or due. A no-op otherwise.
    /// `<config-root>/renew.env` (KEY=VALUE per line), if present, is
    /// parsed first as a fallback so `$VAR`-indirected `dns_credentials` can
    /// resolve on an unattended run.
    Renew {
        /// Ignore the renewal windows and renew everything both legs own.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Debug, clap::Args)]
struct IssueArgs {
    /// Request a publicly-trusted wildcard certificate via ACME
    /// (DNS-01) instead of self-signed issuance.
    #[arg(long)]
    acme: bool,
    /// Base domain (the wildcard certificate covers `*.<domain>`).
    #[arg(long, requires = "acme")]
    domain: Option<String>,
    /// DNS provider for the DNS-01 challenge (supported: cloudflare).
    #[arg(long, requires = "acme")]
    dns_provider: Option<String>,
    /// DNS provider API token.
    #[arg(long, requires = "acme")]
    dns_token: Option<String>,
    /// ACME account email for expiry notifications.
    #[arg(long, requires = "acme")]
    email: Option<String>,
    /// Use the Let's Encrypt staging endpoint.
    #[arg(long, requires = "acme")]
    staging: bool,
    /// Full ACME directory URL, overriding the Let's Encrypt endpoints
    /// entirely — an internal ACME CA such as step-ca.
    #[arg(long, requires = "acme")]
    directory_url: Option<String>,
    /// PEM trust anchor for the ACME server's own TLS endpoint, which
    /// private directories need.
    #[arg(long, requires = "acme")]
    acme_root: Option<PathBuf>,
    /// Wait between writing the DNS TXT record and requesting
    /// validation (default 15).
    #[arg(long, requires = "acme")]
    dns_propagation_seconds: Option<u64>,
    /// Restrict issuance to the named services (default: the installed
    /// set, derived from the catalog).
    #[arg(long, num_args = 1..)]
    services: Vec<String>,
    /// Additional subject alternative names for the service certs.
    #[arg(long, num_args = 1..)]
    extra_san: Vec<String>,
    /// Re-issue service certificates even when a pair exists. Never
    /// re-issues the CA.
    #[arg(long)]
    force: bool,
}

#[derive(Debug, Subcommand)]
enum AuthCommand {
    /// Mint a fresh observatory credential, overwrite `pki/credential`,
    /// and re-align every installed service's `server.auth` and sentinel's
    /// `service_auth` to it.
    Rotate,
    /// Hash one password (Argon2id) for hand-written configs. Prompts with
    /// confirmation, or reads one line from stdin with --stdin.
    HashPassword {
        /// Read the password from stdin instead of prompting.
        #[arg(long)]
        stdin: bool,
    },
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();

    match &cli.command {
        None => run_diagnosis(&cli),
        Some(Command::Tls {
            command: TlsCommand::Issue(_),
        }) => run_tls_issue(&cli),
        Some(Command::Tls {
            command: TlsCommand::Renew { force },
        }) => run_tls_renew(&cli, *force),
        Some(Command::Auth {
            command: AuthCommand::Rotate,
        }) => run_auth_rotate(&cli),
        Some(Command::Auth {
            command: AuthCommand::HashPassword { stdin },
        }) => run_hash_password(*stdin),
    }
}

/// The default run: diagnose (and with --fix, repair + provision), print
/// the report, exit by the post-run state.
fn run_diagnosis(cli: &Cli) -> ExitCode {
    let (config_dir, facts) = match resolve_inputs(cli) {
        Ok(inputs) => inputs,
        Err(code) => return code,
    };

    let report = if cli.fix {
        if !facts.units.is_empty() {
            // Units installed is the strongest liveness signal doctor has
            // (the inventory carries no cross-platform active state), and
            // the canonical flow runs --fix with services live anyway:
            // atomic renames make corruption impossible, but a driver's own
            // config.apply landing mid-fix loses one of the two writes.
            eprintln!(
                "doctor: rusty-photon units are installed, so their services \
                 may be running while fixes are written — a concurrent config \
                 change can lose one write; re-run doctor to verify, and \
                 restart services to pick up fixed configs"
            );
        }
        match doctor::diagnose_and_fix(config_dir, facts) {
            Ok(report) => report,
            Err(e) => {
                eprintln!("doctor: {e}");
                return ExitCode::from(2);
            }
        }
    } else {
        doctor::diagnose(config_dir, facts)
    };
    if let Err(code) = print_report(cli, &report) {
        return code;
    }
    if report.has_failures() {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

/// `doctor tls issue`: the cert step alone — self-signed CA + per-service
/// pairs, or the ACME wildcard path with --acme. Exit 0 on success.
fn run_tls_issue(cli: &Cli) -> ExitCode {
    let Some(Command::Tls {
        command: TlsCommand::Issue(issue),
    }) = &cli.command
    else {
        return ExitCode::from(2);
    };
    let (config_dir, facts) = match resolve_inputs(cli) {
        Ok(inputs) => inputs,
        Err(code) => return code,
    };

    if issue.acme {
        let required = [
            ("--domain", &issue.domain),
            ("--dns-provider", &issue.dns_provider),
            ("--dns-token", &issue.dns_token),
            ("--email", &issue.email),
        ];
        for (flag, value) in required {
            if value.is_none() {
                eprintln!("doctor: {flag} is required with --acme");
                return ExitCode::from(2);
            }
        }
        let (Some(domain), Some(dns_provider), Some(dns_token), Some(email)) = (
            &issue.domain,
            &issue.dns_provider,
            &issue.dns_token,
            &issue.email,
        ) else {
            return ExitCode::from(2);
        };
        return run_tls_issue_acme(
            &config_dir,
            doctor::provision::AcmeArgs {
                domain: domain.clone(),
                dns_provider: dns_provider.clone(),
                dns_token: dns_token.clone(),
                email: email.clone(),
                staging: issue.staging,
                directory_url: issue.directory_url.clone(),
                acme_root: issue.acme_root.clone(),
                dns_propagation_seconds: issue.dns_propagation_seconds,
            },
        );
    }

    let ctx = doctor::checks::Context::gather(config_dir.clone(), facts);
    let service_set = if issue.services.is_empty() {
        ctx.installed_services()
    } else {
        issue.services.clone()
    };
    debug!(
        ?service_set,
        issue.force, "issuing self-signed certificates"
    );
    let applied = match doctor::provision::ensure_material(
        &config_dir,
        &service_set,
        &issue.extra_san,
        issue.force,
    ) {
        Ok(applied) => applied,
        Err(e) => {
            eprintln!("doctor: {e}");
            return ExitCode::from(2);
        }
    };

    if cli.json {
        let report = Report::new(env!("CARGO_PKG_VERSION"), ctx.mode, config_dir, Vec::new())
            .with_fixes_applied(applied);
        if let Err(code) = print_json(&report) {
            return code;
        }
    } else {
        let pki = doctor::provision::pki_dir(&config_dir);
        println!("pki tree: {}", pki.display());
        if applied.is_empty() {
            println!("nothing to issue — the CA and every requested pair already exist");
        }
        for fix in &applied {
            println!("{}", fix.op);
        }
    }
    ExitCode::SUCCESS
}

/// The ACME leg of `tls issue`. The configuration is persisted to
/// `<config-root>/acme.json` before the order is attempted — that is the
/// contract renewal picks up from, whether or not the order succeeds.
fn run_tls_issue_acme(config_dir: &std::path::Path, args: doctor::provision::AcmeArgs) -> ExitCode {
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("doctor: could not start the async runtime: {e}");
            return ExitCode::from(2);
        }
    };
    match runtime.block_on(doctor::provision::run_acme(config_dir, args)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("doctor: {e}");
            ExitCode::from(2)
        }
    }
}

/// `doctor tls renew`: the one-shot both platform timers run. Exit 0 means
/// nothing was due or everything due was renewed; warnings (a CA inside
/// its window) go to stderr either way; exit 2 means a renewal or a
/// post-renewal hook failed.
fn run_tls_renew(cli: &Cli, force: bool) -> ExitCode {
    let (config_dir, facts) = match resolve_inputs(cli) {
        Ok(inputs) => inputs,
        Err(code) => return code,
    };
    let ctx = doctor::checks::Context::gather(config_dir.clone(), facts);
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("doctor: could not start the async runtime: {e}");
            return ExitCode::from(2);
        }
    };
    debug!(force, "running certificate renewal");
    let (applied, warnings, failure) =
        match runtime.block_on(doctor::provision::renew::renew(&config_dir, force)) {
            Ok((applied, warnings)) => (applied, warnings, None),
            Err(e) => (e.applied, e.warnings, Some(e.message)),
        };
    for warning in &warnings {
        eprintln!("doctor: warning: {warning}");
    }
    if cli.json {
        let report = Report::new(env!("CARGO_PKG_VERSION"), ctx.mode, config_dir, Vec::new())
            .with_fixes_applied(applied);
        if let Err(code) = print_json(&report) {
            return code;
        }
    } else {
        if applied.is_empty() && failure.is_none() {
            println!("nothing to renew");
        }
        for fix in &applied {
            println!("{}", fix.op);
        }
    }
    failure.map_or(ExitCode::SUCCESS, |message| {
        eprintln!("doctor: {message}");
        ExitCode::from(2)
    })
}

/// `doctor auth rotate`: mint a fresh credential and re-align every copy —
/// unlike `--fix`, present `server.auth` / `service_auth` blocks are
/// overwritten. Services pick the new hash up at their next restart.
fn run_auth_rotate(cli: &Cli) -> ExitCode {
    let (config_dir, facts) = match resolve_inputs(cli) {
        Ok(inputs) => inputs,
        Err(code) => return code,
    };
    let ctx = doctor::checks::Context::gather(config_dir.clone(), facts);

    let password = match doctor::provision::mint_credential(&config_dir) {
        Ok(password) => password,
        Err(e) => {
            eprintln!("doctor: {e}");
            return ExitCode::from(2);
        }
    };
    let mut applied = vec![doctor::report::AppliedFix {
        check: "provisioning".to_string(),
        op: doctor::report::FixOp::MintCredential,
    }];

    let ops = match rotate_ops(&ctx, &password) {
        Ok(ops) => ops,
        Err(e) => {
            eprintln!("doctor: {e}");
            return ExitCode::from(2);
        }
    };
    match doctor::fix::apply_ops(&config_dir, ops, true) {
        Ok(written) => applied.extend(written),
        Err(e) => {
            eprintln!("doctor: {e}");
            return ExitCode::from(2);
        }
    }

    if cli.json {
        let report = Report::new(env!("CARGO_PKG_VERSION"), ctx.mode, config_dir, Vec::new())
            .with_fixes_applied(applied);
        if let Err(code) = print_json(&report) {
            return code;
        }
    } else {
        println!(
            "rotated the observatory credential (canonical copy: {}); \
             restart services to pick up the new hash",
            doctor::provision::credential_path(&config_dir).display()
        );
        for fix in &applied {
            println!("{}", fix.op);
        }
    }
    ExitCode::SUCCESS
}

/// The distribution ops for a rotation: the fresh hash into every
/// installed service's `server.auth`, the plaintext into sentinel's
/// `service_auth`.
fn rotate_ops(
    ctx: &doctor::checks::Context,
    password: &str,
) -> Result<Vec<(String, doctor::report::FixOp)>, String> {
    use doctor::report::FixOp;
    let mut ops = Vec::new();
    for service in ctx.installed_services() {
        let hash = rp_auth::credentials::hash_password(password)
            .map_err(|e| format!("could not hash the credential: {e}"))?;
        ops.push((
            "provisioning".to_string(),
            FixOp::SetObject {
                service: service.clone(),
                pointer: "/server/auth".to_string(),
                value: serde_json::json!({
                    "username": doctor::provision::CREDENTIAL_USERNAME,
                    "password_hash": hash,
                }),
            },
        ));
        if service == "sentinel" {
            ops.push((
                "provisioning".to_string(),
                FixOp::SetObject {
                    service,
                    pointer: "/service_auth".to_string(),
                    value: serde_json::json!({
                        "username": doctor::provision::CREDENTIAL_USERNAME,
                        "password": password,
                    }),
                },
            ));
        }
    }
    Ok(ops)
}

/// `doctor auth hash-password`: hash one password for hand-written configs
/// (the third-party-driver escape hatch).
fn run_hash_password(stdin_mode: bool) -> ExitCode {
    let password = if stdin_mode {
        debug!("reading the password from stdin");
        let mut line = String::new();
        if let Err(e) = std::io::stdin().read_line(&mut line) {
            eprintln!("doctor: could not read stdin: {e}");
            return ExitCode::from(2);
        }
        line.trim_end().to_string()
    } else {
        let password = match rpassword::prompt_password("Enter password: ") {
            Ok(password) => password,
            Err(e) => {
                eprintln!("doctor: could not read the password: {e}");
                return ExitCode::from(2);
            }
        };
        let confirm = match rpassword::prompt_password("Confirm password: ") {
            Ok(confirm) => confirm,
            Err(e) => {
                eprintln!("doctor: could not read the confirmation: {e}");
                return ExitCode::from(2);
            }
        };
        if password != confirm {
            eprintln!("doctor: passwords do not match");
            return ExitCode::from(2);
        }
        password
    };
    if password.is_empty() {
        eprintln!("doctor: password must not be empty");
        return ExitCode::from(2);
    }
    match rp_auth::credentials::hash_password(&password) {
        Ok(hash) => {
            println!("{hash}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("doctor: {e}");
            ExitCode::from(2)
        }
    }
}

/// The config dir + platform facts every config-touching path starts from.
fn resolve_inputs(cli: &Cli) -> Result<(PathBuf, PlatformFacts), ExitCode> {
    // Only mock builds can read facts from a file, which is the sole
    // fallible path; production gathering cannot fail.
    #[cfg(feature = "mock")]
    let facts = match gather_facts(cli) {
        Ok(facts) => facts,
        Err(e) => {
            eprintln!("doctor: {e}");
            return Err(ExitCode::from(2));
        }
    };
    #[cfg(not(feature = "mock"))]
    let facts = PlatformFacts::gather();
    let config_dir = match doctor::resolve_config_dir(cli.config_dir.clone()) {
        Ok(dir) => dir,
        Err(e) => {
            eprintln!("doctor: {e}");
            return Err(ExitCode::from(2));
        }
    };
    Ok((config_dir, facts))
}

fn print_report(cli: &Cli, report: &Report) -> Result<(), ExitCode> {
    if cli.json {
        print_json(report)
    } else {
        print!("{}", doctor::render::render(report));
        Ok(())
    }
}

fn print_json(report: &Report) -> Result<(), ExitCode> {
    match serde_json::to_string_pretty(report) {
        Ok(json) => {
            println!("{json}");
            Ok(())
        }
        Err(e) => {
            eprintln!("doctor: could not serialize the report: {e}");
            Err(ExitCode::from(2))
        }
    }
}

#[cfg(feature = "mock")]
fn gather_facts(cli: &Cli) -> Result<PlatformFacts, String> {
    cli.platform_facts
        .as_deref()
        .map_or_else(|| Ok(PlatformFacts::gather()), PlatformFacts::load)
}
