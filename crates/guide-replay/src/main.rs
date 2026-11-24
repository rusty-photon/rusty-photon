//! `guide-replay`: `replay` replays a recording, writes `replay.csv` and
//! `report.json`, prints the report, and exits 0 when it is within stage
//! G3's tolerance, 1 when it is not, and 2 when it cannot run. `crop`
//! writes a sample of a recording's frames, cut to a region, as a
//! recording of FITS frames. See `docs/crates/guide-replay.md` (The
//! command).

use std::error::Error;
use std::fs::{self, File};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use guide_replay::{crop, replay, write_csv, Recording, Region, Tolerance};
use rp_guiding::StarParams;

/// Replays recordings of guide-camera frames through the guider's
/// measurement.
#[derive(Debug, Parser)]
#[command(version)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Replays a recording and holds it to stage G3's tolerance.
    Replay {
        /// The recording's directory.
        recording: PathBuf,
        /// Where `replay.csv` and `report.json` go; the recording's
        /// directory when not given.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Writes every Nth frame of a recording, cut to a region, as a
    /// recording of FITS frames.
    Crop {
        /// The recording's directory.
        recording: PathBuf,
        /// The directory the crops go to.
        #[arg(long)]
        out: PathBuf,
        /// The region's left column.
        #[arg(long)]
        x: usize,
        /// The region's top row.
        #[arg(long)]
        y: usize,
        /// The region's width.
        #[arg(long)]
        width: usize,
        /// The region's height.
        #[arg(long)]
        height: usize,
        /// Takes every Nth frame, the first among them.
        #[arg(long, default_value_t = 1)]
        every: u32,
    },
}

fn main() -> ExitCode {
    let args = Args::parse();
    match run(args.command) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(error) => {
            eprintln!("guide-replay: {error}");
            ExitCode::from(2)
        }
    }
}

fn run(command: Command) -> Result<bool, Box<dyn Error>> {
    match command {
        Command::Replay { recording, out } => {
            let opened = Recording::open(&recording)?;
            let replay = replay(&opened, StarParams::default(), Tolerance::default())?;
            let out = out.unwrap_or(recording);
            fs::create_dir_all(&out)?;
            write_csv(&replay.rows, File::create(out.join("replay.csv"))?)?;
            fs::write(
                out.join("report.json"),
                serde_json::to_string_pretty(&replay.report)?,
            )?;
            print!("{}", replay.report);
            Ok(replay.report.passes())
        }
        Command::Crop {
            recording,
            out,
            x,
            y,
            width,
            height,
            every,
        } => {
            let region = Region {
                x,
                y,
                width,
                height,
            };
            let written = crop(&recording, &out, region, every)?;
            println!("{written} frames cropped into {}", out.display());
            Ok(true)
        }
    }
}
