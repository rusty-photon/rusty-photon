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
pub mod rust_toolchain_sha256s;
#[cfg(test)]
mod toolchain_pins;

/// The repository's root, two levels above the crate, where the guards find
/// the files they read. Cargo sets `CARGO_MANIFEST_DIR` when it runs a test.
/// For Bazel the BUILD file sets it, relative to the runfiles root the test
/// starts in, which holds those files as the test's data.
#[cfg(test)]
fn repo_root() -> std::path::PathBuf {
    let crate_dir = std::env::var_os("CARGO_MANIFEST_DIR")
        .expect("CARGO_MANIFEST_DIR is set by Cargo, and for Bazel by the BUILD file");
    std::path::Path::new(&crate_dir)
        .ancestors()
        .nth(2)
        .expect("the crate sits two levels below the repository root")
        .to_path_buf()
}
