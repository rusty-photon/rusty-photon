//! `guide-replay <recording>`: replays a recording, writes `replay.csv`
//! and `report.json`, prints the report, and exits 0 when it is within
//! stage G3's tolerance, 1 when it is not, and 2 when it cannot run. See
//! `docs/crates/guide-replay.md` (The command).

use std::error::Error;
use std::fs::{self, File};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use guide_replay::{replay, write_csv, Recording, Tolerance};
use rp_guiding::StarParams;

/// Replays a recording of guide-camera frames through the guider's
/// measurement.
#[derive(Debug, Parser)]
#[command(version)]
struct Args {
    /// The recording's directory, as `record.ps1` writes it.
    recording: PathBuf,
    /// Where `replay.csv` and `report.json` go; the recording's directory
    /// when not given.
    #[arg(long)]
    out: Option<PathBuf>,
}

fn main() -> ExitCode {
    let args = Args::parse();
    match run(&args) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(error) => {
            eprintln!("guide-replay: {error}");
            ExitCode::from(2)
        }
    }
}

fn run(args: &Args) -> Result<bool, Box<dyn Error>> {
    let recording = Recording::open(&args.recording)?;
    let replay = replay(&recording, StarParams::default(), Tolerance::default())?;
    let out = args.out.as_ref().unwrap_or(&args.recording);
    fs::create_dir_all(out)?;
    write_csv(&replay.rows, File::create(out.join("replay.csv"))?)?;
    fs::write(
        out.join("report.json"),
        serde_json::to_string_pretty(&replay.report)?,
    )?;
    print!("{}", replay.report);
    Ok(replay.report.passes())
}
