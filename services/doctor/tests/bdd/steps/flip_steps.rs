//! Steps driving `doctor tls flip-to-acme` — the one-command self-signed
//! → ACME transition (docs/services/doctor.md §The flip orchestrator).

use cucumber::{then, when};

use crate::world::DoctorWorld;

#[when("I run doctor tls flip-to-acme")]
async fn run_flip(world: &mut DoctorWorld) {
    world
        .run_doctor_subcommand(&["tls", "flip-to-acme"], None)
        .await;
}

#[when("I run doctor tls flip-to-acme with --json")]
async fn run_flip_json(world: &mut DoctorWorld) {
    world
        .run_doctor_subcommand(&["tls", "flip-to-acme", "--json"], None)
        .await;
}

#[when("I run doctor tls flip-to-acme with --dry-run")]
async fn run_flip_dry(world: &mut DoctorWorld) {
    world
        .run_doctor_subcommand(&["tls", "flip-to-acme", "--dry-run"], None)
        .await;
}

#[when("I run doctor tls flip-to-acme with --dry-run and --json")]
async fn run_flip_dry_json(world: &mut DoctorWorld) {
    world
        .run_doctor_subcommand(&["tls", "flip-to-acme", "--dry-run", "--json"], None)
        .await;
}

#[when("I run doctor tls flip-to-acme with --allow-staging")]
async fn run_flip_allow_staging(world: &mut DoctorWorld) {
    world
        .run_doctor_subcommand(&["tls", "flip-to-acme", "--allow-staging"], None)
        .await;
}

#[when(expr = "I run doctor tls flip-to-acme with --domain {string}")]
async fn run_flip_domain(world: &mut DoctorWorld, domain: String) {
    world
        .run_doctor_subcommand(&["tls", "flip-to-acme", "--domain", &domain], None)
        .await;
}

#[when("I run doctor tls flip-to-acme with --dry-run and all required issuance flags")]
async fn run_flip_dry_issuance(world: &mut DoctorWorld) {
    world
        .run_doctor_subcommand(
            &[
                "tls",
                "flip-to-acme",
                "--dry-run",
                "--domain",
                "pier1.example.com",
                "--dns-provider",
                "cloudflare",
                "--dns-token-var",
                "CLOUDFLARE_API_TOKEN",
                "--email",
                "ops@pier1.example.com",
            ],
            None,
        )
        .await;
}

#[then(expr = "the report records a planned op for check {string} on service {string}")]
fn records_planned_op(world: &mut DoctorWorld, check: String, service: String) {
    let plan = world
        .report()
        .get("plan")
        .and_then(|p| p.as_array())
        .cloned()
        .unwrap_or_default();
    assert!(
        plan.iter()
            .any(|f| f["check"] == check.as_str() && f["op"]["service"] == service.as_str()),
        "no planned op for {check} on {service} in: {plan:?}"
    );
}

#[then(expr = "the config root does not contain {string}")]
fn config_root_lacks(world: &mut DoctorWorld, name: String) {
    let path = world.config_dir().join(&name);
    assert!(!path.exists(), "expected {} not to exist", path.display());
}
