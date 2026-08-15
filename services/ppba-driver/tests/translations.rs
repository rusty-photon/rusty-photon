//! Asserts every shipped locale parses, has the same key set as the
//! fallback (`en`), and uses matching placeholder names per key.
//!
//! Runs under `cargo test`, `cargo nextest`, the workspace `cargo rail run
//! --profile commit -q` pre-push gate, GitHub Actions, and Bazel's
//! `:translations` test target — so any of those will fail if a
//! translator's PR introduces a parse error, drops a key, adds an unknown
//! key, or renames a `{ $var }` placeholder.
//!
//! Reads the `i18n/` tree at runtime via [`rusty_photon_i18n::verify_translations_in_dir`],
//! which walks a directory through its own `FsAssets` impl. We deliberately
//! avoid `RustEmbed` here: the verifier's job is to catch translator-side
//! mistakes (parse errors, dropped keys, renamed placeholders) in the
//! source tree *before* anything embeds them, so reading the actual files
//! on disk is the point — not an inconvenience to work around. Going
//! through the filesystem also sidesteps the spike's `debug-embed` trap
//! from the binary side: in dev / fastbuild builds `RustEmbed` defers to
//! a *runtime* walker rooted at the `CARGO_MANIFEST_DIR` value baked in
//! at compile time, and under `rules_rust` that path is a sandbox that
//! no longer exists when the test action runs.
//!
//! Resolving the runtime path differs by build system, so [`locate_i18n_dir`]
//! falls back through several candidates instead of trusting a single source:
//!
//! - **Cargo:** `env!("CARGO_MANIFEST_DIR")` expands to the package source
//!   directory at compile time and stays valid at test runtime — the tree
//!   committed at `services/ppba-driver/i18n/` is right there.
//! - **Bazel:** `rules_rust` sets `CARGO_MANIFEST_DIR` to a compile-time
//!   sandbox path that no longer exists when the test runs from the runfiles
//!   tree. Instead, the `data = glob(["i18n/**/*.ftl"])` on the `:translations`
//!   target stages the same files under
//!   `$TEST_SRCDIR/$TEST_WORKSPACE/services/ppba-driver/i18n/`, which
//!   `locate_i18n_dir` finds by composing the two env vars. `_main` is the
//!   default bzlmod workspace name but can be configured per project, so we
//!   read `TEST_WORKSPACE` rather than hard-coding it.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
// Curated test-scope allow list — documented in the root Cargo.toml [workspace.lints] block.
#![allow(
    clippy::needless_pass_by_ref_mut,
    clippy::needless_pass_by_value,
    clippy::unused_async,
    clippy::used_underscore_binding,
    clippy::significant_drop_tightening,
    clippy::significant_drop_in_scrutinee,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::cast_possible_wrap,
    clippy::suboptimal_flops,
    clippy::too_many_lines,
    clippy::option_if_let_else,
    clippy::match_same_arms,
    clippy::float_cmp,
    clippy::similar_names,
    clippy::struct_excessive_bools
)]

use std::path::{Path, PathBuf};

/// Find the `i18n/` directory at runtime by trying, in order:
/// `CARGO_MANIFEST_DIR/i18n` (Cargo), `$TEST_SRCDIR/$TEST_WORKSPACE/services/ppba-driver/i18n`
/// (Bazel runfiles — falling back to `_main` if `TEST_WORKSPACE` is unset),
/// and finally `<cwd>/services/ppba-driver/i18n`. See the module docs for
/// why each path is or isn't reliable per build system.
fn locate_i18n_dir() -> PathBuf {
    let mut tried = Vec::new();

    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("i18n");
    if manifest.is_dir() {
        return manifest;
    }
    tried.push(manifest);

    if let Ok(srcdir) = std::env::var("TEST_SRCDIR") {
        // `TEST_WORKSPACE` is the canonical name of the repo's main module
        // inside the runfiles tree (set by Bazel's test runner). `_main` is
        // the bzlmod default but projects can rename it, so prefer the env
        // var and only fall back to the default when it's absent.
        let workspace = std::env::var("TEST_WORKSPACE").unwrap_or_else(|_| "_main".into());
        let bazel = Path::new(&srcdir)
            .join(&workspace)
            .join("services/ppba-driver/i18n");
        if bazel.is_dir() {
            return bazel;
        }
        tried.push(bazel);
    }

    // Last resort: try both common CWD shapes — the package dir (cargo
    // nextest's default) and the workspace root (`cargo test` invoked
    // from there). Either is plausible if `CARGO_MANIFEST_DIR` is somehow
    // unset and we're not under a Bazel runner. Above branches cover the
    // common cases; this is pure defence-in-depth.
    if let Ok(cwd) = std::env::current_dir() {
        for candidate in [cwd.join("i18n"), cwd.join("services/ppba-driver/i18n")] {
            if candidate.is_dir() {
                return candidate;
            }
            tried.push(candidate);
        }
    }

    panic!(
        "could not locate ppba-driver i18n/ tree at runtime; candidates tried:\n{}",
        tried
            .iter()
            .map(|p| format!("  - {}", p.display()))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn all_locales_are_parseable_and_consistent_with_en() {
    let i18n_dir = locate_i18n_dir();
    let report = rusty_photon_i18n::verify_translations_in_dir(&i18n_dir, "en");
    assert!(
        report.is_clean(),
        "translation issues against fallback `{}` (locales: {:?}):\n{:#?}",
        report.fallback,
        report.locales,
        report.issues
    );
}
