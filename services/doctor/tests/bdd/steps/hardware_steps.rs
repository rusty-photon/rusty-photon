//! Steps that stage the scenario's hardware facts (the `hardware` object
//! in the platform-facts file).

use cucumber::given;
use rusty_photon_doctor_checks::{PathFacts, PathKind, UsbDevice, UserFacts};

use crate::world::DoctorWorld;

fn parse_mode(mode: &str) -> u32 {
    u32::from_str_radix(mode, 8).unwrap_or_else(|e| panic!("mode {mode:?} is not octal: {e}"))
}

fn parse_vid_pid(id: &str) -> (String, String) {
    let (vendor, product) = id
        .split_once(':')
        .unwrap_or_else(|| panic!("USB id {id:?} is not vendor:product"));
    (vendor.to_string(), product.to_string())
}

/// Touch the hardware object without staging anything — the family runs
/// against an empty device surface.
#[given("hardware facts staged empty")]
fn staged_empty(world: &mut DoctorWorld) {
    world.hardware();
}

#[given(
    expr = "hardware facts with a character device {string} owned by uid {int} gid {int} with mode {string}"
)]
fn char_device(world: &mut DoctorWorld, path: String, uid: u32, gid: u32, mode: String) {
    world.hardware().paths.insert(
        path,
        PathFacts {
            kind: PathKind::CharDevice,
            mode: parse_mode(&mode),
            uid,
            gid,
        },
    );
}

#[given(expr = "hardware facts with a directory at {string}")]
fn directory_at(world: &mut DoctorWorld, path: String) {
    world.hardware().paths.insert(
        path,
        PathFacts {
            kind: PathKind::Dir,
            mode: 0o755,
            uid: 0,
            gid: 0,
        },
    );
}

#[given(expr = "hardware facts with an executable file at {string}")]
fn executable_at(world: &mut DoctorWorld, path: String) {
    world.hardware().paths.insert(
        path,
        PathFacts {
            kind: PathKind::File,
            mode: 0o755,
            uid: 0,
            gid: 0,
        },
    );
}

#[given(expr = "hardware facts with a regular file at {string}")]
fn file_at(world: &mut DoctorWorld, path: String) {
    world.hardware().paths.insert(
        path,
        PathFacts {
            kind: PathKind::File,
            mode: 0o644,
            uid: 0,
            gid: 0,
        },
    );
}

#[given(expr = "hardware facts where host group {string} has gid {int}")]
fn host_group(world: &mut DoctorWorld, name: String, gid: u32) {
    world.hardware().groups.insert(name, gid);
}

#[given(expr = "hardware facts where the rusty-photon user has uid {int} and gid {int}")]
fn service_user(world: &mut DoctorWorld, uid: u32, gid: u32) {
    world.hardware().service_user = Some(UserFacts { uid, gid });
}

#[given(expr = "hardware facts where the rusty-photon user belongs to host group {string}")]
fn service_user_group(world: &mut DoctorWorld, name: String) {
    world.hardware().service_user_groups.push(name);
}

#[given(expr = "hardware facts with a USB device {string} reporting product string {string}")]
fn usb_device_with_model(world: &mut DoctorWorld, id: String, model: String) {
    let (vendor, product) = parse_vid_pid(&id);
    world.hardware().usb.push(UsbDevice {
        vendor,
        product,
        model: Some(model),
        port: None,
        serial: None,
    });
}

#[given(expr = "hardware facts with a USB device {string} with no product string")]
fn usb_device_without_model(world: &mut DoctorWorld, id: String) {
    let (vendor, product) = parse_vid_pid(&id);
    world.hardware().usb.push(UsbDevice {
        vendor,
        product,
        model: None,
        port: None,
        serial: None,
    });
}

#[given(expr = "hardware facts where the USB inventory is unavailable because {string}")]
fn usb_inventory_unavailable(world: &mut DoctorWorld, reason: String) {
    world.hardware().usb_unavailable = Some(reason);
}

#[given(expr = "hardware facts with an empty but readable USB inventory")]
fn usb_inventory_empty(world: &mut DoctorWorld) {
    let hardware = world.hardware();
    hardware.usb.clear();
    hardware.usb_unavailable = None;
}

#[given(expr = "hardware facts with present COM ports {string}")]
fn com_ports(world: &mut DoctorWorld, ports: String) {
    world.hardware().com_ports = ports.split(", ").map(str::to_string).collect();
}

#[given(expr = "the unit {string} confers supplementary group {string}")]
fn unit_confers_group(world: &mut DoctorWorld, unit: String, group: String) {
    let unit_facts = world
        .facts
        .units
        .iter_mut()
        .find(|u| u.name == unit)
        .unwrap_or_else(|| panic!("stage unit {unit} before conferring groups"));
    unit_facts.supplementary_groups.push(group);
}

#[given(expr = "the installed udev rule for {string} is the packaged rule")]
fn installed_rule_packaged(world: &mut DoctorWorld, service: String) {
    let rule = doctor::catalog::udev_rule_for(&service)
        .unwrap_or_else(|| panic!("{service} ships no udev rule"));
    world
        .hardware()
        .udev_rules
        .insert(rule.file_name.to_string(), rule.content.to_string());
}

#[given(
    expr = "the installed udev rule for {string} is the packaged rule with a local edit appended"
)]
fn installed_rule_edited(world: &mut DoctorWorld, service: String) {
    let rule = doctor::catalog::udev_rule_for(&service)
        .unwrap_or_else(|| panic!("{service} ships no udev rule"));
    world.hardware().udev_rules.insert(
        rule.file_name.to_string(),
        format!("{}# local operator override\n", rule.content),
    );
}

#[given(
    expr = "hardware facts where the data directory is owned by uid {int} gid {int} with mode {string}"
)]
fn data_dir_ownership(world: &mut DoctorWorld, uid: u32, gid: u32, mode: String) {
    let dir = world
        .data_dir
        .as_ref()
        .expect("stage the data directory first")
        .to_string_lossy()
        .into_owned();
    world.hardware().paths.insert(
        dir,
        PathFacts {
            kind: PathKind::Dir,
            mode: parse_mode(&mode),
            uid,
            gid,
        },
    );
}
