//! `rusty-photon-ci`: the workspace's developer tools, one subcommand each.
//! Run one with `cargo ci <tool>`.

use std::error::Error;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use rusty_photon_ci::brew_formulas::{self, Channel, Request};
use rusty_photon_ci::process::System;
use rusty_photon_ci::repin_bazel_lock;
use rusty_photon_ci::rust_toolchain_sha256s::{self, Release};

#[derive(Parser)]
#[command(about = "The workspace's developer tools. Run one with `cargo ci <tool>`.")]
struct Cli {
    #[command(subcommand)]
    tool: Tool,
}

#[derive(Subcommand)]
enum Tool {
    /// Render the Homebrew tap's formulas for one channel from the macOS
    /// tarballs
    BrewFormulas {
        #[arg(long)]
        channel: Channel,
        /// The formulas' version: a release's, or the full nightly stamp
        #[arg(long)]
        version: String,
        /// The tarballs and SHA256SUMS.txt
        #[arg(long)]
        dist: PathBuf,
        /// Where the formulas download from, or file://<dir> to verify locally
        #[arg(long)]
        url_base: String,
        /// Where the formulas are written
        #[arg(long)]
        output: PathBuf,
        /// Only these services, comma-separated, and no meta-formula
        #[arg(long, value_delimiter = ',')]
        services: Option<Vec<String>>,
    },
    /// Refresh MODULE.bazel.lock after a change to Cargo.lock or a workspace
    /// Cargo.toml, and stage it
    RepinBazelLock,
    /// Print the `sha256s` entries of MODULE.bazel's `rust.toolchain` for one
    /// Rust release, from its signed channel manifest (needs curl and gpg)
    RustToolchainSha256s {
        /// The stable pin (1.99.0) or the coverage nightly (nightly/2026-08-15)
        release: Release,
    },
}

fn main() -> ExitCode {
    match run(Cli::parse().tool) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn run(tool: Tool) -> Result<(), Box<dyn Error>> {
    match tool {
        Tool::BrewFormulas {
            channel,
            version,
            dist,
            url_base,
            output,
            services,
        } => {
            let root = brew_formulas::repo_root(&mut System)?;
            let request = Request {
                channel,
                version,
                dist,
                url_base,
                output,
                services,
            };
            for path in brew_formulas::render(&root, &request)? {
                println!("rendered {}", path.display());
            }
        }
        Tool::RepinBazelLock => {
            let report =
                repin_bazel_lock::repin(&mut System, std::env::vars_os().map(|(name, _)| name))?;
            println!("{report}");
        }
        Tool::RustToolchainSha256s { release } => {
            let sha256s = rust_toolchain_sha256s::sha256s(&mut System, &release)?;
            if let Some(comment) = sha256s.comment {
                eprintln!("{comment}");
            }
            print!("{}", sha256s.entries);
        }
    }
    Ok(())
}
