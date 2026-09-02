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
        clippy::unused_async_trait_impl,
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
    /// Flip an already-provisioned self-signed install to the ACME
    /// wildcard pair in one transaction: issue the pair (or accept an
    /// existing one), stage the whole convergence op plan in memory,
    /// write the changed configs, and verify the derived public names
    /// resolve.
    FlipToAcme(Box<FlipArgs>),
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
    /// DNS provider API token, persisted verbatim into acme.json. Pass a
    /// single-quoted '$NAME' (or use --dns-token-var) to store the
    /// indirection instead of the secret.
    #[arg(long, requires = "acme")]
    dns_token: Option<String>,
    /// Name of an environment variable holding the DNS provider API
    /// token. Persisted into acme.json as `$NAME` — the token itself
    /// never reaches disk; issuance and renewal resolve it from the
    /// environment (renewal falls back to renew.env).
    #[arg(
        long,
        requires = "acme",
        conflicts_with = "dns_token",
        value_name = "NAME"
    )]
    dns_token_var: Option<String>,
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

/// `tls flip-to-acme`'s flags: `tls issue --acme`'s issuance set —
/// required only when no `acme.json` exists yet — plus the flip's own
/// `--dry-run` / `--allow-staging`.
#[derive(Debug, clap::Args)]
struct FlipArgs {
    /// Base domain (the wildcard certificate covers `*.<domain>`).
    /// Required, with the other issuance flags, when no acme.json exists;
    /// must match acme.json's domain when one does.
    #[arg(long)]
    domain: Option<String>,
    /// DNS provider for the DNS-01 challenge (supported: cloudflare).
    #[arg(long)]
    dns_provider: Option<String>,
    /// DNS provider API token, persisted verbatim into acme.json. Pass a
    /// single-quoted '$NAME' (or use --dns-token-var) to store the
    /// indirection instead of the secret.
    #[arg(long)]
    dns_token: Option<String>,
    /// Name of an environment variable holding the DNS provider API
    /// token, persisted into acme.json as `$NAME`.
    #[arg(long, conflicts_with = "dns_token", value_name = "NAME")]
    dns_token_var: Option<String>,
    /// ACME account email for expiry notifications.
    #[arg(long)]
    email: Option<String>,
    /// Use the Let's Encrypt staging endpoint (refused without
    /// --allow-staging: a fleet must never converge onto
    /// publicly-untrusted certificates).
    #[arg(long)]
    staging: bool,
    /// Full ACME directory URL, overriding the Let's Encrypt endpoints
    /// entirely — an internal ACME CA such as step-ca.
    #[arg(long)]
    directory_url: Option<String>,
    /// PEM trust anchor for the ACME server's own TLS endpoint, which
    /// private directories need.
    #[arg(long)]
    acme_root: Option<PathBuf>,
    /// Wait between writing the DNS TXT record and requesting
    /// validation (default 15).
    #[arg(long)]
    dns_propagation_seconds: Option<u64>,
    /// Print the staged op plan and write nothing.
    #[arg(long)]
    dry_run: bool,
    /// Deliberately converge a staging install — the full-flip rehearsal
    /// on a disposable tree.
    #[arg(long)]
    allow_staging: bool,
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
            command: TlsCommand::FlipToAcme(flip),
        }) => run_tls_flip(&cli, flip),
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
    // Issuance materializes a missing explicit --config-dir — the ACME
    // staging rehearsal targets a scratch directory — where every other
    // entry point rejects it.
    if let Err(e) = doctor::ensure_explicit_config_dir(cli.config_dir.as_deref()) {
        eprintln!("doctor: {e}");
        return ExitCode::from(2);
    }
    let (config_dir, facts) = match resolve_inputs(cli) {
        Ok(inputs) => inputs,
        Err(code) => return code,
    };

    if issue.acme {
        let required = [
            ("--domain", &issue.domain),
            ("--dns-provider", &issue.dns_provider),
            ("--email", &issue.email),
        ];
        for (flag, value) in required {
            if value.is_none() {
                eprintln!("doctor: {flag} is required with --acme");
                return ExitCode::from(2);
            }
        }
        let dns_token = match doctor::provision::dns_token_value(
            issue.dns_token.as_deref(),
            issue.dns_token_var.as_deref(),
        ) {
            Ok((token, warning)) => {
                if let Some(warning) = warning {
                    eprintln!("doctor: warning: {warning}");
                }
                token
            }
            Err(e) => {
                eprintln!("doctor: {e}");
                return ExitCode::from(2);
            }
        };
        let (Some(domain), Some(dns_provider), Some(email)) =
            (&issue.domain, &issue.dns_provider, &issue.email)
        else {
            return ExitCode::from(2);
        };
        return run_tls_issue_acme(
            &config_dir,
            doctor::provision::AcmeArgs {
                domain: domain.clone(),
                dns_provider: dns_provider.clone(),
                dns_token,
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
    match issue_acme(config_dir, args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("doctor: {e}");
            ExitCode::from(2)
        }
    }
}

/// [`run_tls_issue_acme`]'s fallible core, shared with the flip
/// orchestrator's issuance leg.
fn issue_acme(
    config_dir: &std::path::Path,
    args: doctor::provision::AcmeArgs,
) -> Result<(), String> {
    let runtime = tokio::runtime::Runtime::new()
        .map_err(|e| format!("could not start the async runtime: {e}"))?;
    runtime.block_on(doctor::provision::run_acme(config_dir, args))
}

/// `doctor tls flip-to-acme`: the one-command self-signed → ACME
/// transition (docs/services/doctor.md §The flip orchestrator).
/// Preconditions each refuse with a named reason (exit 2) before
/// anything is written; the op plan is staged wholly in memory before
/// the first config write; the final report carries the hosts-line
/// verification and follows the standard exit-code contract.
#[allow(clippy::too_many_lines)]
fn run_tls_flip(cli: &Cli, flip: &FlipArgs) -> ExitCode {
    let (config_dir, facts) = match resolve_inputs(cli) {
        Ok(inputs) => inputs,
        Err(code) => return code,
    };

    // D4's staging guard runs before anything else — never order a
    // staging certificate, never plan against one, without the override.
    let acme_cfg = doctor::provision::active_acme_config(&config_dir);
    let staging = flip.staging || acme_cfg.as_ref().is_some_and(|c| c.staging);
    if staging && !flip.allow_staging {
        eprintln!(
            "doctor: this flip targets the ACME staging endpoint, which issues \
             publicly-untrusted certificates — a fleet must never converge onto \
             them; rehearse in a scratch --config-dir, or pass --allow-staging \
             to deliberately flip this tree anyway"
        );
        return ExitCode::from(2);
    }
    if let (Some(flag_domain), Some(cfg)) = (&flip.domain, &acme_cfg) {
        if *flag_domain != cfg.domain {
            eprintln!(
                "doctor: --domain {flag_domain} contradicts acme.json's domain \
                 {} — drop the flag to flip onto the existing configuration, or \
                 re-issue with `doctor tls issue --acme` if the domain really \
                 changed",
                cfg.domain
            );
            return ExitCode::from(2);
        }
    }

    // A FileAbsent service has nothing for the flip to write into — it
    // would be silently left behind (docs/services/doctor.md §The flip
    // orchestrator).
    let empty = std::collections::BTreeMap::new();
    let ctx = doctor::checks::Context::gather_staged(config_dir.clone(), facts.clone(), &empty);
    let missing = ctx.installed_without_config();
    if !missing.is_empty() {
        eprintln!(
            "doctor: installed services with no config file to flip: {} — start \
             each once so it self-creates its config (an empty {{}} works too), \
             then re-run",
            missing.join(", ")
        );
        return ExitCode::from(2);
    }

    // Issuance, or accept the existing pair.
    if doctor::provision::acme_tls_block_value(&config_dir).is_none() {
        if acme_cfg.is_some() {
            eprintln!(
                "doctor: acme.json exists but the wildcard pair \
                 (pki/acme-cert.pem / acme-key.pem) is missing — run `doctor \
                 tls renew` to (re)order it from the persisted settings, then \
                 re-run the flip"
            );
            return ExitCode::from(2);
        }
        let required = [
            ("--domain", &flip.domain),
            ("--dns-provider", &flip.dns_provider),
            ("--email", &flip.email),
        ];
        for (flag, value) in required {
            if value.is_none() {
                eprintln!(
                    "doctor: no acme.json exists yet, so {flag} is required to \
                     issue the wildcard pair"
                );
                return ExitCode::from(2);
            }
        }
        let dns_token = match doctor::provision::dns_token_value(
            flip.dns_token.as_deref(),
            flip.dns_token_var.as_deref(),
        ) {
            Ok((token, warning)) => {
                if let Some(warning) = warning {
                    eprintln!("doctor: warning: {warning}");
                }
                token
            }
            Err(e) => {
                eprintln!("doctor: {e}");
                return ExitCode::from(2);
            }
        };
        let (Some(domain), Some(dns_provider), Some(email)) =
            (&flip.domain, &flip.dns_provider, &flip.email)
        else {
            return ExitCode::from(2);
        };
        if flip.dry_run {
            // A dry run never orders — nor writes acme.json — so there is
            // no issued state to derive the op plan from yet. Under --json
            // stdout must stay a report, so the pending issuance is carried
            // as a warn check instead of prose.
            let detail = format!(
                "`doctor tls issue --acme` would run first for {domain}; the \
                 op plan is derived from the issued state, so re-run once the \
                 wildcard pair exists (nothing was written)"
            );
            if cli.json {
                let report = Report::new(
                    env!("CARGO_PKG_VERSION"),
                    ctx.mode,
                    config_dir,
                    vec![doctor::report::Check::warn(
                        "tls.flip-issuance-pending",
                        None,
                        detail,
                        Some(
                            "run `doctor tls flip-to-acme` without --dry-run (or \
                             `doctor tls issue --acme`), then re-run the dry run"
                                .to_string(),
                        ),
                    )],
                );
                if let Err(code) = print_json(&report) {
                    return code;
                }
            } else {
                println!("dry run: {detail}");
            }
            return ExitCode::SUCCESS;
        }
        debug!(%domain, "flip: issuing the ACME wildcard pair");
        if let Err(e) = issue_acme(
            &config_dir,
            doctor::provision::AcmeArgs {
                domain: domain.clone(),
                dns_provider: dns_provider.clone(),
                dns_token,
                email: email.clone(),
                staging: flip.staging,
                directory_url: flip.directory_url.clone(),
                acme_root: flip.acme_root.clone(),
                dns_propagation_seconds: flip.dns_propagation_seconds,
            },
        ) {
            eprintln!("doctor: {e}");
            return ExitCode::from(2);
        }
        if doctor::provision::acme_tls_block_value(&config_dir).is_none() {
            eprintln!(
                "doctor: issuance completed but the wildcard pair is still \
                 missing — run `doctor tls renew` and re-run the flip"
            );
            return ExitCode::from(2);
        }
    }

    // The whole op plan, staged in memory (D6): a planning failure costs
    // zero writes.
    let plan = match doctor::flip::plan(&config_dir, &facts, flip.allow_staging) {
        Ok(plan) => plan,
        Err(e) => {
            eprintln!("doctor: {e}");
            return ExitCode::from(2);
        }
    };

    if !flip.dry_run {
        if let Err(e) = doctor::flip::write_staged(&config_dir, &plan.staged) {
            eprintln!("doctor: {e}");
            return ExitCode::from(2);
        }
    }

    // Verification: the flip families plus the hosts-line check, from the
    // (post-write, or on a dry run current) on-disk state.
    let mut vctx = doctor::checks::Context::gather_staged(config_dir.clone(), facts, &empty);
    vctx.converge_staging = flip.allow_staging;
    let checks = doctor::flip_verification(&vctx);
    let report = Report::new(env!("CARGO_PKG_VERSION"), vctx.mode, config_dir, checks);
    let report = if flip.dry_run {
        report.with_plan(plan.ops)
    } else {
        report.with_fixes_applied(plan.ops)
    };
    if !cli.json {
        // "Already matches" is only true when the verification below is
        // clean too — an empty op plan with a failing check (an
        // unresolved public name, hand-set divergence) is not a finished
        // flip, and the banner must not claim one.
        let nothing_planned = if report.has_failures() {
            "no config changes to apply — but verification reports failures below"
        } else {
            "nothing to flip — the fleet already matches the ACME state"
        };
        if flip.dry_run {
            if report.plan.is_empty() {
                println!("{nothing_planned}");
            } else {
                println!("flip plan (dry run — nothing written):");
            }
        } else if report.fixes_applied.is_empty() {
            println!("{nothing_planned}");
        }
    }
    if let Err(code) = print_report(cli, &report) {
        return code;
    }
    if !flip.dry_run && !report.fixes_applied.is_empty() && !vctx.facts.units.is_empty() {
        eprintln!("doctor: restart services to pick up the flipped configs");
    }
    if report.has_failures() {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
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
