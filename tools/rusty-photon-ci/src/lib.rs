//! The workspace's developer tools, one module per tool. Each is a subcommand
//! of the `rusty-photon-ci` binary, which `cargo ci <tool>` runs (the alias is
//! in `.cargo/config.toml`). They take the place of shell scripts, by ADR-022;
//! docs/skills/ci-logic.md, "The Rust tools crate", says how to add one.
//!
//! The crate's tests also guard the repository's own files, each in a module
//! of its own that only the tests compile.

#[cfg(test)]
mod pool_routing;
pub mod process;
pub mod repin_bazel_lock;
