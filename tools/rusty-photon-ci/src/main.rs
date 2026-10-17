//! `rusty-photon-ci`: the workspace's developer tools, one subcommand each.
//! Run one with `cargo ci <tool>`.

use std::process::ExitCode;

use clap::{Parser, Subcommand};
use rusty_photon_ci::process::System;
use rusty_photon_ci::repin_bazel_lock;

#[derive(Parser)]
#[command(about = "The workspace's developer tools. Run one with `cargo ci <tool>`.")]
struct Cli {
    #[command(subcommand)]
    tool: Tool,
}

#[derive(Subcommand)]
enum Tool {
    /// Refresh MODULE.bazel.lock after a change to Cargo.lock or a workspace
    /// Cargo.toml, and stage it
    RepinBazelLock,
}

fn main() -> ExitCode {
    let result = match Cli::parse().tool {
        Tool::RepinBazelLock => {
            repin_bazel_lock::repin(&mut System, std::env::vars_os().map(|(name, _)| name))
        }
    };
    match result {
        Ok(report) => {
            println!("{report}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}
