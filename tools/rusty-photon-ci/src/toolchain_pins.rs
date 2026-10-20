//! Bazel's Rust toolchain pins agree with the MSRV and with each other.
//!
//! `MODULE.bazel`'s `rust.toolchain` pins two releases in its `versions`:
//! the stable pin, which every Bazel build compiles with, and the coverage
//! nightly, which `bazel coverage` compiles with. Its `sha256s` holds the
//! hashes of their archives. None of these mistakes is an error to Bazel,
//! so these tests check them, each on its own, so that one run names
//! every mismatch a bump left behind:
//!
//! - The stable pin is the MSRV's release (the root `Cargo.toml`'s
//!   `rust-version`), at or above the MSRV's patch level, so every PR
//!   compiles with the MSRV. `MODULE.bazel` says why.
//! - The line directly above the nightly's entry records its rustc version,
//!   as `cargo ci rust-toolchain-sha256s nightly/<date>` prints it. It must
//!   name the entry's date, and the MSRV's release or the next one, so a
//!   bump cannot leave the nightly behind. The next release is allowed
//!   because `MODULE.bazel` names it as the remedy for a stabilization the
//!   MSRV's last nightly lacks. The check shows the nightly's cycle, not
//!   that it is the right date in it; choosing that is the bump's job.
//! - `sha256s` holds exactly the archives of the two pins, as
//!   [`Release::archives`] lists them. A pin bumped without its hashes
//!   would otherwise download unverified.
//!
//! This is the one list of these checks; the docs point here.
//!
//! No parser in the workspace reads Starlark, so the `rust.toolchain` call
//! is read line by line, in the shape the hash tool prints: one entry per
//! line. Inside `versions` and `sha256s`, any other line but a comment or a
//! blank one fails the tests rather than being skipped ([`read_pins`]).

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use serde::Deserialize;

use crate::repo_root;
use crate::rust_toolchain_sha256s::Release;

/// What `rust.toolchain` pins.
#[derive(Debug, PartialEq, Eq)]
struct Pins {
    /// The stable pin: `1.99.0`.
    stable: String,
    /// The coverage nightly's date: `2026-08-15`.
    nightly: String,
    /// The line directly above the nightly's entry, when it is a comment.
    recorded: Option<String>,
    /// The keys of `sha256s`, in the file's order.
    keys: Vec<String>,
}

/// The pins in `module`, the text of `MODULE.bazel`. An error starts with
/// the line it is about.
fn read_pins(module: &str) -> Result<Pins, String> {
    let lines: Vec<(usize, &str)> = module.lines().zip(1..).map(|(line, n)| (n, line)).collect();
    let calls: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, (_, line))| *line == "rust.toolchain(")
        .map(|(at, _)| at)
        .collect();
    let [start] = calls[..] else {
        return Err(format!(
            "1: expected one `rust.toolchain(` line, found {}",
            calls.len()
        ));
    };
    let call = &lines[start..];
    let end = call
        .iter()
        .position(|(_, line)| *line == ")")
        .ok_or_else(|| format!("{}: `rust.toolchain(` has no closing `)` line", call[0].0))?;
    let call = &call[..end];

    let mut keys = Vec::new();
    for &(n, line) in section(call, "sha256s = {", "},")? {
        if is_comment(line) || line.trim().is_empty() {
            continue;
        }
        keys.push(sha256s_entry(line).ok_or_else(|| {
            format!(
                "{n}: in `sha256s`, expected `\"<key>\": \"<sha256>\",` on its own line: {line:?}"
            )
        })?);
    }

    let mut stable = Vec::new();
    let mut nightly = Vec::new();
    let mut above: Option<&str> = None;
    for &(n, line) in section(call, "versions = [", "],")? {
        if is_comment(line) {
            above = Some(line.trim());
            continue;
        }
        if line.trim().is_empty() {
            above = None;
            continue;
        }
        let release = quoted(line.trim(), ",")
            .and_then(|release| release.parse::<Release>().ok())
            .ok_or_else(|| {
                format!(
                    "{n}: in `versions`, expected a release `cargo ci rust-toolchain-sha256s` \
                     takes, quoted, on its own line: {line:?}"
                )
            })?;
        match release {
            Release::Stable(version) => stable.push(version),
            Release::Nightly(date) => nightly.push((date, above.map(str::to_owned))),
        }
        above = None;
    }
    let (Ok([stable]), Ok([(nightly, recorded)])) = (
        <[String; 1]>::try_from(stable.clone()),
        <[(String, Option<String>); 1]>::try_from(nightly.clone()),
    ) else {
        return Err(format!(
            "{}: `versions` must hold one stable pin and one nightly, found {stable:?} and {:?}",
            call[0].0,
            nightly.iter().map(|(date, _)| date).collect::<Vec<_>>()
        ));
    };
    Ok(Pins {
        stable,
        nightly,
        recorded,
        keys,
    })
}

/// The lines between the call's `open` line and the `close` line after it,
/// each trimmed of indentation before comparing. `open` must appear once.
fn section<'a>(
    call: &'a [(usize, &'a str)],
    open: &str,
    close: &str,
) -> Result<&'a [(usize, &'a str)], String> {
    let opens: Vec<usize> = call
        .iter()
        .enumerate()
        .filter(|(_, (_, line))| line.trim() == open)
        .map(|(at, _)| at)
        .collect();
    let [at] = opens[..] else {
        return Err(format!(
            "{}: expected one `{open}` line in `rust.toolchain`, found {}",
            call[0].0,
            opens.len()
        ));
    };
    let body = &call[at.saturating_add(1)..];
    let end = body
        .iter()
        .position(|(_, line)| line.trim() == close)
        .ok_or_else(|| format!("{}: `{open}` has no closing `{close}` line", call[at].0))?;
    Ok(&body[..end])
}

fn is_comment(line: &str) -> bool {
    line.trim_start().starts_with('#')
}

/// `"<key>": "<sha256>",` gives the key.
fn sha256s_entry(line: &str) -> Option<String> {
    let (key, hash) = line.trim().split_once(": ")?;
    let key = quoted(key, "")?;
    let hash = quoted(hash, ",")?;
    let is_sha256 = hash.len() == 64
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    (is_sha256 && !key.is_empty()).then(|| key.to_owned())
}

/// The text between the quotes of `"<text>"<suffix>`, which holds no quote.
fn quoted<'a>(text: &'a str, suffix: &str) -> Option<&'a str> {
    let inner = text
        .strip_suffix(suffix)?
        .strip_prefix('"')?
        .strip_suffix('"')?;
    (!inner.contains('"')).then_some(inner)
}

/// The workspace's `rust-version`, from the root `Cargo.toml`'s text.
fn msrv(cargo_toml: &str) -> Result<String, String> {
    #[derive(Deserialize)]
    struct Manifest {
        workspace: Workspace,
    }
    #[derive(Deserialize)]
    struct Workspace {
        package: Package,
    }
    #[derive(Deserialize)]
    struct Package {
        #[serde(rename = "rust-version")]
        rust_version: String,
    }
    toml::from_str::<Manifest>(cargo_toml)
        .map(|manifest| manifest.workspace.package.rust_version)
        .map_err(|e| format!("no `[workspace.package]` `rust-version`: {e}"))
}

/// The release a version names, as major and minor: 1.99 for `1.99`,
/// `1.99.0` or `1.99.1`.
fn release_of(version: &str) -> Option<(u32, u32)> {
    let parts = numbers(version)?;
    match parts[..] {
        [major, minor] | [major, minor, _] => Some((major, minor)),
        _ => None,
    }
}

/// A version's patch level: 1 for `1.99.1`, 0 for `1.99.0` or `1.99`.
fn patch_of(version: &str) -> Option<u32> {
    let parts = numbers(version)?;
    match parts[..] {
        [_, _] => Some(0),
        [_, _, patch] => Some(patch),
        _ => None,
    }
}

fn numbers(version: &str) -> Option<Vec<u32>> {
    version.split('.').map(|part| part.parse().ok()).collect()
}

/// `# nightly/<date> is rustc <version>-nightly (...)` gives the date and
/// the version.
fn recorded_version(line: &str) -> Option<(&str, &str)> {
    let (date, rest) = line.strip_prefix("# nightly/")?.split_once(" is rustc ")?;
    let (version, _) = rest.split_once("-nightly ")?;
    Some((date, version))
}

/// What `pins` and `msrv` disagree on, one message per check.
fn problems(pins: &Pins, msrv: &str) -> Vec<String> {
    let mut problems = Vec::new();
    let (Some(msrv_release), Some(msrv_patch)) = (release_of(msrv), patch_of(msrv)) else {
        return vec![format!(
            "the root Cargo.toml's rust-version {msrv:?} is not a version"
        )];
    };
    let (major, minor) = msrv_release;
    let next = minor.checked_add(1).map(|minor| (major, minor));
    let show = |(major, minor): (u32, u32)| format!("{major}.{minor}");

    if release_of(&pins.stable) != Some(msrv_release)
        || patch_of(&pins.stable).is_none_or(|patch| patch < msrv_patch)
    {
        problems.push(format!(
            "MODULE.bazel's stable Rust pin ({}) must be the MSRV's release, at or above the \
             MSRV itself (the root Cargo.toml's rust-version {msrv:?}), so every PR compiles \
             with the MSRV. Bump them together.",
            pins.stable
        ));
    }

    let recorded = pins.recorded.as_deref().and_then(recorded_version);
    let in_cycle = recorded.is_some_and(|(date, version)| {
        date == pins.nightly
            && release_of(version)
                .is_some_and(|release| release == msrv_release || Some(release) == next)
    });
    if !in_cycle {
        problems.push(format!(
            "the coverage nightly must come from the MSRV's ({}) cycle, or the next one's ({}), \
             and the line directly above its entry must record that; for nightly/{} it reads \
             {:?}. Paste the line `cargo ci rust-toolchain-sha256s nightly/<date>` prints.",
            show(msrv_release),
            next.map_or_else(|| "none".to_owned(), show),
            pins.nightly,
            pins.recorded.as_deref().unwrap_or("no recorded version"),
        ));
    }

    let expected: BTreeSet<String> = [
        Release::Stable(pins.stable.clone()),
        Release::Nightly(pins.nightly.clone()),
    ]
    .iter()
    .flat_map(Release::archives)
    .map(|archive| archive.key)
    .collect();
    let mut keys = BTreeSet::new();
    let repeated: BTreeSet<&str> = pins
        .keys
        .iter()
        .filter(|key| !keys.insert(key.as_str()))
        .map(String::as_str)
        .collect();
    let missing: Vec<&str> = expected
        .iter()
        .map(String::as_str)
        .filter(|key| !keys.contains(key))
        .collect();
    let unexpected: Vec<&str> = keys
        .iter()
        .copied()
        .filter(|key| !expected.contains(*key))
        .collect();
    if !missing.is_empty() || !unexpected.is_empty() || !repeated.is_empty() {
        problems.push(format!(
            "MODULE.bazel's sha256s must hold exactly the archives of the stable pin ({}) and \
             of the coverage nightly ({}), once each, as `cargo ci rust-toolchain-sha256s \
             <release>` prints them. Regenerate them with it, or the toolchains download \
             unverified.\n  missing: {missing:?}\n  unexpected: {unexpected:?}\n  repeated: \
             {repeated:?}",
            pins.stable, pins.nightly
        ));
    }
    problems
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

#[test]
fn the_toolchain_pins_agree_with_the_msrv_and_each_other() {
    let root = repo_root();
    let pins =
        read_pins(&read(&root.join("MODULE.bazel"))).unwrap_or_else(|e| panic!("MODULE.bazel:{e}"));
    let msrv = msrv(&read(&root.join("Cargo.toml"))).unwrap_or_else(|e| panic!("Cargo.toml: {e}"));
    let problems = problems(&pins, &msrv);
    assert!(problems.is_empty(), "{}", problems.join("\n\n"));
}

const RECORDED: &str = "# nightly/2026-08-15 is rustc 1.99.0-nightly (d453bdd8f 2026-08-14)";

/// The keys of `stable`'s and `nightly`'s archives.
fn keys_of(stable: &str, nightly: &str) -> Vec<String> {
    [
        Release::Stable(stable.to_owned()),
        Release::Nightly(nightly.to_owned()),
    ]
    .iter()
    .flat_map(Release::archives)
    .map(|archive| archive.key)
    .collect()
}

/// Pins that agree with an MSRV of 1.99.0.
fn agreeing() -> Pins {
    Pins {
        stable: "1.99.0".to_owned(),
        nightly: "2026-08-15".to_owned(),
        recorded: Some(RECORDED.to_owned()),
        keys: keys_of("1.99.0", "2026-08-15"),
    }
}

/// A `MODULE.bazel` around a `rust.toolchain` call that pins `pins`, laid
/// out as the real one is.
fn module(pins: &Pins) -> String {
    let mut lines = vec![
        "bazel_dep(name = \"rules_rust\", version = \"0.74.0\")".to_owned(),
        String::new(),
        "rust = use_extension(\"@rules_rust//rust:extensions.bzl\", \"rust\")".to_owned(),
        "rust.toolchain(".to_owned(),
        "    edition = \"2021\",".to_owned(),
        "    # The hashes.".to_owned(),
        "    sha256s = {".to_owned(),
    ];
    let hash = "0a".repeat(32);
    lines.extend(
        pins.keys
            .iter()
            .map(|key| format!("        \"{key}\": \"{hash}\",")),
    );
    lines.push("    },".to_owned());
    lines.push("    # The pins.".to_owned());
    lines.push("    versions = [".to_owned());
    lines.push(format!("        \"{}\",", pins.stable));
    lines.extend(
        pins.recorded
            .iter()
            .map(|recorded| format!("        {recorded}")),
    );
    lines.push(format!("        \"nightly/{}\",", pins.nightly));
    lines.push("    ],".to_owned());
    lines.push(")".to_owned());
    lines.push("use_repo(rust, \"rust_toolchains\")".to_owned());
    lines.join("\n") + "\n"
}

#[test]
fn reads_the_pins_of_a_call_laid_out_as_the_tool_prints_them() {
    assert_eq!(read_pins(&module(&agreeing())), Ok(agreeing()));
}

#[test]
fn reads_no_recorded_version_when_the_line_above_the_nightly_is_not_a_comment() {
    let recorded = format!("        {RECORDED}\n");
    let text = module(&agreeing()).replace(&recorded, "");
    assert_eq!(read_pins(&text).unwrap().recorded, None);
    // A comment above the stable pin is not the nightly's.
    let above_stable = text.replace(
        "        \"1.99.0\",\n",
        &format!("{recorded}        \"1.99.0\",\n"),
    );
    assert_eq!(read_pins(&above_stable).unwrap().recorded, None);
    // Nor is one a blank line separates from it.
    let separated = module(&agreeing()).replace(&recorded, &format!("{recorded}\n"));
    assert_eq!(read_pins(&separated).unwrap().recorded, None);
}

#[test]
fn skips_a_comment_inside_sha256s() {
    let text =
        module(&agreeing()).replace("    sha256s = {\n", "    sha256s = {\n        # Stable.\n");
    assert_eq!(read_pins(&text), Ok(agreeing()));
}

#[test]
fn refuses_a_sha256s_line_of_another_shape() {
    let pins = agreeing();
    let key = &pins.keys[3];
    let entry = format!("        \"{key}\": \"{}\",\n", "0a".repeat(32));
    for other in [
        // Two entries on one line.
        format!(
            "        \"{key}\": \"{}\", \"x.tar.xz\": \"{}\",\n",
            "0a".repeat(32),
            "0b".repeat(32)
        ),
        // A hash that is not a SHA-256.
        format!("        \"{key}\": \"{}\",\n", "0a".repeat(31)),
        // An entry built by Starlark.
        format!("        \"{key}\": HASHES[\"{key}\"],\n"),
        // Two keys sharing one hash.
        format!(
            "        \"{key}\", \"x.tar.xz\": \"{}\",\n",
            "0a".repeat(32)
        ),
    ] {
        let text = module(&agreeing()).replace(&entry, &other);
        let error = read_pins(&text).unwrap_err();
        assert!(error.contains("in `sha256s`"), "{error}");
    }
}

#[test]
fn refuses_a_versions_entry_the_hash_tool_does_not_take() {
    let text = module(&agreeing()).replace("        \"1.99.0\",\n", "        \"1.99\",\n");
    let error = read_pins(&text).unwrap_err();
    assert!(error.contains("in `versions`"), "{error}");
}

#[test]
fn refuses_versions_without_one_stable_pin_and_one_nightly() {
    let two_stable = module(&agreeing()).replace(
        "        \"1.99.0\",\n",
        "        \"1.99.0\",\n        \"1.98.1\",\n",
    );
    let error = read_pins(&two_stable).unwrap_err();
    assert!(error.contains("one stable pin and one nightly"), "{error}");
    let no_nightly = module(&agreeing()).replace("        \"nightly/2026-08-15\",\n", "");
    let error = read_pins(&no_nightly).unwrap_err();
    assert!(error.contains("one stable pin and one nightly"), "{error}");
}

#[test]
fn refuses_a_module_without_one_toolchain_call() {
    let text = module(&agreeing());
    let error = read_pins(&text.replace("rust.toolchain(", "rust.toolchains(")).unwrap_err();
    assert!(error.contains("found 0"), "{error}");
    let error = read_pins(&format!("{text}{text}")).unwrap_err();
    assert!(error.contains("found 2"), "{error}");
}

#[test]
fn refuses_a_call_without_one_sha256s_and_one_versions() {
    let text = module(&agreeing());
    let error = read_pins(&text.replace("    sha256s = {\n", "    hashes = {\n")).unwrap_err();
    assert!(
        error.contains("one `sha256s = {` line in `rust.toolchain`, found 0"),
        "{error}"
    );
    let twice = text.replace(
        "    versions = [\n",
        "    versions = [\n    ],\n    versions = [\n",
    );
    let error = read_pins(&twice).unwrap_err();
    assert!(
        error.contains("one `versions = [` line in `rust.toolchain`, found 2"),
        "{error}"
    );
}

#[test]
fn reads_the_workspace_msrv() {
    let cargo = "[workspace]\nmembers = []\n\n[workspace.package]\nedition = \"2021\"\nrust-version = \"1.99.0\"\n";
    assert_eq!(msrv(cargo), Ok("1.99.0".to_owned()));
    msrv("[workspace]\nmembers = []\n").unwrap_err();
}

#[test]
fn agreeing_pins_have_no_problems() {
    assert_eq!(problems(&agreeing(), "1.99.0"), Vec::<String>::new());
    assert_eq!(problems(&agreeing(), "1.99"), Vec::<String>::new());
}

#[test]
fn a_stable_pin_above_the_msrvs_patch_level_agrees() {
    let pins = Pins {
        stable: "1.99.1".to_owned(),
        keys: keys_of("1.99.1", "2026-08-15"),
        ..agreeing()
    };
    assert_eq!(problems(&pins, "1.99.0"), Vec::<String>::new());
}

#[test]
fn refuses_a_stable_pin_of_another_release() {
    let pins = Pins {
        stable: "1.100.0".to_owned(),
        keys: keys_of("1.100.0", "2026-08-15"),
        ..agreeing()
    };
    let problems = problems(&pins, "1.99.0");
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(
        problems[0].contains("stable Rust pin (1.100.0)"),
        "{problems:?}"
    );
}

#[test]
fn refuses_a_stable_pin_below_the_msrvs_patch_level() {
    let problems = problems(&agreeing(), "1.99.1");
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(
        problems[0].contains("stable Rust pin (1.99.0)"),
        "{problems:?}"
    );
}

#[test]
fn a_nightly_of_the_next_cycle_agrees() {
    let pins = Pins {
        recorded: Some(RECORDED.replace("1.99.0-nightly", "1.100.0-nightly")),
        ..agreeing()
    };
    assert_eq!(problems(&pins, "1.99.0"), Vec::<String>::new());
}

#[test]
fn refuses_a_nightly_outside_the_msrvs_cycle_and_the_next() {
    for version in ["1.98.0", "1.101.0", "2.99.0"] {
        let pins = Pins {
            recorded: Some(RECORDED.replace("1.99.0-nightly", &format!("{version}-nightly"))),
            ..agreeing()
        };
        let problems = problems(&pins, "1.99.0");
        assert_eq!(problems.len(), 1, "{version}: {problems:?}");
        assert!(
            problems[0].contains("coverage nightly"),
            "{version}: {problems:?}"
        );
    }
}

#[test]
fn refuses_a_recorded_version_of_another_date_or_shape() {
    for recorded in [
        Some(RECORDED.replace("nightly/2026-08-15", "nightly/2026-08-14")),
        Some("# the coverage nightly".to_owned()),
        Some(RECORDED.replace("-nightly ", " ")),
        None,
    ] {
        let pins = Pins {
            recorded: recorded.clone(),
            ..agreeing()
        };
        let problems = problems(&pins, "1.99.0");
        assert_eq!(problems.len(), 1, "{recorded:?}: {problems:?}");
        assert!(
            problems[0].contains("coverage nightly"),
            "{recorded:?}: {problems:?}"
        );
    }
}

#[test]
fn refuses_sha256s_missing_an_archive() {
    let mut pins = agreeing();
    let gone = pins.keys.remove(7);
    let problems = problems(&pins, "1.99.0");
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(
        problems[0].contains(&format!("missing: [{gone:?}]")),
        "{problems:?}"
    );
}

#[test]
fn refuses_sha256s_with_an_archive_of_another_release() {
    let mut pins = agreeing();
    pins.keys
        .push("rustc-1.98.1-x86_64-unknown-linux-gnu.tar.xz".to_owned());
    let problems = problems(&pins, "1.99.0");
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(
        problems[0].contains("unexpected: [\"rustc-1.98.1-x86_64-unknown-linux-gnu.tar.xz\"]"),
        "{problems:?}"
    );
}

#[test]
fn refuses_sha256s_naming_an_archive_twice() {
    let mut pins = agreeing();
    pins.keys.push(pins.keys[0].clone());
    let problems = problems(&pins, "1.99.0");
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(
        problems[0].contains(&format!("repeated: {{{:?}}}", pins.keys[0])),
        "{problems:?}"
    );
}

#[test]
fn reports_each_disagreement_on_its_own() {
    let pins = Pins {
        recorded: None,
        keys: Vec::new(),
        ..agreeing()
    };
    assert_eq!(problems(&pins, "1.99.1").len(), 3);
}

#[test]
fn refuses_an_msrv_that_is_not_a_version() {
    let problems = problems(&agreeing(), "stable");
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].contains("is not a version"), "{problems:?}");
}
