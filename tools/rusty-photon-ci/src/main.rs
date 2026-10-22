//! `rusty-photon-ci`: the workspace's developer tools, one subcommand each.
//! Run one with `cargo ci <tool>`.

use std::error::Error;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use rusty_photon_ci::brew_formulas::{self, Channel};
use rusty_photon_ci::build_tarballs::{self, Host};
use rusty_photon_ci::packaging;
use rusty_photon_ci::process::System;
use rusty_photon_ci::repin_bazel_lock;
use rusty_photon_ci::rust_toolchain_sha256s::{self, Release};
use rusty_photon_ci::verify_brew;

#[derive(Parser)]
#[command(about = "The workspace's developer tools. Run one with `cargo ci <tool>`.")]
struct Cli {
    #[command(subcommand)]
    tool: Tool,
}

#[derive(Subcommand)]
enum Tool {
    /// Build the per-service macOS arm64 tarballs into dist/<version>/
    BuildTarballs {
        /// Only these services, comma-separated
        #[arg(long, value_delimiter = ',')]
        services: Option<Vec<String>>,
        /// Download nothing: rebuild from the SDKs a previous run cached
        #[arg(long)]
        skip_sdk_staging: bool,
        /// The version to stamp: the workspace's, or a +nightly. stamp on it
        #[arg(long)]
        version: Option<String>,
    },
    /// Verify the macOS tarballs through Homebrew: install, test, start,
    /// probe, stop and uninstall each service
    VerifyBrew {
        /// Only these services, comma-separated, and no meta-formula
        #[arg(long, value_delimiter = ',')]
        services: Option<Vec<String>>,
        /// The tarballs: dist/<workspace version> when not given
        #[arg(long)]
        dist: Option<PathBuf>,
        /// The channel whose formulas to render and install
        #[arg(long, default_value = "nightly")]
        channel: Channel,
        /// Leave the scratch tap and the installed formulas, to debug
        #[arg(long)]
        keep: bool,
    },
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
        Tool::BuildTarballs {
            services,
            skip_sdk_staging,
            version,
        } => {
            build_tarballs::check_host(std::env::consts::OS, std::env::consts::ARCH)?;
            let root = packaging::repo_root(&mut System)?;
            let request = build_tarballs::Request {
                services,
                skip_sdk_staging,
                version,
            };
            let dist = build_tarballs::build(&mut System, &root, &Host::this(), &request)?;
            println!("\nTarballs in {}:", dist.display());
            let mut names: Vec<String> = std::fs::read_dir(&dist)?
                .filter_map(Result::ok)
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            for name in names {
                println!("{name}");
            }
        }
        Tool::VerifyBrew {
            services,
            dist,
            channel,
            keep,
        } => {
            build_tarballs::check_host(std::env::consts::OS, std::env::consts::ARCH)?;
            let root = packaging::repo_root(&mut System)?;
            let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
            let request = verify_brew::Request {
                channel,
                dist,
                services,
                keep,
            };
            verify_brew::verify(&mut System, &root, &home, &request, std::thread::sleep)?;
        }
        Tool::BrewFormulas {
            channel,
            version,
            dist,
            url_base,
            output,
            services,
        } => {
            let root = packaging::repo_root(&mut System)?;
            let request = brew_formulas::Request {
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
