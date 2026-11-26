//! Steps of `guide_log.feature`: the files of `guide-logs/` in the state
//! directory, read once the guider has stopped, and parsed with the parser
//! built from the logs PHD2 wrote on the rigs.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use cucumber::{given, then};

use crate::phd2_log::{self, header_value, Log, Row, Section};
use crate::world::GuiderWorld;

/// The guide logs' directory in the state directory the configuration
/// names.
fn guide_logs(world: &GuiderWorld) -> PathBuf {
    PathBuf::from(
        world.config["state_dir"]
            .as_str()
            .expect("the configuration names a state directory"),
    )
    .join("guide-logs")
}

/// The guide logs, by name.
fn files(world: &GuiderWorld) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(guide_logs(world)) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .map(|entry| entry.expect("read the guide logs' directory").path())
        .collect();
    files.sort();
    files
}

/// The one guide log, parsed.
fn the_log(world: &GuiderWorld) -> Log {
    let files = files(world);
    let [path] = files.as_slice() else {
        panic!(
            "the state directory holds {} guide logs: {files:?}",
            files.len()
        );
    };
    let text = std::fs::read_to_string(path).expect("read the guide log");
    phd2_log::parse(&text).unwrap_or_else(|error| panic!("{error}\n{text}"))
}

/// The guide log's guiding run, of which it holds one.
fn the_run(world: &GuiderWorld) -> phd2_log::Guiding {
    let runs: Vec<phd2_log::Guiding> = the_log(world)
        .blocks
        .into_iter()
        .flat_map(|block| block.sections)
        .filter_map(|section| match section {
            Section::Guiding(run) => Some(run),
            _ => None,
        })
        .collect();
    let [run] = runs.as_slice() else {
        panic!("the guide log holds {} guiding runs", runs.len());
    };
    run.clone()
}

#[given("the state directory holds a guide log last written 100 days ago")]
fn old_guide_log(world: &mut GuiderWorld) {
    let dir = guide_logs(world);
    std::fs::create_dir_all(&dir).expect("create the guide logs' directory");
    let path = dir.join("PHD2_GuideLog_2026-08-01_210000.txt");
    std::fs::write(&path, "an old night\n").expect("write the old guide log");
    let written = SystemTime::now() - Duration::from_hours(100 * 24);
    std::fs::File::options()
        .write(true)
        .open(&path)
        .and_then(|file| file.set_modified(written))
        .expect("date the old guide log");
}

#[given("the state directory holds a file where the guide logs would go")]
fn file_in_the_way(world: &mut GuiderWorld) {
    let dir = guide_logs(world);
    std::fs::create_dir_all(dir.parent().expect("the state directory"))
        .expect("create the state directory");
    std::fs::write(&dir, "not a directory").expect("write the file in the way");
}

#[then(regex = r"^the state directory holds (\d+) guide logs?$")]
fn guide_log_count(world: &mut GuiderWorld, count: usize) {
    if guide_logs(world).is_file() {
        assert_eq!(count, 0, "the guide logs' place holds a file");
        return;
    }
    let files = files(world);
    assert_eq!(files.len(), count, "{files:?}");
}

#[then("the state directory holds 1 guide log, named for the time it was opened")]
fn named_for_its_time(world: &mut GuiderWorld) {
    let files = files(world);
    let [path] = files.as_slice() else {
        panic!(
            "the state directory holds {} guide logs: {files:?}",
            files.len()
        );
    };
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .expect("a guide log's name");
    let stamp = name
        .strip_prefix("PHD2_GuideLog_")
        .and_then(|rest| rest.strip_suffix(".txt"))
        .unwrap_or_else(|| panic!("a guide log named {name}"));
    let log = the_log(world);
    // The name's time is the block's, as `YYYY-MM-DD_HHMMSS`.
    let enabled = log.blocks[0].enabled_at.replace(' ', "_").replace(':', "");
    assert_eq!(
        stamp, enabled,
        "{name} against {}",
        log.blocks[0].enabled_at
    );
}

#[then("the guide log parses as PHD2's format")]
fn parses(world: &mut GuiderWorld) {
    let log = the_log(world);
    let block = &log.blocks[0];
    assert!(
        block.writer.starts_with("rusty-photon guider "),
        "the version line names {}",
        block.writer
    );
}

#[then(regex = r"^the guide log holds a calibration that passed, with rows of the legs (.+)$")]
fn calibration_passed(world: &mut GuiderWorld, legs: String) {
    let wanted: Vec<String> = legs
        .replace(" and ", ", ")
        .split(", ")
        .map(str::to_owned)
        .collect();
    let log = the_log(world);
    let calibration = log
        .blocks
        .iter()
        .flat_map(|block| &block.sections)
        .find_map(|section| match section {
            Section::Calibration(calibration) => Some(calibration),
            _ => None,
        })
        .expect("the guide log holds a calibration");
    let mut seen: Vec<String> = Vec::new();
    for row in &calibration.rows {
        if seen.last() != Some(&row.direction) {
            seen.push(row.direction.clone());
        }
    }
    assert_eq!(seen, wanted, "the legs of the rows");
    let legs: Vec<&str> = calibration
        .legs
        .iter()
        .map(|leg| leg.direction.as_str())
        .collect();
    assert_eq!(legs, ["West", "North"], "the legs' results");
    assert!(
        calibration.complete.is_some(),
        "the calibration did not complete"
    );
}

#[then(regex = r"^the guide log holds a guiding run of at least (\d+) rows that ends$")]
fn guiding_run(world: &mut GuiderWorld, rows: usize) {
    let run = the_run(world);
    assert!(run.rows.len() >= rows, "{} rows", run.rows.len());
    assert!(run.ends_at.is_some(), "the run does not end");
}

#[then(regex = r#"^the guide log's guiding run has the note "(.+)"$"#)]
fn has_note(world: &mut GuiderWorld, note: String) {
    let run = the_run(world);
    assert!(
        run.notes.iter().any(|(_, seen)| *seen == note),
        "no note {note:?} among {:?}",
        run.notes
    );
}

#[then(regex = r#"^the guide log's guiding run has a note beginning "(.+)"$"#)]
fn has_note_beginning(world: &mut GuiderWorld, start: String) {
    let run = the_run(world);
    assert!(
        run.notes.iter().any(|(_, seen)| seen.starts_with(&start)),
        "no note beginning {start:?} among {:?}",
        run.notes
    );
}

#[then("the guide log's guiding run has a dropped frame")]
fn dropped(world: &mut GuiderWorld) {
    let run = the_run(world);
    assert!(
        run.rows
            .iter()
            .any(|row| matches!(row, Row::Drop(drop) if drop.code == 5)),
        "no DROP row of code 5"
    );
}

#[then("the guide log's guiding header gives the pier side East, the hour angle and the altitude")]
fn pointing(world: &mut GuiderWorld) {
    let run = the_run(world);
    assert_eq!(header_value(&run.header, "Pier side"), Some("East"));
    let hour_angle = header_value(&run.header, "Hour angle").expect("an hour angle");
    assert!(hour_angle.ends_with(" hr"), "{hour_angle}");
    let altitude = header_value(&run.header, "Alt").expect("an altitude");
    assert!(altitude.ends_with(" deg"), "{altitude}");
}
