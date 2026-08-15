//! The catalog's embed list must match the packaging tree. Walks the source
//! tree via `CARGO_MANIFEST_DIR`, so it runs under Cargo only (the Bazel
//! target is tagged `requires-cargo` and rides the nightly safety net).

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![allow(clippy::unwrap_used, clippy::expect_used)]
// Curated test-scope allow list — documented in the root Cargo.toml [workspace.lints] block.
#![allow(
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
    clippy::struct_excessive_bools
)]

/// Every `services/*/pkg` directory carries a `doctor.toml` and appears in
/// the catalog, and nothing else does (docs/services/doctor.md §The derived
/// catalog).
#[test]
fn test_catalog_matches_the_packaging_tree() {
    let services_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap();
    let mut packaged: Vec<String> = std::fs::read_dir(services_dir)
        .unwrap()
        .filter_map(|entry| {
            let entry = entry.unwrap();
            let pkg = entry.path().join("pkg");
            if !pkg.is_dir() {
                return None;
            }
            let name = entry.file_name().into_string().unwrap();
            assert!(
                pkg.join("doctor.toml").is_file(),
                "services/{name}/pkg exists but has no doctor.toml — add one \
                 (docs/services/doctor.md §The derived catalog)"
            );
            Some(name)
        })
        .collect();
    packaged.sort();
    let known: Vec<String> = doctor::catalog::catalog()
        .iter()
        .map(|e| e.name.to_string())
        .collect();
    assert_eq!(
        packaged, known,
        "the embed list in catalog.rs must match services/*/pkg"
    );
}

/// Every udev rule in the packaging tree is embedded (and nothing else):
/// the hardware checks compare installed rules against these embeds, so a
/// rule missing here is a rule doctor silently never checks. sentinel's
/// `50-*.rules` is a polkit rule, recognizable by content, and stays out.
#[test]
fn test_udev_rule_embeds_match_the_packaging_tree() {
    let services_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap();
    let mut in_tree: Vec<(String, String)> = Vec::new();
    for entry in std::fs::read_dir(services_dir).unwrap() {
        let entry = entry.unwrap();
        let pkg = entry.path().join("pkg");
        if !pkg.is_dir() {
            continue;
        }
        let service = entry.file_name().into_string().unwrap();
        for file in std::fs::read_dir(&pkg).unwrap() {
            let file = file.unwrap();
            let name = file.file_name().into_string().unwrap();
            if std::path::Path::new(&name).extension() != Some(std::ffi::OsStr::new("rules")) {
                continue;
            }
            let content = std::fs::read_to_string(file.path()).unwrap();
            if content.contains("polkit.addRule") {
                continue;
            }
            in_tree.push((service.clone(), name));
        }
    }
    in_tree.sort();
    let mut embedded: Vec<(String, String)> = doctor::catalog::UDEV_RULES
        .iter()
        .map(|r| (r.service.to_string(), r.file_name.to_string()))
        .collect();
    embedded.sort();
    assert_eq!(
        in_tree, embedded,
        "the UDEV_RULES embed list in catalog.rs must match services/*/pkg/*.rules"
    );
}

/// The firmware check mirrors the helper script's own idempotency gate —
/// the three artifact paths must stay in lockstep with the script.
#[test]
fn test_firmware_artifacts_match_the_helper_script() {
    let script_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("qhy-camera/pkg/rusty-photon-qhy-firmware-install");
    let script = std::fs::read_to_string(&script_path).unwrap();
    for (path, _) in doctor::hardware::QHY_FIRMWARE_ARTIFACTS {
        assert!(
            script.contains(path),
            "{} never mentions {path} — the firmware check's artifact list \
             drifted from the helper's own gate",
            script_path.display()
        );
    }
}
