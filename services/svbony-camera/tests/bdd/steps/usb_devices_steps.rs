//! Device-claims steps (`usb_devices.feature`): the list, the staged USB
//! inventory, placeholders, the list's validation, and `doctor --devices`.

use std::time::{Duration, Instant};

use cucumber::gherkin::Step;
use cucumber::{given, then, when};

use crate::world::{registered_cameras, CameraWorld};

// --- configuration and the staged inventory -----------------------------------

#[given("the configuration lists these USB devices:")]
async fn configuration_lists_usb_devices(world: &mut CameraWorld, step: &Step) {
    let table = step.table().expect("the list step needs a data table");
    let header = &table.rows[0];
    let entries: Vec<serde_json::Value> = table
        .rows
        .iter()
        .skip(1)
        .map(|row| {
            let mut entry = serde_json::Map::new();
            for (column, cell) in header.iter().zip(row) {
                let value = if column == "device_number" {
                    serde_json::json!(cell.parse::<u32>().expect("device_number is a number"))
                } else {
                    serde_json::json!(cell)
                };
                entry.insert(column.clone(), value);
            }
            serde_json::Value::Object(entry)
        })
        .collect();
    world.config_json = Some(serde_json::json!({ "usb_devices": entries }));
}

#[given(regex = r"^the configuration JSON (.+)$")]
async fn configuration_json(world: &mut CameraWorld, json: String) {
    world.config_json = Some(
        serde_json::from_str(&json)
            .unwrap_or_else(|e| panic!("the step's configuration is not JSON: {e}: {json}")),
    );
}

#[given("the staged USB inventory:")]
#[when("the staged USB inventory becomes:")]
async fn staged_usb_inventory(world: &mut CameraWorld, step: &Step) {
    let document = step
        .docstring()
        .expect("the staged inventory step needs a docstring");
    let path = world.scratch_dir().join("usb-inventory.json");
    // Written whole and renamed into place, so a re-scan reading the file
    // mid-write (U6) never sees half a document.
    let staging = world.scratch_dir().join("usb-inventory.json.tmp");
    std::fs::write(&staging, document).expect("write the staged inventory");
    std::fs::rename(&staging, &path).expect("move the staged inventory into place");
    world.usb_inventory = Some(path);
}

// --- starting the service -------------------------------------------------------

#[when("the svbony-camera service starts")]
async fn service_starts(world: &mut CameraWorld) {
    world.start().await;
}

#[when("the svbony-camera service starts with an empty simulation backend")]
async fn service_starts_empty(world: &mut CameraWorld) {
    world.empty_backend = true;
    world.start().await;
}

#[when("the svbony-camera service is started")]
async fn service_is_started(world: &mut CameraWorld) {
    world.try_start().await;
}

/// The start must have exited without binding, and for the reason the
/// scenario names. The harness forwards a child's stderr rather than keeping
/// it, so once the start is known to exit, the same command line runs again
/// to completion to read what it said — safe only after that: a start the
/// service accepted would never complete.
#[then(regex = r"^the service refuses to start saying (.+)$")]
async fn service_refuses_to_start(world: &mut CameraWorld, expected: String) {
    match world
        .start_refusal
        .as_ref()
        .expect("no start was attempted")
    {
        Err(_) => {}
        Ok(()) => panic!("the service started from a configuration it should refuse"),
    }
    let args = world.start_args();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let output = bdd_infra::run_once_async(env!("CARGO_PKG_NAME"), &args, None).await;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "the refused start exited cleanly");
    assert!(
        stderr.contains(&expected),
        "the refused start does not say {expected:?}:\n{stderr}"
    );
}

// --- what the server registers --------------------------------------------------

#[then(regex = r"^the server registers (\d+) Camera devices?$")]
async fn server_registers(world: &mut CameraWorld, count: usize) {
    assert_eq!(
        world.cameras.len(),
        count,
        "the server registers {} Camera devices",
        world.cameras.len()
    );
}

#[then(regex = r#"^camera device (\d+) reports the UniqueID "([^"]+)"$"#)]
async fn camera_reports_unique_id(world: &mut CameraWorld, device: u32, expected: String) {
    // `unique_id` is a sync `Device` member: the client read it from
    // `configureddevices`, not from an HTTP round-trip of its own.
    assert_eq!(world.device(device).unique_id(), expected);
}

#[then(regex = r#"^camera device (\d+) reports the Name "([^"]+)"$"#)]
async fn camera_reports_name(world: &mut CameraWorld, device: u32, expected: String) {
    assert_eq!(world.device(device).name().await.unwrap(), expected);
}

#[then(regex = r#"^camera device (\d+) reports a Description containing "([^"]+)"$"#)]
async fn camera_reports_description(world: &mut CameraWorld, device: u32, expected: String) {
    let description = world.device(device).description().await.unwrap();
    assert!(
        description.contains(&expected),
        "Description {description:?} does not contain {expected:?}"
    );
}

#[then(regex = r#"^within (\d+) seconds camera device (\d+) reports the UniqueID "([^"]+)"$"#)]
async fn camera_reports_unique_id_within(
    world: &mut CameraWorld,
    seconds: u64,
    device: u32,
    expected: String,
) {
    // The service rebuilds its server on the reload, so each poll asks the
    // management API afresh, and a poll the rebuild refuses is "not yet"
    // (testing.md §5.9).
    let budget = Duration::from_secs(seconds);
    let start = Instant::now();
    let index = usize::try_from(device).expect("device number fits usize");
    loop {
        let seen = registered_cameras(world.port())
            .await
            .and_then(|cameras| cameras.get(index).map(|c| c.unique_id().to_string()));
        if seen.as_deref() == Some(expected.as_str()) {
            return;
        }
        let waited = start.elapsed();
        assert!(
            waited < budget,
            "camera device {device} did not report {expected:?} within {budget:?} \
             (last saw {seen:?})"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

// --- placeholders ---------------------------------------------------------------
//
// "I try to connect camera device N" is `departure_steps`' — it stashes the
// rejection's code and message alike.

#[then(regex = r"^the connect is rejected with ASCOM error 0x([0-9A-Fa-f]+)$")]
async fn connect_rejected_with(world: &mut CameraWorld, code: String) {
    let expected = u16::from_str_radix(&code, 16).expect("a hex ASCOM error code");
    assert_eq!(
        world.last_error_code,
        Some(expected),
        "expected 0x{expected:X}, got {:?} ({:?})",
        world.last_error_code.map(|c| format!("0x{c:X}")),
        world.last_error_message
    );
}

#[then(regex = r#"^the rejection says "([^"]+)"$"#)]
async fn rejection_says(world: &mut CameraWorld, expected: String) {
    let message = world
        .last_error_message
        .as_deref()
        .expect("the connect was not rejected");
    assert!(
        message.contains(&expected),
        "the rejection {message:?} does not say {expected:?}"
    );
}

// --- config actions --------------------------------------------------------------

#[then(regex = r#"^the config lists the USB port "([^"]+)" as device (\d+)$"#)]
async fn config_lists_port(world: &mut CameraWorld, port: String, device: u64) {
    let response = world.last_response.as_ref().expect("no response stashed");
    let list = response["config"]["usb_devices"]
        .as_array()
        .unwrap_or_else(|| panic!("config.get has no usb_devices list: {response}"));
    assert!(
        list.iter()
            .any(|e| e["usb_port"] == port.as_str() && e["device_number"] == device),
        "usb_devices {list:?} does not list {port} as device {device}"
    );
}

#[then("the config has no usb_devices list")]
async fn config_has_no_list(world: &mut CameraWorld) {
    let response = world.last_response.as_ref().expect("no response stashed");
    assert!(
        response["config"].get("usb_devices").is_none(),
        "config.get reports a usb_devices list: {response}"
    );
}

#[when(regex = r"^config\.apply sets usb_devices to (.+)$")]
async fn apply_usb_devices(world: &mut CameraWorld, json: String) {
    let list: serde_json::Value = serde_json::from_str(&json)
        .unwrap_or_else(|e| panic!("the step's list is not JSON: {e}: {json}"));
    let mut config = world.config_get().await;
    config
        .as_object_mut()
        .expect("config.get's config is an object")
        .insert("usb_devices".to_string(), list);
    world.call_action("config.apply", &config.to_string()).await;
}

#[then(regex = r#"^the apply errors name (\S+) saying "([^"]+)"$"#)]
async fn apply_errors_name(world: &mut CameraWorld, path: String, expected: String) {
    let response = world.last_response.as_ref().expect("no response stashed");
    let errors = response["errors"]
        .as_array()
        .unwrap_or_else(|| panic!("the apply response has no errors: {response}"));
    assert!(
        errors.iter().any(|e| e["path"] == path.as_str()
            && e["msg"].as_str().is_some_and(|m| m.contains(&expected))),
        "no apply error at {path} saying {expected:?}: {errors:?}"
    );
}

// --- doctor -------------------------------------------------------------------------

/// Run the service binary's `doctor` with `args`, against the scenario's
/// configuration and, when one is staged, its USB inventory.
async fn run_doctor(world: &mut CameraWorld, args: &[&str]) {
    let config_path = world.write_config();
    let mut argv: Vec<String> = std::iter::once("doctor".to_string())
        .chain(args.iter().map(|a| (*a).to_string()))
        .chain(["--config".to_string(), config_path])
        .collect();
    if let Some(inventory) = &world.usb_inventory {
        argv.push("--usb-inventory".to_string());
        argv.push(inventory.to_str().expect("utf8 inventory path").to_string());
    }
    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
    world.doctor_output =
        Some(bdd_infra::run_once_async(env!("CARGO_PKG_NAME"), &argv, None).await);
}

fn doctor_stdout(world: &CameraWorld) -> String {
    let output = world.doctor_output.as_ref().expect("doctor was not run");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[when("the doctor subcommand runs on the configuration")]
async fn doctor_runs_on_configuration(world: &mut CameraWorld) {
    run_doctor(world, &["--json"]).await;
}

#[then(regex = r"^the doctor's config\.full-shape check fails saying (.+)$")]
async fn full_shape_fails_saying(world: &mut CameraWorld, expected: String) {
    let stdout = doctor_stdout(world);
    let report: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("doctor stdout is not a JSON report: {e}\n{stdout}"));
    let check = report["checks"]
        .as_array()
        .expect("the report has a checks array")
        .iter()
        .find(|c| c["name"] == "config.full-shape")
        .unwrap_or_else(|| panic!("no config.full-shape check in {report}"));
    assert_eq!(check["status"], "fail", "{check}");
    let detail = check["detail"].as_str().expect("the check has a detail");
    assert!(
        detail.contains(&expected),
        "config.full-shape's detail {detail:?} does not say {expected:?}"
    );
}

#[when("doctor --devices runs")]
async fn doctor_devices_runs(world: &mut CameraWorld) {
    run_doctor(world, &["--devices"]).await;
}

#[then(regex = r"^the doctor exits with code (\d+)$")]
async fn doctor_exits_with(world: &mut CameraWorld, code: i32) {
    let output = world.doctor_output.as_ref().expect("doctor was not run");
    assert_eq!(
        output.status.code(),
        Some(code),
        "doctor exited {:?}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[then(regex = r#"^the doctor output says "([^"]+)"$"#)]
async fn doctor_output_says(world: &mut CameraWorld, expected: String) {
    let stdout = doctor_stdout(world);
    assert!(
        stdout.contains(&expected),
        "the doctor output does not say {expected:?}:\n{stdout}"
    );
}

/// The listing's table: the header row and every row under it, each split
/// into its cells. Columns are separated by two or more spaces, which no cell
/// contains.
fn listing_rows(stdout: &str) -> Vec<Vec<String>> {
    let mut lines = stdout
        .lines()
        .skip_while(|l| !l.trim_start().starts_with("Port "));
    let Some(header) = lines.next() else {
        return Vec::new();
    };
    std::iter::once(header)
        .chain(lines.take_while(|l| !l.trim().is_empty()))
        .map(|line| {
            line.trim()
                .split("  ")
                .map(str::trim)
                .filter(|cell| !cell.is_empty())
                .map(str::to_string)
                .collect()
        })
        .collect()
}

#[then("the devices listing shows these cameras:")]
async fn listing_shows(world: &mut CameraWorld, step: &Step) {
    let stdout = doctor_stdout(world);
    let expected = &step
        .table()
        .expect("the listing step needs a data table")
        .rows;
    let rows = listing_rows(&stdout);
    assert_eq!(&rows, expected, "the listing differs:\n{stdout}");
}

#[then("the devices listing shows no camera")]
async fn listing_shows_no_camera(world: &mut CameraWorld) {
    let stdout = doctor_stdout(world);
    let rows = listing_rows(&stdout);
    assert!(rows.len() <= 1, "the listing shows cameras:\n{stdout}");
}

#[then("the paste-ready usb_devices block is:")]
async fn paste_block_is(world: &mut CameraWorld, step: &Step) {
    let stdout = doctor_stdout(world);
    let expected: serde_json::Value = serde_json::from_str(
        step.docstring()
            .expect("the paste-block step needs a docstring"),
    )
    .expect("the step's block is JSON");
    // The block is printed as the `"usb_devices": [...]` member it is pasted
    // as, so it is read back the same way: as a member of an object.
    let block: String = stdout
        .lines()
        .skip_while(|l| !l.trim_start().starts_with("\"usb_devices\": ["))
        .scan(false, |closed, line| {
            if *closed {
                return None;
            }
            *closed = line.trim() == "]" || line.trim_end().ends_with("[]");
            Some(line)
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!block.is_empty(), "no usb_devices block in:\n{stdout}");
    let parsed: serde_json::Value = serde_json::from_str(&format!("{{{block}}}"))
        .unwrap_or_else(|e| panic!("the printed block is not valid JSON: {e}\n{block}"));
    assert_eq!(
        parsed["usb_devices"], expected,
        "the block differs:\n{stdout}"
    );
}
