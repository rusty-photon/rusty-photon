//! BDD step definitions for `document_resolution.feature`: how rp finds
//! a frame's FITS file and sidecar on disk, and that whatever follows a
//! lookup works on the files it found.
//!
//! Reused from other step modules: the capture step (`tool_steps.rs`);
//! the pinned data directory, `rp is restarted`, the remembered
//! document ids and the document fetch (`document_http_api_steps.rs`);
//! the image metadata fetch (`image_http_api_steps.rs`); the
//! `compute_image_stats` call (`image_stats_steps.rs`); and the
//! plate-solve calls and stub assertions (`plate_solve_steps.rs`).

use std::path::{Path, PathBuf};

use cucumber::{then, when};
use serde_json::Value;

use crate::steps::plate_solve_steps::{
    call_plate_solve_with_args, last_stub_request, PlateSolveArgs,
};
use crate::world::RpWorld;

/// A FITS header card is 80 bytes, and the header is a run of them.
const CARD_LEN: usize = 80;

// --- When steps: changing the files on disk ---

#[when(expr = "the sidecar of frame {string} is replaced by the sidecar of frame {string}")]
fn replace_sidecar(world: &mut RpWorld, target: String, source: String) {
    let from = sidecar_of(&remembered_fits(world, &source));
    let to = sidecar_of(&remembered_fits(world, &target));
    copy_file(&from, &to);
}

#[when(expr = "the FITS file of frame {string} is replaced by the FITS file of frame {string}")]
fn replace_fits(world: &mut RpWorld, target: String, source: String) {
    let from = remembered_fits(world, &source);
    let to = remembered_fits(world, &target);
    copy_file(&from, &to);
}

#[when(regex = r#"^the FITS file of frame "([^"]+)" is altered so that (.+)$"#)]
fn alter_fits(world: &mut RpWorld, frame: String, alteration: String) {
    let path = remembered_fits(world, &frame);
    match alteration.as_str() {
        // A keyword of its own, so the header stays valid FITS.
        "it has no DOC_ID card" => patch_doc_id_card(&path, |card| {
            card[..8].copy_from_slice(b"XDOC_ID ");
        }),
        "its DOC_ID card is a number" => patch_doc_id_card(&path, |card| {
            let replacement = format!("{:<8}= {:>20}{:<50}", "DOC_ID", 42, "");
            card.copy_from_slice(replacement.as_bytes());
        }),
        "it is no longer a FITS file" => std::fs::write(&path, b"not a FITS file")
            .unwrap_or_else(|e| panic!("overwrite {}: {e}", path.display())),
        other => panic!("unknown FITS alteration {other:?}"),
    }
}

#[when("the sidecar of the captured frame records max_adu as null")]
fn sidecar_max_adu_null(world: &mut RpWorld) {
    let sidecar = sidecar_of(&captured_fits(world));
    let mut doc = read_json(&sidecar);
    doc.as_object_mut()
        .unwrap_or_else(|| panic!("sidecar {} is not a JSON object", sidecar.display()))
        .insert("max_adu".to_string(), Value::Null);
    std::fs::write(
        &sidecar,
        serde_json::to_vec_pretty(&doc).expect("serialize sidecar"),
    )
    .unwrap_or_else(|e| panic!("write {}: {e}", sidecar.display()));
}

#[when("the captured frame's files are copied into a new data directory")]
fn copy_into_new_data_directory(world: &mut RpWorld) {
    relocate_into_new_data_directory(world, false);
}

#[when("the captured frame's files are moved into a new data directory")]
fn move_into_new_data_directory(world: &mut RpWorld) {
    relocate_into_new_data_directory(world, true);
}

#[when("the captured frame's files are copied into an archive outside the data directory")]
fn copy_into_archive(world: &mut RpWorld) {
    let archive = tempfile::tempdir().expect("failed to create archive tempdir");
    let relocated = copy_pair(&captured_fits(world), archive.path());
    world.relocated_image_path = Some(relocated.to_string_lossy().into_owned());
    world.archive_dir_holder = Some(archive);
}

// --- When steps: plate_solve by path ---

#[when("the MCP client calls \"plate_solve\" with the relocated image path")]
async fn plate_solve_relocated_path(world: &mut RpWorld) {
    let image_path = relocated_fits(world).to_string_lossy().into_owned();
    call_plate_solve_with_args(
        world,
        PlateSolveArgs {
            image_path: Some(image_path),
            ..Default::default()
        },
    )
    .await;
}

#[when(expr = "the MCP client calls \"plate_solve\" with the image path of frame {string}")]
async fn plate_solve_frame_path(world: &mut RpWorld, frame: String) {
    let image_path = remembered_fits(world, &frame)
        .to_string_lossy()
        .into_owned();
    call_plate_solve_with_args(
        world,
        PlateSolveArgs {
            image_path: Some(image_path),
            ..Default::default()
        },
    )
    .await;
}

// --- Then steps ---

#[then("the image metadata fits_path should be the relocated FITS file")]
fn metadata_fits_path_is_relocated(world: &mut RpWorld) {
    let metadata = world
        .last_image_metadata
        .as_ref()
        .expect("no image metadata fetched");
    let fits_path = metadata
        .get("fits_path")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("image metadata has no string fits_path: {metadata}"));
    assert_eq!(PathBuf::from(fits_path), relocated_fits(world));
}

#[then(
    "the stub plate solver should have received a request whose fits_path is the relocated FITS file"
)]
async fn stub_received_relocated_fits_path(world: &mut RpWorld) {
    let request = last_stub_request(world).await;
    let fits_path = request
        .get("fits_path")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("expected fits_path in request, got: {request}"));
    assert_eq!(PathBuf::from(fits_path), relocated_fits(world));
}

#[then(expr = "the relocated sidecar should contain a section named {string}")]
fn relocated_sidecar_has_section(world: &mut RpWorld, section: String) {
    let sidecar = sidecar_of(&relocated_fits(world));
    assert!(
        sections_of(&sidecar).get(&section).is_some(),
        "expected section {section:?} in {}",
        sidecar.display()
    );
}

#[then(expr = "the original sidecar should not contain a section named {string}")]
fn original_sidecar_lacks_section(world: &mut RpWorld, section: String) {
    assert_sidecar_lacks_section(&sidecar_of(&captured_fits(world)), &section);
}

#[then(expr = "the sidecar of frame {string} should not contain a section named {string}")]
fn frame_sidecar_lacks_section(world: &mut RpWorld, frame: String, section: String) {
    assert_sidecar_lacks_section(&sidecar_of(&remembered_fits(world, &frame)), &section);
}

// --- Helpers ---

fn captured_fits(world: &RpWorld) -> PathBuf {
    PathBuf::from(
        world
            .last_image_path
            .as_ref()
            .expect("no captured image path available"),
    )
}

fn remembered_fits(world: &RpWorld, name: &str) -> PathBuf {
    PathBuf::from(
        world
            .remembered_image_paths
            .get(name)
            .unwrap_or_else(|| panic!("no remembered frame named {name:?}")),
    )
}

fn relocated_fits(world: &RpWorld) -> PathBuf {
    PathBuf::from(
        world
            .relocated_image_path
            .as_ref()
            .expect("no frame was copied or moved"),
    )
}

fn sidecar_of(fits: &Path) -> PathBuf {
    assert_eq!(
        fits.extension().and_then(|e| e.to_str()),
        Some("fits"),
        "not a FITS path: {}",
        fits.display()
    );
    fits.with_extension("json")
}

fn copy_file(from: &Path, to: &Path) {
    std::fs::copy(from, to)
        .unwrap_or_else(|e| panic!("copy {} to {}: {e}", from.display(), to.display()));
}

/// Copy a FITS file and its sidecar into `dir`, keeping their names, and
/// return where the FITS file landed.
fn copy_pair(fits: &Path, dir: &Path) -> PathBuf {
    let name = fits.file_name().expect("FITS path has a file name");
    let new_fits = dir.join(name);
    copy_file(fits, &new_fits);
    copy_file(&sidecar_of(fits), &sidecar_of(&new_fits));
    new_fits
}

/// Put the captured frame's pair into a fresh data directory, which the
/// next rp start uses. The old directory stays alive for the scenario,
/// so a copied frame's original pair can still be read.
fn relocate_into_new_data_directory(world: &mut RpWorld, remove_original: bool) {
    let fits = captured_fits(world);
    let dir = tempfile::tempdir().expect("failed to create the new data directory");
    let relocated = copy_pair(&fits, dir.path());
    if remove_original {
        for path in [sidecar_of(&fits), fits] {
            std::fs::remove_file(&path)
                .unwrap_or_else(|e| panic!("remove {}: {e}", path.display()));
        }
    }
    world.relocated_image_path = Some(relocated.to_string_lossy().into_owned());
    world.pinned_data_directory = Some(dir.path().to_string_lossy().into_owned());
    world.previous_data_dir_holder = world.pinned_data_dir_holder.replace(dir);
}

/// Rewrite the primary header's `DOC_ID` card in place. Panics when the
/// header has none, since the scenario's premise is a file `capture`
/// stamped.
fn patch_doc_id_card(path: &Path, patch: impl FnOnce(&mut [u8])) {
    let mut bytes = std::fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let mut offset = 0;
    loop {
        let card = bytes
            .get(offset..offset + CARD_LEN)
            .unwrap_or_else(|| panic!("no DOC_ID card in the header of {}", path.display()));
        if card.starts_with(b"DOC_ID  ") {
            break;
        }
        assert!(
            !card.starts_with(b"END     "),
            "no DOC_ID card in the header of {}",
            path.display()
        );
        offset += CARD_LEN;
    }
    patch(&mut bytes[offset..offset + CARD_LEN]);
    std::fs::write(path, bytes).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
}

fn read_json(path: &Path) -> Value {
    let body = std::fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_slice(&body).unwrap_or_else(|e| panic!("{} is not JSON: {e}", path.display()))
}

fn sections_of(sidecar: &Path) -> Value {
    read_json(sidecar)
        .get("sections")
        .cloned()
        .unwrap_or(Value::Null)
}

fn assert_sidecar_lacks_section(sidecar: &Path, section: &str) {
    assert!(
        sections_of(sidecar).get(section).is_none(),
        "expected no section {section:?} in {}",
        sidecar.display()
    );
}
