//! End-to-end tests for the CLI's i18n behaviour. Spawns the built binary so
//! we exercise the same code path operators see.
//!
//! Note on Fluent isolation marks: when Fluent interpolates a `{ $value }`
//! placeholder it wraps the value in U+2068 / U+2069 (bidirectional isolation
//! marks) so RTL/LTR mixing renders correctly. Tests therefore search for
//! prefix/suffix substrings rather than the literal user-supplied value.

#![allow(clippy::unwrap_used, clippy::expect_used)]
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

use std::process::Command;

fn bin() -> String {
    // Bazel supplies the binary via PPBA_DRIVER_BINARY ($(rootpath ...)); Cargo
    // sets CARGO_BIN_EXE_ppba-driver at compile time. option_env! (not env!)
    // keeps this compiling under Bazel, where the cargo-only var is absent.
    std::env::var("PPBA_DRIVER_BINARY")
        .ok()
        .or_else(|| option_env!("CARGO_BIN_EXE_ppba-driver").map(String::from))
        .expect("PPBA_DRIVER_BINARY (Bazel) or CARGO_BIN_EXE_ppba-driver (cargo) must be set")
}

/// Spawn the binary with locale env vars stripped, optionally setting `LANG`.
///
/// Tests pass `LANG=C` rather than relying on no-env behaviour because the
/// CI runner inherits its own `LANG` (commonly `en_US.UTF-8` or
/// `C.UTF-8`) and we want a deterministic English baseline regardless. The
/// test names encode this explicitly (`with_lang_c`) instead of implying a
/// "default" that depends on the runner.
fn run(env_locale: Option<&str>, args: &[&str]) -> (String, String, bool) {
    let mut cmd = Command::new(bin());
    cmd.env_remove("RP_LOCALE")
        .env_remove("LC_ALL")
        .env_remove("LC_MESSAGES")
        .env_remove("LANG");
    if let Some(locale) = env_locale {
        cmd.env("LANG", locale);
    }
    let output = cmd.args(args).output().expect("spawn binary");
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
        output.status.success(),
    )
}

#[test]
fn help_renders_english_with_lang_c() {
    let (stdout, _, ok) = run(Some("C"), &["--help"]);
    assert!(ok, "--help should exit 0");
    assert!(
        stdout.contains("ASCOM Alpaca driver for Pegasus Astro PPBA Gen2"),
        "english about line missing:\n{stdout}"
    );
    assert!(
        stdout.contains("Path to configuration file"),
        "english config help missing:\n{stdout}"
    );
    assert!(
        stdout.contains("Log level"),
        "english log-level help missing:\n{stdout}"
    );
}

#[test]
fn help_renders_german_when_lang_is_de() {
    let (stdout, _, ok) = run(Some("de_DE.UTF-8"), &["--help"]);
    assert!(ok, "--help should exit 0");
    assert!(
        stdout.contains("ASCOM-Alpaca-Treiber"),
        "german about line missing:\n{stdout}"
    );
    assert!(
        stdout.contains("Pfad zur Konfigurationsdatei"),
        "german config help missing:\n{stdout}"
    );
    assert!(
        stdout.contains("Protokollstufe"),
        "german log-level help missing:\n{stdout}"
    );
}

#[test]
fn invalid_log_level_error_renders_english_with_lang_c() {
    let (_, stderr, ok) = run(Some("C"), &["--log-level", "wat"]);
    assert!(!ok, "bad log level should exit non-zero");
    assert!(
        stderr.contains("Invalid log level:"),
        "english error prefix missing:\n{stderr}"
    );
    assert!(
        stderr.contains("trace, debug, info, warn, error"),
        "english error suffix missing:\n{stderr}"
    );
}

#[test]
fn invalid_log_level_error_renders_german_when_lang_is_de() {
    let (_, stderr, ok) = run(Some("de_DE.UTF-8"), &["--log-level", "wat"]);
    assert!(!ok, "bad log level should exit non-zero");
    assert!(
        stderr.contains("Ungültige Protokollstufe:"),
        "german error prefix missing:\n{stderr}"
    );
    assert!(
        stderr.contains("Verwende: trace, debug, info, warn, error"),
        "german error suffix missing:\n{stderr}"
    );
}

#[test]
fn rp_locale_env_overrides_lang() {
    let mut cmd = Command::new(bin());
    cmd.env_remove("LC_ALL")
        .env_remove("LC_MESSAGES")
        .env("LANG", "en_US.UTF-8")
        .env("RP_LOCALE", "de_DE.UTF-8")
        .arg("--help");
    let output = cmd.output().expect("spawn");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    // Belt-and-braces: assert exit success first so a future regression that
    // makes --help non-zero (parse error, panic, etc.) can't masquerade as a
    // pass just because the German substring shows up in some error path.
    assert!(
        output.status.success(),
        "--help should exit 0; status={:?}, stderr:\n{stderr}",
        output.status
    );
    assert!(
        stdout.contains("Protokollstufe"),
        "RP_LOCALE should win over LANG:\n{stdout}"
    );
}

#[test]
fn unsupported_locale_falls_back_to_english() {
    // `xx-YY` is a syntactically valid langid that we don't ship a translation
    // for. Fluent's negotiation should fall back to en.
    let (stdout, _, ok) = run(Some("xx_YY.UTF-8"), &["--help"]);
    assert!(ok);
    assert!(
        stdout.contains("ASCOM Alpaca driver for Pegasus Astro PPBA Gen2"),
        "unsupported locale should fall back to english:\n{stdout}"
    );
}
