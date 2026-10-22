//! `rusty-photon-ci`: the workspace's developer tools, one subcommand each.
//! Run one with `cargo ci <tool>`.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use rusty_photon_ci::brew_formulas::{self, Channel};
use rusty_photon_ci::build_msi;
use rusty_photon_ci::build_packages;
use rusty_photon_ci::build_tarballs::{self, Host};
use rusty_photon_ci::packaging;
use rusty_photon_ci::process::{System, Wall};
use rusty_photon_ci::repin_bazel_lock;
use rusty_photon_ci::rust_toolchain_sha256s::{self, Release};
use rusty_photon_ci::verify_brew;
use rusty_photon_ci::verify_msi;
use rusty_photon_ci::verify_packages;

#[derive(Parser)]
#[command(about = "The workspace's developer tools. Run one with `cargo ci <tool>`.")]
struct Cli {
    #[command(subcommand)]
    tool: Tool,
}

#[derive(Subcommand)]
enum Tool {
    /// Build the services' .deb packages, and with --rpm their .rpm
    /// packages, into dist/<version>/
    BuildPackages {
        /// Only these services, comma-separated
        #[arg(long, value_delimiter = ',')]
        services: Option<Vec<String>>,
        /// The .rpm packages too
        #[arg(long)]
        rpm: bool,
        /// Download nothing: rebuild from the SDKs a previous run cached
        #[arg(long)]
        skip_sdk_staging: bool,
        /// The version to stamp: the workspace's, or a +nightly. stamp on it,
        /// which an rpm writes with a ^
        #[arg(long)]
        version: Option<String>,
    },
    /// Build the Windows suite MSI into dist/<version>/
    BuildMsi {
        /// Download nothing: rebuild from the SDKs a previous run cached
        #[arg(long)]
        skip_sdk_staging: bool,
        /// Take the binaries a previous run built and only build the installer
        #[arg(long)]
        skip_build: bool,
        /// The version to stamp: the workspace's, or a
        /// +nightly.<yyyymmddhhmm>.g<sha> stamp on it
        #[arg(long)]
        version: Option<String>,
    },
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
    /// Verify the suite MSI through its whole lifecycle: install, each
    /// service's checks, a feature's removal and the uninstall. Run it
    /// elevated, on a box with no install
    VerifyMsi {
        /// The MSI: dist/<v>/rusty-photon-<v>-x64.msi for the workspace's
        /// version when not given
        #[arg(long)]
        msi: Option<PathBuf>,
        /// An earlier MSI to install first, for the MSI to upgrade; an empty
        /// one, as when the nightly channel holds none yet, installs nothing
        #[arg(long)]
        upgrade_from: Option<String>,
        /// Leave the product installed, to debug
        #[arg(long)]
        keep: bool,
    },
    /// Verify the .deb packages, or with --rpm the .rpm packages, through
    /// their whole lifecycle in a container running systemd
    VerifyPackages {
        /// Only these services, comma-separated
        #[arg(long, value_delimiter = ',')]
        services: Option<Vec<String>>,
        /// The packages: dist/<workspace version> when not given
        #[arg(long)]
        dist: Option<PathBuf>,
        /// The .rpm packages, in a Fedora container
        #[arg(long)]
        rpm: bool,
        /// Leave the container, to debug
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

/// Prints what `dir` holds, under `title`.
fn list(title: &str, dir: &Path) -> Result<(), Box<dyn Error>> {
    println!("\n{title} in {}:", dir.display());
    let mut names: Vec<String> = std::fs::read_dir(dir)?
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    for name in names {
        println!("{name}");
    }
    Ok(())
}

fn build_linux(request: &build_packages::Request) -> Result<(), Box<dyn Error>> {
    let arch = build_packages::check_host(std::env::consts::OS, std::env::consts::ARCH)?;
    let root = packaging::repo_root(&mut System)?;
    let host = build_packages::Host::this(arch);
    let dist = build_packages::build(&mut System, &root, &host, request)?;
    list("Packages", &dist)
}

fn build_windows(request: &build_msi::Request) -> Result<(), Box<dyn Error>> {
    build_msi::check_host(std::env::consts::OS, std::env::consts::ARCH)?;
    let root = packaging::repo_root(&mut System)?;
    let dist = build_msi::build(&mut System, &root, &build_msi::Host::this(), request)?;
    list("Package", &dist)
}

fn verify_linux(request: &verify_packages::Request) -> Result<(), Box<dyn Error>> {
    let root = packaging::repo_root(&mut System)?;
    verify_packages::verify(&mut System, &mut Wall, &root, request)?;
    Ok(())
}

fn verify_macos(request: &verify_brew::Request) -> Result<(), Box<dyn Error>> {
    build_tarballs::check_host(std::env::consts::OS, std::env::consts::ARCH)?;
    let root = packaging::repo_root(&mut System)?;
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    verify_brew::verify(&mut System, &root, &home, request, std::thread::sleep)?;
    Ok(())
}

fn verify_windows(request: &verify_msi::Request) -> Result<(), Box<dyn Error>> {
    build_msi::check_host(std::env::consts::OS, std::env::consts::ARCH)?;
    let root = packaging::repo_root(&mut System)?;
    let places = verify_msi::Places::this();
    verify_msi::verify(&mut System, &mut Wall, &root, &places, request)?;
    Ok(())
}

fn build_macos(request: &build_tarballs::Request) -> Result<(), Box<dyn Error>> {
    build_tarballs::check_host(std::env::consts::OS, std::env::consts::ARCH)?;
    let root = packaging::repo_root(&mut System)?;
    let dist = build_tarballs::build(&mut System, &root, &Host::this(), request)?;
    list("Tarballs", &dist)
}

fn run(tool: Tool) -> Result<(), Box<dyn Error>> {
    match tool {
        Tool::BuildPackages {
            services,
            rpm,
            skip_sdk_staging,
            version,
        } => build_linux(&build_packages::Request {
            services,
            rpm,
            skip_sdk_staging,
            version,
        })?,
        Tool::VerifyPackages {
            services,
            dist,
            rpm,
            keep,
        } => verify_linux(&verify_packages::Request {
            dist,
            services,
            rpm,
            keep,
        })?,
        Tool::BuildMsi {
            skip_sdk_staging,
            skip_build,
            version,
        } => build_windows(&build_msi::Request {
            skip_sdk_staging,
            skip_build,
            version,
        })?,
        Tool::VerifyMsi {
            msi,
            upgrade_from,
            keep,
        } => verify_windows(&verify_msi::Request {
            msi,
            // The nightly passes an empty one when its channel holds no MSI.
            upgrade_from: upgrade_from
                .filter(|path| !path.is_empty())
                .map(PathBuf::from),
            keep,
        })?,
        Tool::BuildTarballs {
            services,
            skip_sdk_staging,
            version,
        } => build_macos(&build_tarballs::Request {
            services,
            skip_sdk_staging,
            version,
        })?,
        Tool::VerifyBrew {
            services,
            dist,
            channel,
            keep,
        } => verify_macos(&verify_brew::Request {
            channel,
            dist,
            services,
            keep,
        })?,
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
