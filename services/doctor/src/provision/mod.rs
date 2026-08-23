//! TLS + credential provisioning (docs/services/doctor.md §Provisioning).
//!
//! Everything that *mints* material lives here: the self-signed CA and
//! per-service certificates, the ACME (DNS-01) path, and the observatory
//! credential. The serving half every service links is the
//! `rusty-photon-tls` crate; doctor is the only binary that writes the pki
//! tree. All material anchors at `<config-root>/pki` (flat — no `certs/`
//! subdirectory), with `acme.json` beside the service configs.

pub mod acme;
pub mod acme_config;
pub mod cert;
pub mod dns;
pub mod expiry;
pub mod renew;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rand::distr::{Alphanumeric, SampleString};
use rusty_photon_tls::permissions::write_restricted;
use serde_json::json;
use tracing::{debug, warn};

use crate::report::{AppliedFix, FixOp};

/// The one observatory username (ADR-016 decision 10(e)).
pub const CREDENTIAL_USERNAME: &str = "observatory";

/// 32 alphanumeric characters ≈ 190 bits of entropy — comfortably past the
/// ≥128-bit floor the design demands.
const CREDENTIAL_LENGTH: usize = 32;

/// The pki tree under the resolved config root.
#[must_use]
pub fn pki_dir(config_dir: &Path) -> PathBuf {
    config_dir.join("pki")
}

/// The canonical plaintext credential copy.
#[must_use]
pub fn credential_path(config_dir: &Path) -> PathBuf {
    pki_dir(config_dir).join("credential")
}

fn service_cert_path(pki: &Path, service: &str) -> PathBuf {
    pki.join(format!("{service}.pem"))
}

fn service_key_path(pki: &Path, service: &str) -> PathBuf {
    pki.join(format!("{service}-key.pem"))
}

/// The `server.tls` block value pointing a service at its issued pair.
#[must_use]
pub fn tls_block_value(config_dir: &Path, service: &str) -> serde_json::Value {
    let pki = absolute_pki_dir(config_dir);
    json!({
        "cert": service_cert_path(&pki, service).to_string_lossy(),
        "key": service_key_path(&pki, service).to_string_lossy(),
    })
}

/// True when the install has flipped to ACME: `<config-root>/acme.json`
/// exists — the write side of the `tls issue --acme` contract, the same
/// gate renewal's ACME leg keys on.
///
/// An ACME install's provisioning pass must hand out no self-signed
/// material (issue #616): a client's single reqwest trust configuration
/// cannot verify self-signed and publicly-trusted targets at once, so one
/// self-signed newcomer would be unreachable by every already-flipped
/// client.
#[must_use]
pub fn acme_active(config_dir: &Path) -> bool {
    config_dir.join("acme.json").is_file()
}

/// The `server.tls` block value pointing a service at the shared ACME
/// wildcard pair — what `tls.absent`'s fix writes on an ACME install.
///
/// `None` until both halves exist: `--fix` never wires paths that are not
/// there, and conjuring the pair is `tls issue --acme`'s (and renewal's)
/// job, never `--fix`'s.
#[must_use]
pub fn acme_tls_block_value(config_dir: &Path) -> Option<serde_json::Value> {
    let pki = absolute_pki_dir(config_dir);
    let cert = acme_config::acme_cert_path(&pki);
    let key = acme_config::acme_key_path(&pki);
    (cert.is_file() && key.is_file()).then(|| {
        json!({
            "cert": cert.to_string_lossy(),
            "key": key.to_string_lossy(),
        })
    })
}

/// The pki dir as an absolute path, so config-written paths stay valid
/// whatever directory a service later starts from.
#[must_use]
pub fn absolute_pki_dir(config_dir: &Path) -> PathBuf {
    std::path::absolute(pki_dir(config_dir)).unwrap_or_else(|_| pki_dir(config_dir))
}

/// Align the pki tree (and `acme.json`/`renew.env` beside the configs) with
/// the config root's owner, logging every best-effort skip.
///
/// The provisioning paths have no operator-warning channel of their own;
/// the renewal path does, and calls
/// [`align_pki_ownership_with_warnings`] instead.
///
/// # Errors
///
/// [`align_pki_ownership_with_warnings`]'s: an essential entry (material a
/// service or a renewal reads) that cannot be stat'ed or chowned to the
/// config root's owner. Never fails on a non-Unix host.
pub fn align_pki_ownership(config_dir: &Path) -> Result<(), String> {
    for warning in align_pki_ownership_with_warnings(config_dir)? {
        warn!("{warning}");
    }
    Ok(())
}

/// Align the pki tree (and `acme.json`/`renew.env` beside the configs) with
/// the config root's owner, returning the entries left as they were.
///
/// Provisioning as root on a packaged host (`sudo rusty-photon-doctor
/// --fix`) creates key material root-owned; the services — and the renewal
/// timer, which runs as the service user — could then neither read nor
/// renew it. A fresh file has no original whose owner
/// `rusty_photon_config::save` could preserve, so the tree is aligned
/// wholesale: every entry whose owner differs from the config root's is
/// chowned to match. For an unprivileged caller on its own tree every
/// owner already matches and this is a no-op. Symlinks are skipped (doctor
/// never creates one there; following it would chown the target).
///
/// Only [essential](is_essential_pki_entry) material aborts loudly — a
/// silently root-owned key breaks TLS at the next service start. Every
/// other entry in the tree is aligned best-effort and returned as a
/// warning: a file doctor did not create and nothing reads must not
/// disable unattended renewal for good.
///
/// `renew.env` is operator-authored (docs/services/doctor.md §Renewal) and
/// not always present, so it is included best-effort: it is
/// unconditionally added to `entries`, but a missing file simply fails the
/// `symlink_metadata` lookup and is skipped like any other absent entry.
///
/// # Errors
///
/// Returns a message — naming the `chown` that repairs it — if an
/// essential entry cannot be stat'ed or handed over to the config root's
/// owner. A config root that cannot be stat'ed is not an error: there is
/// nothing to align to.
#[cfg(unix)]
pub fn align_pki_ownership_with_warnings(config_dir: &Path) -> Result<Vec<String>, String> {
    use std::os::unix::fs::MetadataExt;
    let Ok(root_meta) = std::fs::metadata(config_dir) else {
        return Ok(Vec::new());
    };
    let (uid, gid) = (root_meta.uid(), root_meta.gid());
    let mut warnings = Vec::new();
    for (path, essential) in alignment_entries(config_dir) {
        let Err(problem) = align_entry(&path, uid, gid) else {
            continue;
        };
        if essential {
            return Err(format!(
                "{problem} — the services and the renewal timer run as the config \
                 root's owner and need this material; `sudo chown {uid}:{gid} {}` \
                 (or a privileged `rusty-photon-doctor --fix`) repairs it",
                path.display()
            ));
        }
        warnings.push(format!(
            "{problem} — no service and no renewal reads this file, so it was left \
             as it is; `sudo chown {uid}:{gid} {}` if it should belong to the install",
            path.display()
        ));
    }
    Ok(warnings)
}

/// The non-Unix stand-in: ownership is a Unix concept, so there is nothing
/// to align and nothing to warn about.
///
/// # Errors
///
/// Never fails; the `Result` mirrors the Unix signature so callers stay
/// platform-agnostic.
#[cfg(not(unix))]
pub fn align_pki_ownership_with_warnings(_config_dir: &Path) -> Result<Vec<String>, String> {
    Ok(Vec::new())
}

/// One entry the ownership sweep would have to hand over.
#[derive(Debug, Clone)]
pub struct OwnershipMismatch {
    pub path: PathBuf,
    /// Material a service or a renewal reads — the sweep's own split.
    pub essential: bool,
}

/// What the ownership sweep sees: the owner it aligns to, how many
/// entries exist to align, and which of them do not belong to that owner.
#[derive(Debug, Clone)]
pub struct PkiOwnership {
    pub uid: u32,
    pub gid: u32,
    /// Entries that exist. Zero means there is no material to judge — a
    /// host that has never been provisioned.
    pub examined: usize,
    pub mismatched: Vec<OwnershipMismatch>,
}

/// The read-only half of [`align_pki_ownership`], for `tls.ownership`:
/// what a privileged `--fix` would chown, without touching anything.
///
/// `None` where the question does not arise — a non-Unix host, or a
/// config root doctor cannot stat. An entry doctor cannot stat is not
/// reported: its ownership is unproven, not wrong.
#[cfg(unix)]
#[must_use]
pub fn pki_ownership(config_dir: &Path) -> Option<PkiOwnership> {
    use std::os::unix::fs::MetadataExt;
    let root_meta = std::fs::metadata(config_dir).ok()?;
    let (uid, gid) = (root_meta.uid(), root_meta.gid());
    let mut ownership = PkiOwnership {
        uid,
        gid,
        examined: 0,
        mismatched: Vec::new(),
    };
    for (path, essential) in alignment_entries(config_dir) {
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        ownership.examined = ownership.examined.saturating_add(1);
        ownership
            .mismatched
            .extend(foreign_entry(path, essential, &meta, uid, gid));
    }
    Some(ownership)
}

/// The ownership comparison itself: `Some` when the entry does not belong
/// to the owner the sweep aligns everything to.
#[cfg(unix)]
fn foreign_entry(
    path: PathBuf,
    essential: bool,
    meta: &std::fs::Metadata,
    uid: u32,
    gid: u32,
) -> Option<OwnershipMismatch> {
    use std::os::unix::fs::MetadataExt;
    (meta.uid() != uid || meta.gid() != gid).then_some(OwnershipMismatch { path, essential })
}

#[cfg(not(unix))]
pub fn pki_ownership(_config_dir: &Path) -> Option<PkiOwnership> {
    None
}

/// Everything the ownership sweep covers, each paired with whether failing
/// to align it is fatal. The pki directory itself is essential: renewal
/// writes new pairs into it. It is listed whether or not it can be
/// *listed* — a tree this run cannot read is the ownership problem most
/// worth reporting, so dropping it here would hide exactly the case the
/// sweep exists for. An absent one costs nothing: it fails the
/// `symlink_metadata` lookup later and is skipped like any other absent
/// entry.
#[cfg(unix)]
fn alignment_entries(config_dir: &Path) -> Vec<(PathBuf, bool)> {
    let pki = pki_dir(config_dir);
    let mut entries = vec![
        (config_dir.join("acme.json"), true),
        (config_dir.join("renew.env"), true),
        (pki.clone(), true),
    ];
    if let Ok(listing) = std::fs::read_dir(&pki) {
        entries.extend(listing.flatten().map(|e| {
            let path = e.path();
            let essential = is_essential_pki_entry(&path);
            (path, essential)
        }));
    }
    entries
}

/// Whether an entry in the pki tree is material a service or a renewal
/// actually reads: any certificate or key (`*.pem`), the credential, the
/// persisted ACME account.
///
/// Everything else is a stray — `ca.srl`, the serial counter
/// `openssl x509 -req -CAcreateserial` drops beside the CA when an
/// operator hand-mints a certificate for a third-party driver, a leftover
/// CSR, an editor's backup. Doctor neither writes nor reads those, so one
/// of them being unownable is not a reason to stop renewing certificates.
#[cfg(unix)]
fn is_essential_pki_entry(path: &Path) -> bool {
    let name = path.file_name().unwrap_or_default();
    path.extension().is_some_and(|e| e == "pem")
        || name == "credential"
        || name == "acme-account.json"
}

/// Hand one entry to the config root's owner. `Ok` covers everything that
/// needs nothing done: an entry that is gone, a symlink, one already
/// owned correctly. `Err` describes the host-level problem, leaving the
/// caller to decide how much it costs.
#[cfg(unix)]
fn align_entry(path: &Path, uid: u32, gid: u32) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("could not stat {}: {e}", path.display())),
    };
    if meta.file_type().is_symlink() || (meta.uid() == uid && meta.gid() == gid) {
        return Ok(());
    }
    std::os::unix::fs::chown(path, Some(uid), Some(gid)).map_err(|e| {
        format!(
            "could not chown {} to the config root's owner (uid {uid}, gid {gid}): {e}",
            path.display()
        )
    })?;
    debug!(path = %path.display(), uid, gid, "aligned ownership with the config root");
    Ok(())
}

/// Create the CA if absent and issue a certificate pair for every listed
/// service whose pair is missing. Returns the provisioning actions
/// performed.
///
/// `force` re-issues service certificates from the existing CA — never the
/// CA itself: replacing it invalidates every distributed trust anchor, so
/// that is an explicit operator act (delete `ca.pem` and `ca-key.pem`,
/// re-run with `--force` so every service pair chains to the new CA —
/// without it existing pairs are kept and still chain to the old one).
///
/// # Errors
///
/// Returns a message if the CA cannot be generated or its pair cannot be
/// read back, if a service certificate cannot be generated, or if the pki
/// tree's ownership cannot be aligned afterwards; material already written
/// by the same call stays on disk.
pub fn ensure_material(
    config_dir: &Path,
    services: &[String],
    extra_sans: &[String],
    force: bool,
) -> Result<Vec<AppliedFix>, String> {
    let pki = pki_dir(config_dir);
    let ca_cert = rusty_photon_tls::config::ca_cert_path(&pki);
    let ca_key = rusty_photon_tls::config::ca_key_path(&pki);
    let mut applied = Vec::new();

    if ca_cert.exists() && ca_key.exists() {
        debug!(ca = %ca_cert.display(), "CA exists; never regenerated");
    } else {
        cert::generate_ca(&pki).map_err(|e| format!("could not generate the CA: {e}"))?;
        applied.push(AppliedFix {
            check: "provisioning".to_string(),
            op: FixOp::GenerateCa,
        });
    }

    if services.is_empty() {
        align_pki_ownership(config_dir)?;
        return Ok(applied);
    }
    let ca_cert_pem = std::fs::read_to_string(&ca_cert)
        .map_err(|e| format!("could not read {}: {e}", ca_cert.display()))?;
    let ca_key_pem = std::fs::read_to_string(&ca_key)
        .map_err(|e| format!("could not read {}: {e}", ca_key.display()))?;

    for service in services {
        let cert_path = service_cert_path(&pki, service);
        let key_path = service_key_path(&pki, service);
        if !force && cert_path.is_file() && key_path.is_file() {
            debug!(service, "certificate pair exists; skipping");
            continue;
        }
        cert::generate_service_cert(&ca_cert_pem, &ca_key_pem, service, extra_sans, &pki)
            .map_err(|e| format!("could not generate a certificate for {service}: {e}"))?;
        applied.push(AppliedFix {
            check: "provisioning".to_string(),
            op: FixOp::GenerateCert {
                service: service.clone(),
            },
        });
    }
    align_pki_ownership(config_dir)?;
    Ok(applied)
}

/// The credential plaintext from the canonical pki copy, when present.
#[must_use]
pub fn read_credential(config_dir: &Path) -> Option<String> {
    let content = std::fs::read_to_string(credential_path(config_dir)).ok()?;
    let trimmed = content.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Reuse `pki/credential` if present, else mint and write it — a service
/// installed after the first `--fix` run is wired with the *same*
/// credential on the next run.
///
/// # Errors
///
/// [`mint_credential`]'s, on the mint leg only — an existing credential is
/// read back without touching the tree.
pub fn ensure_credential(config_dir: &Path) -> Result<(String, Vec<AppliedFix>), String> {
    if let Some(existing) = read_credential(config_dir) {
        debug!("reusing the existing observatory credential");
        return Ok((existing, Vec::new()));
    }
    let password = mint_credential(config_dir)?;
    Ok((
        password,
        vec![AppliedFix {
            check: "provisioning".to_string(),
            op: FixOp::MintCredential,
        }],
    ))
}

/// Mint a fresh credential and (over)write the canonical 0600 copy —
/// `doctor auth rotate`'s first step, and the mint leg of
/// [`ensure_credential`].
///
/// # Errors
///
/// Returns a message if the pki directory cannot be created, if the
/// credential file cannot be written (or is a symlink), or if the tree's
/// ownership cannot be aligned afterwards.
pub fn mint_credential(config_dir: &Path) -> Result<String, String> {
    let password = Alphanumeric.sample_string(&mut rand::rng(), CREDENTIAL_LENGTH);
    let path = credential_path(config_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
    }
    write_restricted(&path, format!("{password}\n").as_bytes())
        .map_err(|e| format!("could not write {}: {e}", path.display()))?;
    debug!(path = %path.display(), "wrote the observatory credential");
    align_pki_ownership(config_dir)?;
    Ok(password)
}

/// One client-wiring target: a service whose config carries the shared
/// `service_auth` / `ca_cert` field pair, and where in its config that
/// pair lives. `prefix` is a JSON-pointer prefix — empty for the
/// top-level shape (sentinel's probe client, the session-runner /
/// calibrator-flats / polar-align MCP clients — ADR-017), `"/rp"` for
/// planetarium-bridge, whose client block nests under its `rp` key
/// (planetarium-bridge.md § Configuration).
struct ClientWiring {
    service: &'static str,
    /// `false` for services with only a `ca_cert` setting — no
    /// `service_auth`, because they carry no
    /// shared-observatory-credential client role. `rp`'s outbound
    /// Alpaca / plate-solver / guider clients trust the observatory CA
    /// the same way, but device credentials are per-device `auth`
    /// blocks, not the D6 shared credential (issue #609).
    wire_auth: bool,
    prefix: &'static str,
}

const CLIENT_WIRING: &[ClientWiring] = &[
    ClientWiring {
        service: "sentinel",
        wire_auth: true,
        prefix: "",
    },
    ClientWiring {
        service: "session-runner",
        wire_auth: true,
        prefix: "",
    },
    ClientWiring {
        service: "calibrator-flats",
        wire_auth: true,
        prefix: "",
    },
    ClientWiring {
        service: "polar-align",
        wire_auth: true,
        prefix: "",
    },
    ClientWiring {
        service: "planetarium-bridge",
        wire_auth: true,
        prefix: "/rp",
    },
    ClientWiring {
        service: "rp",
        wire_auth: false,
        prefix: "",
    },
];

/// The client-block wiring `--fix` distributes into each client service's
/// config once the material exists.
///
/// That is the plaintext credential into an absent `service_auth` (skipped
/// where `wire_auth` is off) and the CA path into an absent `ca_cert`. On
/// an ACME install ([`acme_active`]) the `ca_cert` half is skipped
/// entirely: the targets are publicly trusted, and a written `ca_cert`
/// would disable the platform roots the client needs. Present (non-null)
/// blocks are operator intent and get no op. Empty when the service has
/// no usable config or the material is not there to point at.
#[must_use]
pub fn plan_client_wiring(config_dir: &Path) -> Vec<(String, FixOp)> {
    CLIENT_WIRING
        .iter()
        .flat_map(|wiring| plan_service_client_wiring(config_dir, wiring))
        .collect()
}

fn plan_service_client_wiring(config_dir: &Path, wiring: &ClientWiring) -> Vec<(String, FixOp)> {
    let path = config_dir.join(format!("{}.json", wiring.service));
    let Ok(content) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) else {
        debug!(path = %path.display(), "config is not valid JSON; no client wiring");
        return Vec::new();
    };
    // Fix ops never create intermediate structure (fix.rs), so a nested
    // client block's parent must already exist — a service self-creates
    // its full default config on first start, which is also what makes
    // the block's absence meaningful rather than "never started".
    if !wiring.prefix.is_empty()
        && !value
            .pointer(wiring.prefix)
            .is_some_and(serde_json::Value::is_object)
    {
        return Vec::new();
    }
    let mut ops = Vec::new();
    let auth_pointer = format!("{}/service_auth", wiring.prefix);
    if wiring.wire_auth
        && value
            .pointer(&auth_pointer)
            .is_none_or(serde_json::Value::is_null)
    {
        if let Some(password) = read_credential(config_dir) {
            ops.push((
                "auth.absent".to_string(),
                FixOp::SetObject {
                    service: wiring.service.to_string(),
                    pointer: auth_pointer,
                    value: json!({ "username": CREDENTIAL_USERNAME, "password": password }),
                },
            ));
        }
    }
    let ca_pointer = format!("{}/ca_cert", wiring.prefix);
    if !acme_active(config_dir)
        && value
            .pointer(&ca_pointer)
            .is_none_or(serde_json::Value::is_null)
    {
        let ca = rusty_photon_tls::config::ca_cert_path(&absolute_pki_dir(config_dir));
        if ca.is_file() {
            ops.push((
                "tls.absent".to_string(),
                FixOp::SetString {
                    service: wiring.service.to_string(),
                    pointer: ca_pointer,
                    value: ca.to_string_lossy().into_owned(),
                },
            ));
        }
    }
    ops
}

/// Everything `doctor tls issue --acme` collects from its flags. All of it
/// persists into `acme.json` — renewal must replay these settings
/// unattended.
#[derive(Debug, Clone)]
pub struct AcmeArgs {
    pub domain: String,
    pub dns_provider: String,
    pub dns_token: String,
    pub email: String,
    pub staging: bool,
    /// Overrides the Let's Encrypt endpoints entirely (an internal ACME CA,
    /// or Pebble in tests).
    pub directory_url: Option<String>,
    /// A PEM trust anchor for the ACME server's own TLS endpoint.
    pub acme_root: Option<PathBuf>,
    /// Wait between writing the TXT record and requesting validation;
    /// `None` keeps the 15s default.
    pub dns_propagation_seconds: Option<u64>,
}

/// Run the ACME issuance flow: persist `acme.json` beside the configs
/// **first**, then build the DNS provider and order a wildcard certificate
/// into the flat pki tree.
///
/// Persisting first is the contract renewal picks up from, whether or not
/// the order succeeds.
///
/// # Errors
///
/// Returns a message if `--acme-root` cannot be made absolute, if
/// `acme.json` cannot be saved, if a `$VAR` credential is not in the
/// environment, if the DNS provider cannot be built, if the ACME order
/// fails, or if the pki tree's ownership cannot be aligned (before or after
/// the order). A saved `acme.json` survives a later failure — renewal
/// retries the order from it.
pub async fn run_acme(config_dir: &Path, args: AcmeArgs) -> Result<(), String> {
    let pki = pki_dir(config_dir);

    // Persisted absolute: renewal replays acme.json from a scheduler whose
    // working directory is arbitrary, so a relative --acme-root (anchored
    // at the invoking shell's cwd, like any CLI path) must be resolved
    // now, not at 3am.
    let acme_root = args
        .acme_root
        .as_ref()
        .map(|p| {
            std::path::absolute(p)
                .map_err(|e| format!("could not resolve --acme-root {}: {e}", p.display()))
        })
        .transpose()?;

    let mut dns_credentials = std::collections::HashMap::new();
    dns_credentials.insert("api_token".to_string(), args.dns_token.clone());
    let config = acme_config::AcmeConfig {
        email: args.email.clone(),
        domain: args.domain.clone(),
        dns_provider: args.dns_provider.clone(),
        dns_credentials,
        staging: args.staging,
        renewal_days_before_expiry: 30,
        post_renewal_hooks: vec![],
        directory_url: args.directory_url.clone(),
        acme_root: acme_root.as_ref().map(|p| p.to_string_lossy().into_owned()),
        dns_propagation_seconds: args.dns_propagation_seconds.unwrap_or(15),
    };

    let config_path = config_dir.join("acme.json");
    acme_config::save_acme_config(&config, &config_path)
        .map_err(|e| format!("could not save {}: {e}", config_path.display()))?;
    debug!(path = %config_path.display(), "saved the ACME configuration");
    // Align before the order too: if it fails, acme.json is renewal's
    // recovery input, and the timer runs unprivileged.
    align_pki_ownership(config_dir)?;

    // `tls issue --acme` is interactive, run from a real shell — no
    // renew.env fallback; `$VAR` must already be in the environment.
    let resolved = acme_config::resolve_credentials(&config.dns_credentials, &HashMap::new())
        .map_err(|e| e.to_string())?;
    let dns_provider = dns::build_dns_provider(&config.dns_provider, &resolved, &config.domain)
        .await
        .map_err(|e| e.to_string())?;
    let acme_client = acme::RealAcmeClient::new(
        dns_provider.as_ref(),
        acme_root,
        std::time::Duration::from_secs(config.dns_propagation_seconds),
    );

    acme::issue_certificate(&config, &pki, &acme_client)
        .await
        .map_err(|e| e.to_string())?;
    align_pki_ownership(config_dir)?;

    println!("ACME certificate issued for *.{}:", config.domain);
    println!("  cert: {}", acme_config::acme_cert_path(&pki).display());
    println!("  key:  {}", acme_config::acme_key_path(&pki).display());
    if config.staging {
        println!("  environment: STAGING (not trusted by browsers)");
    }
    Ok(())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn services(names: &[&str]) -> Vec<String> {
        names.iter().map(std::string::ToString::to_string).collect()
    }

    #[cfg(unix)]
    #[test]
    fn test_align_pki_ownership_is_a_noop_on_a_self_owned_tree() {
        let dir = tempfile::tempdir().unwrap();
        let pki = pki_dir(dir.path());
        std::fs::create_dir_all(&pki).unwrap();
        std::fs::write(pki.join("credential"), "secret\n").unwrap();
        align_pki_ownership(dir.path()).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn test_align_pki_ownership_rehomes_a_foreign_owned_file() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        let pki = pki_dir(dir.path());
        std::fs::create_dir_all(&pki).unwrap();
        let key = pki.join("ca-key.pem");
        std::fs::write(&key, "key material").unwrap();
        let acme = dir.path().join("acme.json");
        std::fs::write(&acme, "{}").unwrap();
        // Only a privileged run (a mapped-root userns: `unshare -r
        // --map-auto` around the test binary) can create the cross-owner
        // state; unprivileged, the chowns fail and the assertions reduce
        // to the no-op case.
        let cross_owner = std::os::unix::fs::chown(&key, Some(12345), Some(12345)).is_ok();
        let _ = std::os::unix::fs::chown(&acme, Some(12345), Some(12345));
        align_pki_ownership(dir.path()).unwrap();
        let root = std::fs::metadata(dir.path()).unwrap();
        for path in [&key, &acme] {
            let meta = std::fs::metadata(path).unwrap();
            assert_eq!(meta.uid(), root.uid(), "cross-owner run: {cross_owner}");
            assert_eq!(meta.gid(), root.gid(), "cross-owner run: {cross_owner}");
        }
    }

    /// A gid from `id -G` different from `primary`, if the environment has
    /// one. An owner may hand a file to any group they belong to, so this
    /// lets the alignment chown run without privileges.
    #[cfg(unix)]
    fn supplementary_gid(primary: u32) -> Option<u32> {
        let out = std::process::Command::new("id").arg("-G").output().ok()?;
        String::from_utf8(out.stdout)
            .ok()?
            .split_whitespace()
            .filter_map(|g| g.parse().ok())
            .find(|g| *g != primary)
    }

    #[cfg(unix)]
    #[test]
    fn test_align_pki_ownership_chowns_a_group_stray_without_privileges() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        let pki = pki_dir(dir.path());
        std::fs::create_dir_all(&pki).unwrap();
        let file = pki.join("sentinel-key.pem");
        std::fs::write(&file, "key material").unwrap();
        let root = std::fs::metadata(dir.path()).unwrap();
        let Some(other) = supplementary_gid(root.gid()) else {
            eprintln!("single-group environment; the cross-owner path needs the privileged tests");
            return;
        };
        // Sandboxes with a single-mapping user namespace cannot express
        // the chgrp at all (EINVAL); plain cargo runs and real machines can.
        if std::os::unix::fs::chown(&file, None, Some(other)).is_err() {
            eprintln!("environment cannot chgrp to a supplementary group; skipping");
            return;
        }
        align_pki_ownership(dir.path()).unwrap();
        let meta = std::fs::metadata(&file).unwrap();
        assert_eq!(meta.gid(), root.gid(), "gid must return to the root's");
        assert_eq!(meta.uid(), root.uid());
    }

    #[cfg(unix)]
    #[test]
    fn test_align_pki_ownership_skips_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let pki = pki_dir(dir.path());
        std::fs::create_dir_all(&pki).unwrap();
        // A dangling symlink: without the skip, the follow-the-link chown
        // would error on the missing target and fail the alignment.
        std::os::unix::fs::symlink("/nonexistent-target", pki.join("stray-link")).unwrap();
        align_pki_ownership(dir.path()).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn test_align_pki_ownership_tolerates_a_missing_tree() {
        let dir = tempfile::tempdir().unwrap();
        align_pki_ownership(dir.path()).unwrap();
        align_pki_ownership(&dir.path().join("never-created")).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn test_essential_pki_entries_are_the_material_something_reads() {
        for material in ["ca.pem", "ca-key.pem", "zwo-camera.pem", "acme-cert.pem"] {
            assert!(is_essential_pki_entry(Path::new(material)), "{material}");
        }
        assert!(is_essential_pki_entry(Path::new("credential")));
        assert!(is_essential_pki_entry(Path::new("acme-account.json")));
        // ca.srl is openssl's serial counter from a hand-minted
        // certificate: doctor never writes it and renewal never reads it.
        for stray in ["ca.srl", "zwo-camera.csr", "ca.pem.bak", "notes.txt"] {
            assert!(!is_essential_pki_entry(Path::new(stray)), "{stray}");
        }
    }

    /// Strip search permission from the pki directory so `lstat` on its
    /// entries fails with EACCES: the alignment's host-level failure that
    /// an unprivileged test can actually produce. `None` when the process
    /// is privileged (DAC checks bypassed), where there is nothing to
    /// assert.
    #[cfg(unix)]
    fn tree_whose_entries_cannot_be_stat_ed(entry: &str) -> Option<(tempfile::TempDir, PathBuf)> {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let pki = pki_dir(dir.path());
        std::fs::create_dir_all(&pki).unwrap();
        std::fs::write(pki.join(entry), "content").unwrap();
        std::fs::set_permissions(&pki, std::fs::Permissions::from_mode(0o400)).unwrap();
        let readable = std::fs::symlink_metadata(pki.join(entry)).is_ok();
        if readable {
            std::fs::set_permissions(&pki, std::fs::Permissions::from_mode(0o700)).unwrap();
            eprintln!("running privileged; the unalignable-entry paths need an unprivileged run");
            return None;
        }
        Some((dir, pki))
    }

    #[cfg(unix)]
    #[test]
    fn test_align_pki_ownership_only_warns_about_a_stray_it_cannot_align() {
        use std::os::unix::fs::PermissionsExt;
        let Some((dir, pki)) = tree_whose_entries_cannot_be_stat_ed("ca.srl") else {
            return;
        };
        let warnings = align_pki_ownership_with_warnings(dir.path()).unwrap();
        std::fs::set_permissions(&pki, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("ca.srl"), "{warnings:?}");
        assert!(
            warnings[0].contains("chown"),
            "the warning must name the repair: {warnings:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_align_pki_ownership_fails_on_material_it_cannot_align() {
        use std::os::unix::fs::PermissionsExt;
        let Some((dir, pki)) = tree_whose_entries_cannot_be_stat_ed("sentinel-key.pem") else {
            return;
        };
        let err = align_pki_ownership_with_warnings(dir.path()).unwrap_err();
        std::fs::set_permissions(&pki, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(err.contains("sentinel-key.pem"), "{err}");
        assert!(
            err.contains("chown"),
            "the error must name the repair: {err}"
        );
    }

    /// The chown the sweep performs, exercised directly against an owner
    /// this process cannot hand a file to. Unprivileged that is EPERM —
    /// the arm the fatal/best-effort split hangs on; privileged the chown
    /// really happens and the entry is simply aligned.
    #[cfg(unix)]
    #[test]
    fn test_align_entry_reports_a_chown_it_cannot_perform() {
        const NOBODY: u32 = 65534;

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ca-key.pem");
        std::fs::write(&file, "key material").unwrap();
        if let Err(problem) = align_entry(&file, NOBODY, NOBODY) {
            assert!(problem.contains("ca-key.pem"), "{problem}");
            assert!(problem.contains("could not chown"), "{problem}");
        } else {
            use std::os::unix::fs::MetadataExt;
            let meta = std::fs::metadata(&file).unwrap();
            assert_eq!(meta.uid(), NOBODY, "a privileged run must have chowned it");
        }
    }

    /// The comparison `tls.ownership` reports on, judged against an owner
    /// that is not this process's — the packaged service user next to
    /// material an earlier `sudo` left behind. Testable as a function
    /// because a sandbox cannot create a cross-owned file to point at.
    #[cfg(unix)]
    #[test]
    fn test_an_entry_owned_by_someone_else_is_a_mismatch() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("sentinel-key.pem");
        std::fs::write(&file, "key material").unwrap();
        let meta = std::fs::symlink_metadata(&file).unwrap();

        let foreign =
            foreign_entry(file.clone(), true, &meta, meta.uid() + 1, meta.gid()).expect("mismatch");
        assert!(foreign.path.ends_with("sentinel-key.pem"));
        assert!(foreign.essential, "a key file is material something reads");
        // A group it does not share counts too: the services' access is
        // owner *and* group.
        assert!(foreign_entry(file.clone(), false, &meta, meta.uid(), meta.gid() + 1).is_some());
        assert!(
            foreign_entry(file, true, &meta, meta.uid(), meta.gid()).is_none(),
            "an entry that already belongs to the config root's owner is not a finding"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_align_pki_ownership_logs_the_strays_it_leaves_alone() {
        use std::os::unix::fs::PermissionsExt;
        let Some((dir, pki)) = tree_whose_entries_cannot_be_stat_ed("ca.srl") else {
            return;
        };
        // The logging wrapper the provisioning paths call: a stray it
        // cannot align is a log line, never a failed provisioning run.
        let aligned = align_pki_ownership(dir.path());
        std::fs::set_permissions(&pki, std::fs::Permissions::from_mode(0o700)).unwrap();
        aligned.unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn test_a_pki_tree_that_cannot_be_listed_is_still_judged() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let pki = pki_dir(dir.path());
        std::fs::create_dir_all(&pki).unwrap();
        std::fs::write(pki.join("ca.pem"), "cert").unwrap();
        std::fs::set_permissions(&pki, std::fs::Permissions::from_mode(0o000)).unwrap();

        let ownership = pki_ownership(dir.path()).unwrap();
        std::fs::set_permissions(&pki, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            ownership.examined >= 1,
            "the directory itself must be judged even when its contents cannot be \
             listed — an unreadable tree is the ownership problem, not an excuse \
             to say nothing"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_align_pki_ownership_surfaces_a_stat_error_as_err() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("acme.json"), "{}").unwrap();
        // Strip search permission from config_dir so lstat on acme.json
        // fails with EACCES rather than NotFound — the loop must
        // distinguish "gone" (tolerated) from "broken" (an error).
        std::fs::set_permissions(&config_dir, std::fs::Permissions::from_mode(0o600)).unwrap();
        let result = align_pki_ownership(&config_dir);
        std::fs::set_permissions(&config_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        // Ok means running privileged (e.g. root): DAC checks bypassed, so it's a no-op.
        if let Err(e) = result {
            assert!(e.contains("could not stat"), "unexpected error: {e}");
        }
    }

    #[test]
    fn test_plan_client_wiring_skips_a_missing_sentinel_config() {
        let dir = tempfile::tempdir().unwrap();
        assert!(plan_client_wiring(dir.path()).is_empty());
    }

    #[test]
    fn test_plan_client_wiring_skips_an_unparseable_sentinel_config() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("sentinel.json"), "{ not json").unwrap();
        assert!(plan_client_wiring(dir.path()).is_empty());
    }

    #[test]
    fn test_ensure_material_creates_ca_and_service_pairs_flat() {
        let dir = tempfile::tempdir().unwrap();
        let applied = ensure_material(dir.path(), &services(&["ppba-driver"]), &[], false).unwrap();
        let ops: Vec<String> = applied.iter().map(|a| a.op.to_string()).collect();
        assert_eq!(applied.len(), 2, "{ops:?}");
        assert!(matches!(applied[0].op, FixOp::GenerateCa));
        assert!(
            matches!(&applied[1].op, FixOp::GenerateCert { service } if service == "ppba-driver")
        );
        let pki = pki_dir(dir.path());
        for name in [
            "ca.pem",
            "ca-key.pem",
            "ppba-driver.pem",
            "ppba-driver-key.pem",
        ] {
            assert!(pki.join(name).is_file(), "missing {name}");
        }
        assert!(
            !pki.join("certs").exists(),
            "the pki tree is flat — no certs/ subdirectory"
        );
    }

    #[test]
    fn test_ensure_material_is_idempotent_and_force_reissues_certs_only() {
        let dir = tempfile::tempdir().unwrap();
        ensure_material(dir.path(), &services(&["dsd-fp2"]), &[], false).unwrap();
        let pki = pki_dir(dir.path());
        let ca_before = std::fs::read(pki.join("ca.pem")).unwrap();
        let cert_before = std::fs::read(pki.join("dsd-fp2.pem")).unwrap();

        let applied = ensure_material(dir.path(), &services(&["dsd-fp2"]), &[], false).unwrap();
        assert!(applied.is_empty(), "second run generates nothing");
        assert_eq!(std::fs::read(pki.join("dsd-fp2.pem")).unwrap(), cert_before);

        let applied = ensure_material(dir.path(), &services(&["dsd-fp2"]), &[], true).unwrap();
        assert_eq!(applied.len(), 1, "--force re-issues the service cert");
        assert!(matches!(&applied[0].op, FixOp::GenerateCert { .. }));
        assert_ne!(std::fs::read(pki.join("dsd-fp2.pem")).unwrap(), cert_before);
        assert_eq!(
            std::fs::read(pki.join("ca.pem")).unwrap(),
            ca_before,
            "--force never touches the CA"
        );
    }

    #[test]
    fn test_ensure_credential_mints_once_and_reuses() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_credential(dir.path()).is_none());
        let (password, applied) = ensure_credential(dir.path()).unwrap();
        assert_eq!(password.len(), CREDENTIAL_LENGTH);
        assert!(password.chars().all(|c| c.is_ascii_alphanumeric()));
        assert_eq!(applied.len(), 1);
        assert!(matches!(applied[0].op, FixOp::MintCredential));

        let (again, applied) = ensure_credential(dir.path()).unwrap();
        assert_eq!(again, password, "the canonical copy is reused");
        assert!(applied.is_empty());
        assert_eq!(read_credential(dir.path()).unwrap(), password);
    }

    #[cfg(unix)]
    #[test]
    fn test_credential_file_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        ensure_credential(dir.path()).unwrap();
        let mode = std::fs::metadata(credential_path(dir.path()))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "credential mode {mode:o}");
    }

    #[test]
    fn test_mint_credential_overwrites_for_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let first = mint_credential(dir.path()).unwrap();
        let second = mint_credential(dir.path()).unwrap();
        assert_ne!(first, second);
        assert_eq!(read_credential(dir.path()).unwrap(), second);
    }

    #[test]
    fn test_plan_client_wiring_wires_absent_blocks_only() {
        let dir = tempfile::tempdir().unwrap();
        // No sentinel.json: nothing to wire.
        assert!(plan_client_wiring(dir.path()).is_empty());

        std::fs::write(
            dir.path().join("sentinel.json"),
            r#"{ "server": { "port": 11114 }, "ca_cert": null }"#,
        )
        .unwrap();
        // Material absent: nothing to point at yet.
        assert!(plan_client_wiring(dir.path()).is_empty());

        ensure_material(dir.path(), &[], &[], false).unwrap();
        ensure_credential(dir.path()).unwrap();
        let ops = plan_client_wiring(dir.path());
        assert_eq!(ops.len(), 2, "{ops:?}");
        assert!(matches!(
            &ops[0].1,
            FixOp::SetObject { service, pointer, .. }
                if service == "sentinel" && pointer == "/service_auth"
        ));
        assert!(matches!(
            &ops[1].1,
            FixOp::SetString { service, pointer, .. }
                if service == "sentinel" && pointer == "/ca_cert"
        ));

        // Present blocks are never re-planned.
        std::fs::write(
            dir.path().join("sentinel.json"),
            r#"{ "service_auth": { "username": "u", "password": "p" }, "ca_cert": "/x/ca.pem" }"#,
        )
        .unwrap();
        assert!(plan_client_wiring(dir.path()).is_empty());
    }

    #[test]
    fn test_plan_client_wiring_covers_the_mcp_client_services() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("session-runner.json"),
            r#"{ "workflows_dir": "/w", "state_dir": "/s" }"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("calibrator-flats.json"),
            r#"{ "camera_id": "c", "filter_wheel_id": "f", "calibrator_id": "cc", "filters": [] }"#,
        )
        .unwrap();
        ensure_material(dir.path(), &[], &[], false).unwrap();
        ensure_credential(dir.path()).unwrap();

        // No sentinel.json staged: exactly the two MCP clients get wired,
        // each with both halves.
        let ops = plan_client_wiring(dir.path());
        assert_eq!(ops.len(), 4, "{ops:?}");
        for wired in ["session-runner", "calibrator-flats"] {
            assert!(
                ops.iter().any(|(check, op)| check == "auth.absent"
                    && matches!(op, FixOp::SetObject { service, pointer, .. }
                        if service == wired && pointer == "/service_auth")),
                "missing service_auth wiring for {wired}: {ops:?}"
            );
            assert!(
                ops.iter().any(|(check, op)| check == "tls.absent"
                    && matches!(op, FixOp::SetString { service, pointer, .. }
                        if service == wired && pointer == "/ca_cert")),
                "missing ca_cert wiring for {wired}: {ops:?}"
            );
        }
    }

    #[test]
    fn test_plan_client_wiring_wires_the_bridge_nested_rp_block() {
        // planetarium-bridge's client pair nests under its `rp` key
        // (planetarium-bridge.md § Configuration), so its wiring ops
        // carry the nested pointers — and are planned only when the
        // `rp` block exists, since fix ops never create parents.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("planetarium-bridge.json"),
            r#"{ "server": { "port": 11126 },
                 "rp": { "mcp_server_url": "http://127.0.0.1:11115/mcp" } }"#,
        )
        .unwrap();
        ensure_material(dir.path(), &[], &[], false).unwrap();
        ensure_credential(dir.path()).unwrap();

        let ops = plan_client_wiring(dir.path());
        assert_eq!(ops.len(), 2, "{ops:?}");
        assert!(
            ops.iter().any(|(check, op)| check == "auth.absent"
                && matches!(op, FixOp::SetObject { service, pointer, .. }
                    if service == "planetarium-bridge" && pointer == "/rp/service_auth")),
            "{ops:?}"
        );
        assert!(
            ops.iter().any(|(check, op)| check == "tls.absent"
                && matches!(op, FixOp::SetString { service, pointer, .. }
                    if service == "planetarium-bridge" && pointer == "/rp/ca_cert")),
            "{ops:?}"
        );

        // Without the `rp` parent the ops could never apply — none are
        // planned.
        std::fs::write(
            dir.path().join("planetarium-bridge.json"),
            r#"{ "server": { "port": 11126 } }"#,
        )
        .unwrap();
        assert!(plan_client_wiring(dir.path()).is_empty());
    }

    #[test]
    fn test_plan_client_wiring_wires_rp_ca_cert_only_not_service_auth() {
        // rp (issue #609) is CA-only: it has no shared-observatory-
        // credential client role, so `--fix` must never propose a
        // `/service_auth` op for it even once the credential exists —
        // rp's `deny_unknown_fields` Config has no such field and would
        // reject the config on the next load.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("rp.json"),
            r#"{ "session": { "data_directory": "/d" }, "equipment": {} }"#,
        )
        .unwrap();
        ensure_material(dir.path(), &[], &[], false).unwrap();
        ensure_credential(dir.path()).unwrap();

        let ops = plan_client_wiring(dir.path());
        assert_eq!(ops.len(), 1, "{ops:?}");
        assert!(matches!(
            &ops[0],
            (check, FixOp::SetString { service, pointer, .. })
                if check == "tls.absent" && service == "rp" && pointer == "/ca_cert"
        ));

        // Present (even explicit null-turned-string) `ca_cert` gets no op.
        std::fs::write(
            dir.path().join("rp.json"),
            r#"{ "session": { "data_directory": "/d" }, "equipment": {}, "ca_cert": "/x/ca.pem" }"#,
        )
        .unwrap();
        assert!(plan_client_wiring(dir.path()).is_empty());
    }

    #[test]
    fn test_acme_active_keys_on_the_acme_json_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!acme_active(dir.path()));
        std::fs::write(dir.path().join("acme.json"), "{}").unwrap();
        assert!(acme_active(dir.path()));
    }

    #[test]
    fn test_acme_tls_block_value_requires_both_halves() {
        let dir = tempfile::tempdir().unwrap();
        let pki = pki_dir(dir.path());
        std::fs::create_dir_all(&pki).unwrap();
        assert_eq!(acme_tls_block_value(dir.path()), None);
        std::fs::write(pki.join("acme-cert.pem"), "cert").unwrap();
        assert_eq!(
            acme_tls_block_value(dir.path()),
            None,
            "a cert without its key must not be wired"
        );
        std::fs::write(pki.join("acme-key.pem"), "key").unwrap();
        let value = acme_tls_block_value(dir.path()).unwrap();
        let cert = value["cert"].as_str().unwrap();
        let key = value["key"].as_str().unwrap();
        assert!(cert.ends_with("acme-cert.pem"), "{cert}");
        assert!(key.ends_with("acme-key.pem"), "{key}");
        assert!(std::path::Path::new(cert).is_absolute());
    }

    #[test]
    fn test_plan_client_wiring_skips_ca_cert_on_an_acme_install() {
        // Even with a self-signed CA left on disk from before the flip,
        // acme.json wins: the client verifies publicly-trusted targets
        // through platform roots, and a written ca_cert would disable them
        // (issue #616). The credential half is trust-model-agnostic and
        // still wired.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("sentinel.json"),
            r#"{ "server": { "port": 11114 } }"#,
        )
        .unwrap();
        ensure_material(dir.path(), &[], &[], false).unwrap();
        ensure_credential(dir.path()).unwrap();
        std::fs::write(dir.path().join("acme.json"), "{}").unwrap();

        let ops = plan_client_wiring(dir.path());
        assert_eq!(ops.len(), 1, "{ops:?}");
        assert!(matches!(
            &ops[0].1,
            FixOp::SetObject { service, pointer, .. }
                if service == "sentinel" && pointer == "/service_auth"
        ));
    }

    #[test]
    fn test_tls_block_value_points_at_the_flat_pki_pair() {
        let dir = tempfile::tempdir().unwrap();
        let value = tls_block_value(dir.path(), "qhy-focuser");
        let cert = value["cert"].as_str().unwrap();
        let key = value["key"].as_str().unwrap();
        assert!(cert.ends_with("qhy-focuser.pem"), "{cert}");
        assert!(key.ends_with("qhy-focuser-key.pem"), "{key}");
        assert!(std::path::Path::new(cert).is_absolute());
        assert!(!cert.contains("certs"), "flat pki: {cert}");
    }

    #[tokio::test]
    async fn test_run_acme_persists_a_relative_acme_root_as_absolute() {
        // A renewal timer runs with an arbitrary working directory, so the
        // persisted acme.json must carry an absolute trust-anchor path even
        // when the operator passed a relative one. acme.json is persisted
        // before the DNS provider is built, so a bogus provider name lets
        // this assert on the file without any network.
        let dir = tempfile::tempdir().unwrap();
        let err = run_acme(
            dir.path(),
            AcmeArgs {
                domain: "observatory.test".to_string(),
                dns_provider: "no-such-provider".to_string(),
                dns_token: "tok".to_string(),
                email: "t@observatory.test".to_string(),
                staging: false,
                directory_url: None,
                acme_root: Some(std::path::PathBuf::from("relative/pebble-ca.pem")),
                dns_propagation_seconds: None,
            },
        )
        .await
        .unwrap_err();
        assert!(err.contains("unsupported DNS provider"), "{err}");

        let saved = std::fs::read_to_string(dir.path().join("acme.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&saved).unwrap();
        let root = value["acme_root"].as_str().unwrap();
        assert!(
            std::path::Path::new(root).is_absolute(),
            "persisted acme_root must be absolute: {root}"
        );
        assert!(root.ends_with("pebble-ca.pem"), "{root}");
    }
}
