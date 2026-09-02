//! The flip orchestrator's op plan and transaction
//! (docs/services/doctor.md §The flip orchestrator).
//!
//! `doctor tls flip-to-acme` derives its whole op plan before the first
//! config write: the convergence checks are iterated to a fixpoint —
//! the client-side host rewrites are only plannable once the target's
//! staged `server.tls` points at the wildcard pair — against **staged
//! in-memory copies** of the configs, so a planning failure costs zero
//! writes (D6 of docs/plans/acme-flip.md). Only then are the changed
//! files written, one `rusty_photon_config::save` each.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::Value;
use tracing::debug;

use crate::checks::Context;
use crate::facts::PlatformFacts;
use crate::report::AppliedFix;

/// One fix can unlock the next (the host rewrite follows the repointed
/// `server.tls`), so planning iterates like the `--fix` fixpoint; the
/// cap is the same runaway backstop, far above any real chain.
const MAX_PLAN_ROUNDS: usize = 4;

/// The staged flip: the cross-round op plan and the changed configs'
/// final values, ready to write (or, on `--dry-run`, to print).
pub struct FlipPlan {
    /// Every op in application order, tagged with its originating check.
    pub ops: Vec<AppliedFix>,
    /// The changed services' final config values.
    pub staged: Vec<(String, Value)>,
}

/// Derive the flip's full op plan without writing anything.
///
/// `converge_staging` is `--allow-staging`'s planning mode: the staged
/// contexts treat a staging `acme.json` as convergeable (the one
/// deliberate exception to D4's withholding).
///
/// # Errors
///
/// Returns a message when a planned op targets a service whose config
/// value could not be read — the transaction contract is that a
/// planning failure costs zero *config* writes, so the caller aborts
/// with the message. ACME material an issuance leg wrote earlier in
/// the same run (acme.json, the wildcard pair) is already on disk and
/// stays valid — renewal's territory, not the plan's.
pub fn plan(
    config_dir: &Path,
    facts: &PlatformFacts,
    converge_staging: bool,
) -> Result<FlipPlan, String> {
    let empty = BTreeMap::new();
    let base = Context::gather_staged(config_dir.to_path_buf(), facts.clone(), &empty);
    let mut originals: BTreeMap<String, Value> = BTreeMap::new();
    for scan in &base.scans {
        if let Some(value) = scan.value() {
            originals.insert(scan.entry.name.to_string(), value.clone());
        }
    }

    let mut staged = originals.clone();
    let mut ops = Vec::new();
    for round in 0..MAX_PLAN_ROUNDS {
        let mut ctx = Context::gather_staged(config_dir.to_path_buf(), facts.clone(), &staged);
        ctx.converge_staging = converge_staging;
        let planned: Vec<(String, crate::report::FixOp)> = crate::checks::flip_convergence(&ctx)
            .into_iter()
            .flat_map(|check| {
                let name = check.name.clone();
                check
                    .fixes
                    .into_iter()
                    .map(move |op| (name.clone(), op))
                    .collect::<Vec<_>>()
            })
            .collect();
        if planned.is_empty() {
            debug!(round, ops = ops.len(), "flip plan converged");
            break;
        }
        let mut round_changed = false;
        for (check, op) in planned {
            let Some(service) = op.service().map(str::to_string) else {
                debug!("skipping a non-config op in the flip plan: {op}");
                continue;
            };
            let Some(value) = staged.get_mut(&service) else {
                return Err(format!(
                    "the flip plans a change to {service}.json, which is missing or \
                     unreadable — no service config was written; ACME material \
                     issued earlier in this run (acme.json, the wildcard pair) is \
                     already on disk and stays valid"
                ));
            };
            if crate::fix::apply_op(value, &op, false) {
                round_changed = true;
                ops.push(AppliedFix { check, op });
            }
        }
        if !round_changed {
            // Every planned op kept a present value (operator intent) —
            // re-planning would loop on the same suggestions forever.
            debug!(
                round,
                "flip plan stopped: remaining divergence is operator intent"
            );
            break;
        }
    }

    let staged: Vec<(String, Value)> = staged
        .into_iter()
        .filter(|(name, value)| originals.get(name) != Some(value))
        .collect();
    Ok(FlipPlan { ops, staged })
}

/// Write the staged values, one file at a time, through
/// `rusty_photon_config::save` (owner and mode preserved, as every fix
/// write).
///
/// # Errors
///
/// POSIX offers no multi-file atomicity: an unwritable file mid-sequence
/// leaves the earlier files rewritten, so the message names exactly which
/// files were and were not written — re-running the command, or plain
/// `doctor --fix` (D1's backstop), converges the remainder.
pub fn write_staged(config_dir: &Path, staged: &[(String, Value)]) -> Result<(), String> {
    let mut written: Vec<String> = Vec::new();
    for (index, (service, value)) in staged.iter().enumerate() {
        let path = config_dir.join(format!("{service}.json"));
        if let Err(e) = rusty_photon_config::save(&path, value) {
            let unwritten: Vec<String> = staged
                .iter()
                .skip(index)
                .map(|(name, _)| format!("{name}.json"))
                .collect();
            let written = if written.is_empty() {
                "none".to_string()
            } else {
                written.join(", ")
            };
            return Err(format!(
                "could not write {}: {e}; written: {written}; not written: {} — \
                 re-run `doctor tls flip-to-acme` (or `doctor --fix`) to converge \
                 the remainder",
                path.display(),
                unwritten.join(", ")
            ));
        }
        debug!(path = %path.display(), "flip wrote a staged config");
        written.push(format!("{service}.json"));
    }
    Ok(())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn facts() -> PlatformFacts {
        serde_json::from_value(serde_json::json!({ "platform": "linux" })).unwrap()
    }

    /// A provisioned-then-issued tree: rp serving the doctor pair with a
    /// CA pin, ui-htmx dialing rp over loopback https with a pinned CA
    /// path, and the staged acme.json + wildcard pair.
    fn stage_flip_tree(dir: &Path, staging: bool) {
        let pki = crate::provision::absolute_pki_dir(dir);
        std::fs::create_dir_all(&pki).unwrap();
        for name in ["acme-cert.pem", "acme-key.pem", "ca.pem"] {
            std::fs::write(pki.join(name), b"pem").unwrap();
        }
        let rp_tls = crate::provision::tls_block_value(dir, "rp");
        let ca = pki.join("ca.pem").to_string_lossy().into_owned();
        std::fs::write(
            dir.join("rp.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "server": { "port": 11115, "tls": rp_tls },
                "ca_cert": ca,
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            dir.join("ui-htmx.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "server": { "port": 11120 },
                "rp": { "base_url": "https://127.0.0.1:11115", "ca_cert_path": ca },
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            dir.join("acme.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "email": "ops@pier1.example.com",
                "domain": "pier1.example.com",
                "dns_provider": "cloudflare",
                "dns_credentials": { "api_token": "$TOKEN" },
                "staging": staging,
                "renewal_days_before_expiry": 30,
                "post_renewal_hooks": [],
            }))
            .unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn test_plan_composes_the_repoint_and_the_host_rewrite_across_rounds() {
        let dir = tempfile::tempdir().unwrap();
        stage_flip_tree(dir.path(), false);

        let plan = plan(dir.path(), &facts(), false).unwrap();

        let staged: BTreeMap<&str, &Value> = plan
            .staged
            .iter()
            .map(|(name, value)| (name.as_str(), value))
            .collect();
        let rp = staged.get("rp").expect("rp.json staged");
        let wildcard = crate::provision::acme_tls_block_value(dir.path()).unwrap();
        assert_eq!(rp.pointer("/server/tls/cert"), wildcard.get("cert"));
        assert_eq!(rp.pointer("/server/tls/key"), wildcard.get("key"));
        assert_eq!(rp.pointer("/ca_cert"), None, "the CA pin is removed");
        assert_eq!(
            rp.pointer("/server/advertised_url").and_then(Value::as_str),
            Some("https://rp.pier1.example.com:11115")
        );
        let ui = staged.get("ui-htmx").expect("ui-htmx.json staged");
        assert_eq!(
            ui.pointer("/rp/base_url").and_then(Value::as_str),
            Some("https://rp.pier1.example.com:11115"),
            "the host rewrite lands even though it is only plannable after \
             the staged repoint — the cross-round composition"
        );
        assert_eq!(ui.pointer("/rp/ca_cert_path"), None);
        assert!(
            plan.ops
                .iter()
                .any(|f| f.check == "tls.stale-selfsigned-pointer"),
            "{:?}",
            plan.ops
        );
        assert!(
            plan.ops.iter().any(|f| f.check == "joins.client-transport"),
            "{:?}",
            plan.ops
        );

        // Nothing on disk moved: planning is the read-only half.
        let on_disk: Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("ui-htmx.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            on_disk.pointer("/rp/base_url").and_then(Value::as_str),
            Some("https://127.0.0.1:11115")
        );
    }

    #[test]
    fn test_plan_withholds_on_staging_and_converges_with_the_override() {
        let dir = tempfile::tempdir().unwrap();
        stage_flip_tree(dir.path(), true);

        let withheld = plan(dir.path(), &facts(), false).unwrap();
        assert!(withheld.ops.is_empty(), "{:?}", withheld.ops);
        assert!(withheld.staged.is_empty());

        let converged = plan(dir.path(), &facts(), true).unwrap();
        assert!(!converged.ops.is_empty());
        let staged: BTreeMap<&str, &Value> = converged
            .staged
            .iter()
            .map(|(name, value)| (name.as_str(), value))
            .collect();
        let wildcard = crate::provision::acme_tls_block_value(dir.path()).unwrap();
        assert_eq!(
            staged.get("rp").unwrap().pointer("/server/tls/cert"),
            wildcard.get("cert")
        );
    }

    #[test]
    fn test_plan_is_empty_on_a_converged_tree() {
        let dir = tempfile::tempdir().unwrap();
        stage_flip_tree(dir.path(), false);
        let first = plan(dir.path(), &facts(), false).unwrap();
        write_staged(dir.path(), &first.staged).unwrap();

        let second = plan(dir.path(), &facts(), false).unwrap();
        assert!(second.ops.is_empty(), "{:?}", second.ops);
        assert!(second.staged.is_empty());
    }

    #[test]
    fn test_write_staged_names_the_written_and_unwritten_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("rp.json"), "{}").unwrap();
        let staged = vec![
            ("rp".to_string(), serde_json::json!({ "a": 1 })),
            ("missing-dir".to_string(), serde_json::json!({ "b": 2 })),
        ];
        // The second write targets a config dir entry whose parent write
        // path is fine — force a failure via an unwritable directory
        // component instead: a *file* standing where the config must go.
        let blocked = dir.path().join("missing-dir.json");
        std::fs::create_dir(&blocked).unwrap();
        let err = write_staged(dir.path(), &staged).unwrap_err();
        assert!(err.contains("written: rp.json"), "{err}");
        assert!(err.contains("not written: missing-dir.json"), "{err}");
        let back: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join("rp.json")).unwrap())
                .unwrap();
        assert_eq!(back.pointer("/a"), Some(&Value::from(1)));
    }
}
