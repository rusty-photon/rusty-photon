//! Steps that stage the scenario's platform facts.

use std::path::PathBuf;

use cucumber::gherkin::Step;
use cucumber::given;
use doctor::facts::{Platform, UnitFacts};

use crate::world::DoctorWorld;

#[given(expr = "platform facts with an enabled unit {string}")]
fn one_unit(world: &mut DoctorWorld, unit: String) {
    world.add_unit(&unit);
}

#[given("platform facts with enabled units:")]
fn units_table(world: &mut DoctorWorld, step: &Step) {
    let table = step.table().expect("step needs a table");
    for row in table.rows.iter().skip(1) {
        world.add_unit(&row[0]);
    }
}

#[given("platform facts with no rusty-photon units")]
fn no_units(world: &mut DoctorWorld) {
    world.facts.units.clear();
}

#[given(expr = "platform facts with a disabled unit {string}")]
fn disabled_unit(world: &mut DoctorWorld, unit: String) {
    world.facts.units.push(UnitFacts {
        name: unit,
        enabled: false,
        condition_path: None,
        source_name: None,
        supplementary_groups: Vec::new(),
        active: None,
        failed: None,
        binary_path: None,
    });
}

#[given(expr = "platform facts where unit {string} is in a failed state")]
fn failed_unit(world: &mut DoctorWorld, unit: String) {
    world.add_unit(&unit);
    let staged = world
        .facts
        .units
        .iter_mut()
        .find(|u| u.name == unit)
        .expect("the unit was just added");
    staged.failed = Some(true);
}

#[given("platform facts where the service manager holds no unit failed")]
fn no_failed_units(world: &mut DoctorWorld) {
    for unit in &mut world.facts.units {
        unit.failed = Some(false);
    }
}

#[given(expr = "Windows platform facts with an enabled unit {string}")]
fn windows_unit(world: &mut DoctorWorld, unit: String) {
    world.facts.platform = Platform::Windows;
    world.add_unit(&unit);
}

#[given(expr = "platform facts where enabled unit {string} is gated on a missing file")]
fn unit_gated_missing(world: &mut DoctorWorld, unit: String) {
    let gate = world.temp.path().join("absent-config.json");
    push_gated_unit(world, unit, gate);
}

#[given(expr = "platform facts where enabled unit {string} is gated on config file {string}")]
fn unit_gated_on_config(world: &mut DoctorWorld, unit: String, config: String) {
    let gate = world.config_dir().join(config);
    push_gated_unit(world, unit, gate);
}

fn push_gated_unit(world: &mut DoctorWorld, unit: String, gate: PathBuf) {
    world.facts.units.push(UnitFacts {
        name: unit,
        enabled: true,
        condition_path: Some(gate),
        source_name: None,
        supplementary_groups: Vec::new(),
        active: None,
        failed: None,
        binary_path: None,
    });
}

#[given("the platform facts say no polkit rule grants sentinel restarts")]
const fn polkit_absent(world: &mut DoctorWorld) {
    world.facts.polkit_grants_sentinel_restart = Some(false);
}

#[given("the platform facts say a polkit rule grants sentinel restarts")]
const fn polkit_present(world: &mut DoctorWorld) {
    world.facts.polkit_grants_sentinel_restart = Some(true);
}

/// `dns.unresolvable`'s staged seam: list a public name as resolvable.
/// The first use creates the facts' `dns` object, which is what opts the
/// scenario into the DNS story at all — every derived name not listed
/// stays unresolvable.
#[given(expr = "the host resolves the public name {string}")]
fn host_resolves_public_name(world: &mut DoctorWorld, name: String) {
    world
        .facts
        .dns
        .get_or_insert_with(Default::default)
        .resolvable
        .push(name);
}

/// The all-broken DNS story: a `dns` object with nothing resolvable.
#[given("the host resolves none of the public names")]
fn host_resolves_no_public_names(world: &mut DoctorWorld) {
    world.facts.dns = Some(doctor::facts::DnsFacts::default());
}
