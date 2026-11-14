//! Comments name no issue or pull request by bare number.
//!
//! A comment is a living document: it says what the code does and why it is
//! not otherwise, in the present tense, and cites a record only as a full
//! URL or a `docs/` path (AGENTS.md rule 16, docs/workspace.md § Comments).
//! A `#` followed by a number does neither: every reader of the line pays to
//! fetch what it withholds. This test fails `bazel test //...` on one in
//!
//! - every workspace member: the comments of its Rust, TOML and Starlark
//!   files, and the whole of its feature files, which are prose end to end;
//! - the repository's own files: the comments of the root `Cargo.toml`,
//!   `MODULE.bazel`, `BUILD.bazel` and `.bazelrc`, of the workflows, and of
//!   the local actions' JavaScript, YAML and Starlark.
//!
//! A number is two to four digits, the first not 0: every number the
//! tracker has given past its first nine, and no list item (`#1`) or hex
//! colour (`#000`). A `&#39;` is an entity and `testing.md#68-do-not-assert`
//! a heading's anchor. A full URL has no `#` before a number, so a citation
//! passes as it is.
//!
//! [`PENDING`] holds the numbers the cleanup has not reached: for each file,
//! each number its comments name and how often. A row whose number the file
//! no longer names is the cleanup's progress, recorded by deleting the row
//! or lowering its count; a number no row holds is a new breach, fixed in
//! the comment and never by a row. So a clean file stays clean, and a
//! pending one grows no worse.
//!
//! What is a comment is decided by syntax: a Rust lexer that knows strings,
//! raw strings, char literals and lifetimes, so a `//` inside a string is
//! none; a JavaScript one that knows strings, regex literals, and template
//! literals with the code inside their `${ }`; and for TOML, Starlark,
//! `.bazelrc` and YAML a `#` outside quotes that starts the line or follows
//! a space, YAML's own definition of a comment. A YAML parser drops
//! comments, so this is the one guard that reads a workflow's lines
//! (docs/skills/ci-logic.md).
//!
//! Under Bazel the test sees only its `data`: every member's `sources`
//! filegroup, which its BUILD file declares, and the root's files. A unit
//! whose files are missing fails the test rather than passing unread.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crate::repo_root;

/// The numbers the cleanup has not reached, in path order.
///
/// Each row is a file, a number its comments name as written, and how often
/// they name it. Delete a row, or lower its count, once the file names the
/// number less often; never add one. A row follows its file when the file
/// moves.
const PENDING: &[(&str, &str, usize)] = &[
    (".bazelrc", "#342", 2),
    (".bazelrc", "#452", 1),
    (".bazelrc", "#467", 1),
    (".bazelrc", "#624", 2),
    (".bazelrc", "#676", 1),
    (".bazelrc", "#781", 1),
    (".github/actions/install-omnisim/omnisim.mjs", "#319", 2),
    (".github/actions/install-omnisim/omnisim.mjs", "#326", 1),
    (".github/actions/install-omnisim/omnisim.mjs", "#467", 1),
    (".github/actions/lib/install.mjs", "#5360", 1),
    (".github/actions/lib/install.test.mjs", "#5360", 1),
    (".github/workflows/bazel.yml", "#1120", 1),
    (".github/workflows/bazel.yml", "#467", 1),
    (".github/workflows/check.yml", "#984", 2),
    (".github/workflows/check.yml", "#988", 3),
    (".github/workflows/check.yml", "#990", 1),
    (".github/workflows/plate-solver-smoke.yml", "#234", 1),
    (".github/workflows/plate-solver-smoke.yml", "#236", 1),
    (".github/workflows/ui-browser-nightly.yml", "#494", 1),
    ("MODULE.bazel", "#3490", 1),
    ("MODULE.bazel", "#3808", 1),
    ("MODULE.bazel", "#587", 2),
    ("MODULE.bazel", "#739", 1),
    ("MODULE.bazel", "#749", 1),
    ("MODULE.bazel", "#752", 2),
    ("MODULE.bazel", "#781", 1),
    ("MODULE.bazel", "#796", 1),
    ("crates/bdd-infra/BUILD.bazel", "#431", 1),
    ("crates/bdd-infra/src/lib.rs", "#578", 3),
    ("crates/bdd-infra/src/rp_harness/mcp_client.rs", "#319", 1),
    ("crates/bdd-infra/src/rp_harness/omnisim.rs", "#171", 5),
    ("crates/bdd-infra/src/rp_harness/omnisim.rs", "#172", 1),
    ("crates/bdd-infra/src/rp_harness/omnisim.rs", "#431", 4),
    ("crates/bdd-infra/src/rp_harness/omnisim.rs", "#467", 3),
    (
        "crates/bdd-infra/src/rp_harness/plate_solver_stub.rs",
        "#434",
        1,
    ),
    ("crates/bdd-infra/tests/service_handle.rs", "#578", 1),
    (
        "crates/qhyccd-rs/tests/simulation/camera_tests.rs",
        "#11",
        1,
    ),
    ("crates/rp-guider/BUILD.bazel", "#609", 1),
    ("crates/rp-guider/BUILD.bazel", "#620", 1),
    ("crates/rp-guider/src/lib.rs", "#609", 2),
    ("crates/rp-guider/src/lib.rs", "#620", 1),
    ("crates/rp-plate-solver/BUILD.bazel", "#609", 1),
    ("crates/rp-plate-solver/BUILD.bazel", "#620", 1),
    ("crates/rp-plate-solver/src/lib.rs", "#609", 2),
    ("crates/rp-plate-solver/src/lib.rs", "#620", 1),
    ("crates/rusty-photon-driver/src/discovery.rs", "#63", 1),
    (
        "crates/rusty-photon-service-lifecycle/src/lib.rs",
        "#289",
        1,
    ),
    (
        "crates/rusty-photon-service-lifecycle/src/lib.rs",
        "#294",
        1,
    ),
    (
        "crates/rusty-photon-shared-transport/src/connection.rs",
        "#1371",
        1,
    ),
    (
        "crates/rusty-photon-shared-transport/src/shared.rs",
        "#1250",
        2,
    ),
    (
        "crates/rusty-photon-shared-transport/src/shared.rs",
        "#1251",
        2,
    ),
    (
        "crates/rusty-photon-shared-transport/src/transport.rs",
        "#622",
        1,
    ),
    (
        "crates/rusty-photon-shared-transport/tests/common/mod.rs",
        "#1250",
        1,
    ),
    (
        "crates/rusty-photon-shared-transport/tests/common/mod.rs",
        "#269",
        1,
    ),
    (
        "crates/rusty-photon-shared-transport/tests/lifecycle.rs",
        "#1250",
        1,
    ),
    (
        "crates/rusty-photon-shared-transport/tests/lifecycle.rs",
        "#1251",
        1,
    ),
    (
        "crates/rusty-photon-shared-transport/tests/race.rs",
        "#250",
        1,
    ),
    (
        "crates/rusty-photon-shared-transport/tests/race.rs",
        "#251",
        1,
    ),
    (
        "crates/rusty-photon-shared-transport/tests/race.rs",
        "#257",
        1,
    ),
    (
        "crates/rusty-photon-shared-transport/tests/rollback.rs",
        "#258",
        2,
    ),
    (
        "crates/rusty-photon-shared-transport/tests/rollback.rs",
        "#260",
        1,
    ),
    (
        "crates/rusty-photon-shared-transport/tests/rollback.rs",
        "#269",
        1,
    ),
    ("crates/rusty-photon-tls/src/server.rs", "#610", 2),
    ("crates/rusty-photon-tls/src/test_cert.rs", "#621", 1),
    ("crates/rusty-photon-tls/tests/tls_roundtrip.rs", "#610", 1),
    ("crates/svbony-rs/libsvbony-sys/build.rs", "#681", 1),
    ("crates/svbony-rs/libsvbony-sys/lib.rs", "#681", 1),
    ("crates/zwo-rs/examples/probe_ceiling.rs", "#888", 1),
    ("crates/zwo-rs/examples/probe_formats.rs", "#881", 1),
    ("crates/zwo-rs/src/error.rs", "#1431", 1),
    ("crates/zwo-rs/src/lib.rs", "#1411", 2),
    ("crates/zwo-rs/src/lib.rs", "#1431", 1),
    ("services/calibrator-flats/tests/bdd.rs", "#172", 1),
    ("services/doctor/src/checks.rs", "#1301", 1),
    ("services/doctor/src/checks.rs", "#569", 1),
    ("services/doctor/src/checks.rs", "#607", 1),
    ("services/doctor/src/checks.rs", "#609", 1),
    ("services/doctor/src/checks.rs", "#612", 1),
    ("services/doctor/src/checks.rs", "#616", 2),
    ("services/doctor/src/checks.rs", "#620", 3),
    ("services/doctor/src/checks.rs", "#663", 4),
    ("services/doctor/src/checks.rs", "#800", 2),
    ("services/doctor/src/checks.rs", "#805", 4),
    ("services/doctor/src/lib.rs", "#616", 1),
    ("services/doctor/src/provision/cert.rs", "#621", 1),
    ("services/doctor/src/provision/mod.rs", "#609", 1),
    ("services/doctor/src/provision/mod.rs", "#616", 2),
    ("services/doctor/src/scan.rs", "#609", 1),
    ("services/doctor/src/scan.rs", "#612", 1),
    ("services/doctor/src/scan.rs", "#620", 2),
    ("services/doctor/src/scan.rs", "#663", 1),
    ("services/doctor/src/scan.rs", "#800", 2),
    (
        "services/doctor/tests/features/client_target_joins.feature",
        "#607",
        1,
    ),
    (
        "services/doctor/tests/features/client_target_joins.feature",
        "#663",
        1,
    ),
    (
        "services/doctor/tests/features/client_target_joins.feature",
        "#805",
        1,
    ),
    (
        "services/doctor/tests/features/name_joins.feature",
        "#569",
        1,
    ),
    (
        "services/doctor/tests/features/url_conventions.feature",
        "#569",
        1,
    ),
    ("services/dsd-fp2/src/mock.rs", "#423", 1),
    ("services/pa-falcon-rotator/src/manager.rs", "#280", 1),
    ("services/pa-falcon-rotator/src/manager.rs", "#949", 1),
    (
        "services/pa-falcon-rotator/src/rotator_device.rs",
        "#241",
        4,
    ),
    (
        "services/pa-falcon-rotator/src/rotator_device.rs",
        "#251",
        1,
    ),
    (
        "services/pa-falcon-rotator/src/rotator_device.rs",
        "#282",
        1,
    ),
    ("services/pa-falcon-rotator/src/switch_device.rs", "#241", 1),
    ("services/pa-falcon-rotator/tests/bdd.rs", "#171", 1),
    ("services/pa-scops-oag/src/focuser_device.rs", "#250", 1),
    ("services/phd2-guider/src/connection.rs", "#603", 1),
    ("services/phd2-guider/tests/bdd/world.rs", "#342", 1),
    ("services/phd2-guider/tests/test_integration.rs", "#287", 1),
    ("services/phd2-guider/tests/test_integration.rs", "#294", 2),
    ("services/phd2-guider/tests/test_integration.rs", "#342", 1),
    ("services/phd2-guider/tests/test_integration.rs", "#603", 1),
    ("services/plate-solver/src/runner/wcs.rs", "#160", 2),
    ("services/plate-solver/tests/bdd.rs", "#233", 1),
    (
        "services/plate-solver/tests/bdd/steps/solve_steps.rs",
        "#236",
        1,
    ),
    (
        "services/plate-solver/tests/features/real_astap_smoke.feature",
        "#236",
        2,
    ),
    ("services/ppba-driver/src/switch_device.rs", "#251", 1),
    ("services/qhy-camera/src/backend.rs", "#370", 1),
    ("services/qhy-camera/tests/bdd.rs", "#171", 1),
    ("services/qhy-focuser/src/focuser_device.rs", "#250", 2),
    ("services/qhy-focuser/src/focuser_device.rs", "#258", 1),
    ("services/rp/BUILD.bazel", "#467", 1),
    ("services/rp/src/equipment/alpaca.rs", "#319", 3),
    ("services/rp/src/equipment/alpaca.rs", "#609", 3),
    ("services/rp/src/equipment/mod.rs", "#609", 1),
    ("services/rp/src/equipment/supervisor.rs", "#1138", 1),
    ("services/rp/src/equipment/test_support.rs", "#111", 1),
    ("services/rp/src/events.rs", "#800", 1),
    (
        "services/rp/src/imaging/tools/center_on_target.rs",
        "#319",
        1,
    ),
    ("services/rp/src/lib.rs", "#609", 1),
    ("services/rp/src/lib.rs", "#792", 1),
    ("services/rp/src/mcp/internals.rs", "#319", 1),
    ("services/rp/src/mcp/tests.rs", "#1163", 1),
    ("services/rp/src/mcp/tests.rs", "#175", 1),
    ("services/rp/src/mcp/tests.rs", "#319", 2),
    ("services/rp/tests/bdd.rs", "#143", 1),
    ("services/rp/tests/bdd.rs", "#172", 1),
    ("services/rp/tests/bdd/steps/mount_steps.rs", "#1252", 1),
    ("services/sentinel/src/health.rs", "#595", 1),
    ("services/sentinel/src/lib.rs", "#810", 1),
    ("services/sentinel/src/state.rs", "#595", 1),
    (
        "services/sentinel/tests/bdd/steps/health_steps.rs",
        "#677",
        1,
    ),
    ("services/sentinel/tests/bdd/world.rs", "#677", 2),
    ("services/svbony-camera/src/camera.rs", "#882", 1),
    ("services/ui-htmx/Cargo.toml", "#2673", 2),
    ("services/ui-htmx/src/config.rs", "#569", 3),
    ("services/ui-htmx/src/lib.rs", "#2673", 1),
    ("services/ui-htmx/src/main.rs", "#2673", 1),
    ("services/ui-htmx/src/sse_fixtures.rs", "#2673", 2),
    ("services/ui-htmx/src/sse_proxy.rs", "#2673", 2),
    ("services/ui-htmx/tests/features/sse.feature", "#2673", 1),
    ("services/zwo-camera/src/camera.rs", "#881", 1),
];

/// This file, for the messages.
const HERE: &str = "tools/rusty-photon-ci/src/bare_refs.rs";

/// How a file's comments are found.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Syntax {
    /// `//` and nested `/* */`, outside strings, raw strings and char
    /// literals.
    Rust,
    /// `//` and `/* */`, outside strings, regex literals and template
    /// literals.
    Js,
    /// `#` outside quotes, starting the line or following a space.
    Hash,
    /// Every line.
    Prose,
}

/// What a unit's files are: one file, or the files under a directory that
/// have these extensions.
#[derive(Clone, Copy)]
enum Reads {
    File(Syntax),
    Dir(&'static [(&'static str, Syntax)]),
}

/// A member's files. Its BUILD file's `sources` filegroup globs the same.
const MEMBER_FILES: &[(&str, Syntax)] = &[
    ("rs", Syntax::Rust),
    ("feature", Syntax::Prose),
    ("toml", Syntax::Hash),
    ("bzl", Syntax::Hash),
    ("bazel", Syntax::Hash),
];

/// The workflows, as the root BUILD file's `workflows` filegroup globs them.
const WORKFLOW_FILES: &[(&str, Syntax)] = &[("yml", Syntax::Hash), ("yaml", Syntax::Hash)];

/// The local actions' files, as `.github/actions`' `sources` filegroup
/// globs them.
const ACTION_FILES: &[(&str, Syntax)] = &[
    ("mjs", Syntax::Js),
    ("yml", Syntax::Hash),
    ("yaml", Syntax::Hash),
    ("bzl", Syntax::Hash),
    ("bazel", Syntax::Hash),
];

/// Where the test reads: a workspace member's directory, or one of the
/// repository's own directories or files.
struct Unit {
    /// The member's path, or the directory's or file's.
    name: String,
    /// The target that gives the unit's files to the test under Bazel,
    /// named when they are missing.
    label: String,
    reads: Reads,
    /// Whether the directory is a Bazel package, whose `BUILD.bazel` must
    /// then be among the files read.
    package: bool,
}

/// The repository's own files and directories the test reads, with the
/// target that gives each to the test under Bazel.
fn root_units() -> Vec<Unit> {
    let file = |name: &str, label: &str| Unit {
        name: name.to_owned(),
        label: label.to_owned(),
        reads: Reads::File(Syntax::Hash),
        package: false,
    };
    let dir = |name: &str, label: &str, files: &'static [(&'static str, Syntax)], package| Unit {
        name: name.to_owned(),
        label: label.to_owned(),
        reads: Reads::Dir(files),
        package,
    };
    vec![
        file("Cargo.toml", "//:Cargo.toml"),
        file("MODULE.bazel", "//:MODULE.bazel"),
        file("BUILD.bazel", "//:BUILD.bazel"),
        file(".bazelrc", "//:.bazelrc"),
        dir(".github/workflows", "//:workflows", WORKFLOW_FILES, false),
        dir(
            ".github/actions",
            "//.github/actions:sources",
            ACTION_FILES,
            true,
        ),
    ]
}

/// Every unit: each of `members`, then the root's own.
fn units(members: &[String]) -> Vec<Unit> {
    members
        .iter()
        .map(|member| Unit {
            name: member.clone(),
            label: format!("//{member}:sources"),
            reads: Reads::Dir(MEMBER_FILES),
            package: true,
        })
        .chain(root_units())
        .collect()
}

/// The workspace's members, as the root `Cargo.toml` lists them. Each is a
/// literal path, since the test reads the directory it names.
fn members(cargo_toml: &str) -> Result<Vec<String>, String> {
    let table: toml::Table = cargo_toml
        .parse()
        .map_err(|error| format!("Cargo.toml: {error}"))?;
    let members = table
        .get("workspace")
        .and_then(toml::Value::as_table)
        .and_then(|workspace| workspace.get("members"))
        .and_then(toml::Value::as_array)
        .ok_or("Cargo.toml: no [workspace] members")?;
    members
        .iter()
        .map(|member| match member.as_str() {
            Some(path) if !path.contains(['*', '?', '[']) => Ok(path.to_owned()),
            _ => Err(format!(
                "Cargo.toml: the member {member:?} is not a literal path, which the test reads"
            )),
        })
        .collect()
}

/// The files of `unit` under `root`, each with its syntax.
///
/// Paths are repo-relative with `/`, in order. A directory that is another
/// of `members` belongs to that member, and `target` to nobody.
fn files(root: &Path, unit: &Unit, members: &[String]) -> Vec<(String, Syntax)> {
    let extensions = match unit.reads {
        Reads::File(syntax) => return vec![(unit.name.clone(), syntax)],
        Reads::Dir(extensions) => extensions,
    };
    let mut found = Vec::new();
    let mut dirs = vec![unit.name.clone()];
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = fs::read_dir(root.join(&dir)) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            let path = format!("{dir}/{name}");
            if entry.path().is_dir() {
                if name != "target" && !name.starts_with('.') && !members.contains(&path) {
                    dirs.push(path);
                }
            } else if let Some(&(_, syntax)) = name
                .rsplit_once('.')
                .and_then(|(_, ext)| extensions.iter().find(|(known, _)| *known == ext))
            {
                found.push((path, syntax));
            }
        }
    }
    found.sort();
    found
}

/// A bare issue or pull request number in a comment.
#[derive(Debug, PartialEq, Eq)]
struct Hit {
    path: String,
    line: usize,
    /// The number as written: the `#` and its digits.
    number: String,
    /// The comment, trimmed.
    text: String,
}

/// The hits in `text`, the file at `path`, one per number.
fn hits_in(path: &str, text: &str, syntax: Syntax) -> Vec<Hit> {
    let mut hits = Vec::new();
    for (line, comment) in comments(text, syntax) {
        for number in bare_refs(&comment) {
            hits.push(Hit {
                path: path.to_owned(),
                line,
                number: number.to_owned(),
                text: comment.trim().to_owned(),
            });
        }
    }
    hits
}

/// Every hit in `unit`'s files under `root`, or why the unit cannot be
/// read.
fn scan(root: &Path, unit: &Unit, members: &[String]) -> Result<Vec<Hit>, String> {
    let files = files(root, unit, members);
    if files.is_empty() {
        return Err(format!(
            "{}: no file to read (under Bazel, is {} in the unit test's data in \
             tools/rusty-photon-ci/BUILD.bazel?)",
            unit.name, unit.label
        ));
    }
    if unit.package && !files.iter().any(|(path, _)| path.ends_with("/BUILD.bazel")) {
        return Err(format!(
            "{}: its BUILD.bazel is not among the files read (under Bazel, does {} glob \
             `**/*.bazel`?)",
            unit.name, unit.label
        ));
    }
    let mut hits = Vec::new();
    for (path, syntax) in files {
        let text = fs::read_to_string(root.join(&path)).map_err(|error| {
            format!(
                "{path}: {error} (under Bazel, is {} in the unit test's data?)",
                unit.label
            )
        })?;
        hits.extend(hits_in(&path, &text, syntax));
    }
    Ok(hits)
}

/// Each comment in `text`, with the line it is on; a comment that spans
/// lines gives one entry per line.
fn comments(text: &str, syntax: Syntax) -> Vec<(usize, String)> {
    match syntax {
        Syntax::Rust | Syntax::Js => Lexer::new(text, syntax).comments(),
        Syntax::Hash => text
            .lines()
            .zip(1..)
            .filter_map(|(line, n)| hash_comment(line).map(|comment| (n, comment.to_owned())))
            .collect(),
        Syntax::Prose => text
            .lines()
            .zip(1..)
            .map(|(line, n)| (n, line.to_owned()))
            .collect(),
    }
}

/// The `#` comment of one `line`, if it has one.
///
/// It starts at a `#` outside quotes that starts the line or follows a
/// space, and runs to the end of the line. Inside double quotes a `\`
/// escapes the next character.
fn hash_comment(line: &str) -> Option<&str> {
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut previous: Option<char> = None;
    for (at, c) in line.char_indices() {
        match quote {
            Some(_) if escaped => escaped = false,
            Some('"') if c == '\\' => escaped = true,
            Some(open) if c == open => quote = None,
            None if matches!(c, '"' | '\'') => quote = Some(c),
            None if c == '#' && previous.is_none_or(char::is_whitespace) => return line.get(at..),
            _ => {}
        }
        previous = Some(c);
    }
    None
}

/// The bare issue or pull request numbers in `text`.
///
/// Each is a `#` followed by two to four digits, the first not 0, that no
/// letter, digit or `_` continues, that no `-` and a letter continue (a
/// heading's anchor, `testing.md#68-do-not-assert`), and that no `&`
/// precedes (an entity).
fn bare_refs(text: &str) -> Vec<&str> {
    text.match_indices('#')
        .filter_map(|(at, _)| {
            let before = text.get(..at).and_then(|head| head.chars().last());
            let rest = text.get(at.saturating_add(1)..)?;
            let digits = rest.chars().take_while(char::is_ascii_digit).count();
            let number = (2..=4).contains(&digits) && !rest.starts_with('0');
            let mut after = rest.chars().skip(digits);
            let (next, then) = (after.next(), after.next());
            let ends = next.is_none_or(|c| !is_ident(c));
            let anchor = next == Some('-') && then.is_some_and(char::is_alphanumeric);
            if before == Some('&') || !number || !ends || anchor {
                return None;
            }
            text.get(at..at.saturating_add(1).saturating_add(digits))
        })
        .collect()
}

/// Whether `c` can be part of an identifier or a number.
fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The JavaScript keywords a regex literal may directly follow; after any
/// other word a `/` divides.
const BEFORE_REGEX: &[&str] = &[
    "await",
    "case",
    "delete",
    "do",
    "else",
    "in",
    "instanceof",
    "new",
    "of",
    "return",
    "throw",
    "typeof",
    "void",
    "yield",
];

/// A reader of Rust or JavaScript that knows where its comments are.
struct Lexer {
    chars: Vec<char>,
    at: usize,
    line: usize,
    syntax: Syntax,
    found: Vec<(usize, String)>,
}

impl Lexer {
    fn new(text: &str, syntax: Syntax) -> Self {
        Self {
            chars: text.chars().collect(),
            at: 0,
            line: 1,
            syntax,
            found: Vec::new(),
        }
    }

    /// The character `ahead` of the cursor.
    fn peek(&self, ahead: usize) -> Option<char> {
        self.chars.get(self.at.saturating_add(ahead)).copied()
    }

    /// The character `behind` the cursor.
    fn behind(&self, behind: usize) -> Option<char> {
        self.at
            .checked_sub(behind)
            .and_then(|at| self.chars.get(at))
            .copied()
    }

    /// Moves past one character, counting lines, and returns it.
    fn bump(&mut self) -> Option<char> {
        let c = self.peek(0)?;
        self.at = self.at.saturating_add(1);
        if c == '\n' {
            self.line = self.line.saturating_add(1);
        }
        Some(c)
    }

    /// Every comment, with its line.
    fn comments(mut self) -> Vec<(usize, String)> {
        while self.peek(0).is_some() {
            self.step();
        }
        self.found
    }

    /// Reads one token at the cursor: a comment, a literal, or one
    /// character of anything else.
    fn step(&mut self) {
        if let Some(hashes) = self.raw_string() {
            self.skip_raw_string(hashes);
            return;
        }
        if self.regex_opens() {
            self.regex();
            return;
        }
        match (self.peek(0), self.peek(1), self.syntax) {
            (Some('/'), Some('/'), _) => self.line_comment(),
            (Some('/'), Some('*'), _) => self.block_comment(),
            (Some('`'), _, Syntax::Js) => self.template(),
            (Some(quote @ '"'), _, _) | (Some(quote @ '\''), _, Syntax::Js) => {
                self.string(quote);
            }
            (Some('\''), _, _) => self.char_or_lifetime(),
            _ => {
                self.bump();
            }
        }
    }

    /// Moves past the string literal `quote` opens at the cursor, in which
    /// `\` escapes the next character.
    fn string(&mut self, quote: char) {
        self.bump();
        while let Some(c) = self.bump() {
            if c == '\\' {
                self.bump();
            } else if c == quote {
                return;
            }
        }
    }

    /// Moves past the template literal at the cursor, reading the code of
    /// each `${ }` as code, so a quote or a backtick inside one is not the
    /// template's end.
    fn template(&mut self) {
        self.bump();
        while let Some(c) = self.bump() {
            match (c, self.peek(0)) {
                ('\\', _) => {
                    self.bump();
                }
                ('`', _) => return,
                ('$', Some('{')) => {
                    self.bump();
                    self.interpolation();
                }
                _ => {}
            }
        }
    }

    /// Moves past the code of a `${ }` whose `{` the cursor has passed, to
    /// the `}` that closes it.
    fn interpolation(&mut self) {
        let mut depth = 1_usize;
        while let Some(c) = self.peek(0) {
            match c {
                '{' => {
                    depth = depth.saturating_add(1);
                    self.bump();
                }
                '}' => {
                    depth = depth.saturating_sub(1);
                    self.bump();
                    if depth == 0 {
                        return;
                    }
                }
                _ => self.step(),
            }
        }
    }

    /// Whether a JavaScript regex literal opens at the cursor: a `/` that
    /// starts no comment and follows no operand — nothing, a punctuator
    /// other than `)` or `]`, or one of [`BEFORE_REGEX`]. After an
    /// operand, a `/` divides.
    fn regex_opens(&self) -> bool {
        if self.syntax != Syntax::Js
            || self.peek(0) != Some('/')
            || matches!(self.peek(1), Some('/' | '*'))
        {
            return false;
        }
        let mut back = 1;
        while self.behind(back).is_some_and(char::is_whitespace) {
            back = back.saturating_add(1);
        }
        let Some(before) = self.behind(back) else {
            return true;
        };
        if !is_ident(before) {
            return !matches!(before, ')' | ']');
        }
        let mut word = String::new();
        while let Some(c) = self.behind(back).filter(|&c| is_ident(c)) {
            word.insert(0, c);
            back = back.saturating_add(1);
        }
        BEFORE_REGEX.contains(&word.as_str())
    }

    /// Moves past the regex literal at the cursor: its body, in which `\`
    /// escapes the next character and a `[` class runs to its `]`, and its
    /// flags. A line break ends it, since a regex has none.
    fn regex(&mut self) {
        self.bump();
        let mut class = false;
        while let Some(c) = self.bump() {
            match c {
                '\\' => {
                    self.bump();
                }
                '[' => class = true,
                ']' => class = false,
                '/' if !class => break,
                '\n' => return,
                _ => {}
            }
        }
        while self.peek(0).is_some_and(|c| c.is_ascii_alphabetic()) {
            self.bump();
        }
    }

    /// The `#`s of the raw string literal that opens at the cursor, if one
    /// does.
    ///
    /// `r"`, `r#"`, `br"`, `cr#"`: an `r` that no identifier continues, or
    /// that `b` or `c` prefixes, then the `#`s, then `"`.
    fn raw_string(&self) -> Option<usize> {
        if self.syntax != Syntax::Rust || self.peek(0) != Some('r') {
            return None;
        }
        let opens = match self.behind(1) {
            None => true,
            Some('b' | 'c') => !self.behind(2).is_some_and(is_ident),
            Some(c) => !is_ident(c),
        };
        if !opens {
            return None;
        }
        // A raw string has at most 255 `#`s.
        let hashes = (1..=255)
            .take_while(|&ahead| self.peek(ahead) == Some('#'))
            .count();
        (self.peek(hashes.saturating_add(1)) == Some('"')).then_some(hashes)
    }

    /// Moves past the raw string with `hashes` `#`s whose `r` is at the
    /// cursor.
    fn skip_raw_string(&mut self, hashes: usize) {
        for _ in 0..hashes.saturating_add(2) {
            self.bump();
        }
        while let Some(c) = self.bump() {
            if c == '"' && (0..hashes).all(|ahead| self.peek(ahead) == Some('#')) {
                for _ in 0..hashes {
                    self.bump();
                }
                return;
            }
        }
    }

    /// Moves past the char literal at the cursor — `'a'`, `'\''`,
    /// `'\u{1F600}'` — or past the `'` of a lifetime or label.
    fn char_or_lifetime(&mut self) {
        self.bump();
        match (self.peek(0), self.peek(1)) {
            (Some('\\'), _) => {
                while let Some(c) = self.bump() {
                    if c == '\'' {
                        return;
                    }
                }
            }
            (Some(_), Some('\'')) => {
                self.bump();
                self.bump();
            }
            _ => {}
        }
    }

    /// Takes the `//` comment at the cursor, to the end of its line.
    fn line_comment(&mut self) {
        let line = self.line;
        let mut text = String::new();
        while let Some(c) = self.peek(0) {
            if c == '\n' {
                break;
            }
            text.push(c);
            self.bump();
        }
        self.found.push((line, text));
    }

    /// Takes the `/* */` comment at the cursor, nested in Rust and flat in
    /// JavaScript, one entry per line it spans.
    fn block_comment(&mut self) {
        let start = self.line;
        let mut text = String::new();
        let mut depth = 0_usize;
        loop {
            match (self.peek(0), self.peek(1)) {
                (None, _) => break,
                (Some('/'), Some('*')) if depth == 0 || self.syntax == Syntax::Rust => {
                    depth = depth.saturating_add(1);
                    text.push_str("/*");
                    self.bump();
                    self.bump();
                }
                (Some('*'), Some('/')) => {
                    depth = depth.saturating_sub(1);
                    text.push_str("*/");
                    self.bump();
                    self.bump();
                    if depth == 0 {
                        break;
                    }
                }
                (Some(c), _) => {
                    text.push(c);
                    self.bump();
                }
            }
        }
        for (piece, offset) in text.lines().zip(0..) {
            self.found
                .push((start.saturating_add(offset), piece.to_owned()));
        }
    }
}

/// What `hits` say against `pending`: a problem naming the numbers no row
/// holds as often, which are new, and one naming the rows the files no
/// longer bear out, which the cleanup reached.
fn verdict(hits: &[Hit], pending: &[(&str, &str, usize)]) -> Vec<String> {
    let mut problems = Vec::new();
    let mut listed: BTreeMap<(&str, &str), usize> = BTreeMap::new();
    for &(path, number, count) in pending {
        if listed.insert((path, number), count).is_some() {
            problems.push(format!(
                "PENDING in {HERE} holds the row for {path} {number} twice."
            ));
        }
    }
    let mut found: BTreeMap<(&str, &str), Vec<&Hit>> = BTreeMap::new();
    for hit in hits {
        found
            .entry((hit.path.as_str(), hit.number.as_str()))
            .or_default()
            .push(hit);
    }
    let mut new = Vec::new();
    let mut reached = Vec::new();
    for (&(path, number), hits) in &found {
        let count = listed.remove(&(path, number)).unwrap_or(0);
        match hits.len().cmp(&count) {
            Ordering::Greater => {
                let held = if count > 0 {
                    format!(" (the row holds {count})")
                } else {
                    String::new()
                };
                new.extend(
                    hits.iter()
                        .map(|hit| format!("  {}:{}: {}{held}", hit.path, hit.line, hit.text)),
                );
            }
            Ordering::Less => reached.push(row(path, number, count, hits.len())),
            Ordering::Equal => {}
        }
    }
    for ((path, number), count) in listed {
        reached.push(row(path, number, count, 0));
    }
    if !new.is_empty() {
        problems.push(format!(
            "Comment lines name an issue or pull request by bare number beyond what PENDING in \
             {HERE} holds. A comment states what is true, in the present tense, and cites a \
             record only as a full URL or a docs/ path (AGENTS.md rule 16, docs/workspace.md \
             § Comments): reword them, never add a row.\n{}",
            new.join("\n")
        ));
    }
    if !reached.is_empty() {
        problems.push(format!(
            "The cleanup reached these rows of PENDING in {HERE}: delete each, or lower its \
             count to what the file names.\n{}",
            reached.join("\n")
        ));
    }
    problems
}

/// A row of [`PENDING`] as written, with what the file names now.
fn row(path: &str, number: &str, listed: usize, found: usize) -> String {
    format!("  (\"{path}\", \"{number}\", {listed}),  now {found}")
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

#[test]
fn comments_name_no_issue_or_pull_request_by_bare_number() {
    let root = repo_root();
    let members = members(&read(&root.join("Cargo.toml"))).unwrap_or_else(|e| panic!("{e}"));
    let mut problems = Vec::new();
    let mut hits = Vec::new();
    for unit in units(&members) {
        match scan(&root, &unit, &members) {
            Ok(found) => hits.extend(found),
            Err(problem) => problems.push(problem),
        }
    }
    problems.extend(verdict(&hits, PENDING));
    assert!(problems.is_empty(), "{}", problems.join("\n\n"));
}

/// `found`, with its text borrowed, for comparing.
fn pairs(found: &[(usize, String)]) -> Vec<(usize, &str)> {
    found
        .iter()
        .map(|(line, text)| (*line, text.as_str()))
        .collect()
}

#[test]
fn rust_comments_are_found_outside_literals() {
    let text = "//! inner\nfn f() {\n    let s = \"not // one\"; // trailing\n    let c = '#'; \
                let q = '\\''; /* block\n    spans */ let l: &'a str = s;\n    /// doc\n}\n";
    assert_eq!(
        pairs(&comments(text, Syntax::Rust)),
        [
            (1, "//! inner"),
            (3, "// trailing"),
            (4, "/* block"),
            (5, "    spans */"),
            (6, "/// doc"),
        ]
    );
}

#[test]
fn a_raw_string_hides_its_slashes_quotes_and_hashes() {
    let text = "let r = r#\"// \" not #\"# ; // yes\nlet b = br\"//\"; // too\nlet id = r#type; \
                // id\nlet s = \"it's // here\"; // last\n";
    assert_eq!(
        pairs(&comments(text, Syntax::Rust)),
        [(1, "// yes"), (2, "// too"), (3, "// id"), (4, "// last")]
    );
}

#[test]
fn a_nested_block_comment_closes_with_its_outer_one_in_rust() {
    let text = "/* a /* b */ c */ // d\n";
    assert_eq!(
        pairs(&comments(text, Syntax::Rust)),
        [(1, "/* a /* b */ c */"), (1, "// d")]
    );
}

#[test]
fn js_comments_are_found_outside_strings_and_templates() {
    let text = "const a = 'x // y'; // one\nconst b = `//\n${c}`; /* two\n three */ // four\n\
                /* a /* b */ // five\n";
    assert_eq!(
        pairs(&comments(text, Syntax::Js)),
        [
            (1, "// one"),
            (3, "/* two"),
            (4, " three */"),
            (4, "// four"),
            (5, "/* a /* b */"),
            (5, "// five"),
        ]
    );
}

#[test]
fn a_js_regex_literal_hides_its_quotes_and_a_division_is_none() {
    let text = "const re = /^\"[^\"\\\\]*\"$/; // one\nconst d = (a + b) / count / 2; // two\n\
                return /['\\r\\n]/gi.test(s); // three\nconst e = x.replace(/\\/\\//, '\"'); \
                // four\n";
    assert_eq!(
        pairs(&comments(text, Syntax::Js)),
        [
            (1, "// one"),
            (2, "// two"),
            (3, "// three"),
            (4, "// four")
        ]
    );
}

#[test]
fn a_js_template_reads_the_code_inside_its_interpolations() {
    let text = "const t = `a ${ok ? `x` : 'y'} b`; // one\nconst u = `${f('`', {k: 1})}`; \
                // two\nconst v = `${a /* in */ + `${b}`}`; // three\n";
    assert_eq!(
        pairs(&comments(text, Syntax::Js)),
        [
            (1, "// one"),
            (2, "// two"),
            (3, "/* in */"),
            (3, "// three")
        ]
    );
}

#[test]
fn hash_comments_start_the_line_or_follow_a_space_outside_quotes() {
    let text = "# top\nkey = \"a # b\" # c\nx = 'q # r' #d\nurl = \"x\\\" # y\" # z\nplain#1\n  \
                # indented\n";
    assert_eq!(
        pairs(&comments(text, Syntax::Hash)),
        [
            (1, "# top"),
            (2, "# c"),
            (3, "#d"),
            (4, "# z"),
            (6, "# indented"),
        ]
    );
}

#[test]
fn prose_is_every_line() {
    assert_eq!(
        pairs(&comments("Feature: x\n\n  Scenario: y\n", Syntax::Prose)),
        [(1, "Feature: x"), (2, ""), (3, "  Scenario: y")]
    );
}

#[test]
fn a_bare_ref_is_a_hash_and_two_to_four_digits_not_led_by_zero() {
    assert_eq!(
        bare_refs("see #12, (#1688) and #9999."),
        ["#12", "#1688", "#9999"]
    );
    assert_eq!(
        bare_refs("#1 item, #12345 long, #0123 zero, #000 black, #12ab hex, #12_x ident"),
        Vec::<&str>::new()
    );
}

#[test]
fn an_entity_and_a_url_fragment_are_not_bare_refs() {
    assert_eq!(
        bare_refs("&#39; https://github.com/o/r/pull/1688#issuecomment-6 and #L123"),
        Vec::<&str>::new()
    );
}

#[test]
fn a_heading_anchor_is_not_a_bare_ref_but_a_range_is() {
    assert_eq!(
        bare_refs("[§6.8](../docs/skills/testing.md#68-do-not-assert)"),
        Vec::<&str>::new()
    );
    assert_eq!(bare_refs("(#1250-#1251)"), ["#1250", "#1251"]);
}

#[test]
fn members_reads_the_literal_paths() {
    let cargo = "[workspace]\nmembers = [\n  \"crates/a\",\n  # c\n  \"services/b\",\n]\n";
    assert_eq!(members(cargo).unwrap(), ["crates/a", "services/b"]);
}

#[test]
fn members_refuses_a_glob_and_a_manifest_without_members() {
    let error = members("[workspace]\nmembers = [\"crates/*\"]\n").unwrap_err();
    assert!(error.contains("not a literal path"), "{error}");
    let error = members("[package]\nname = \"x\"\n").unwrap_err();
    assert!(error.contains("no [workspace] members"), "{error}");
}

#[test]
fn files_of_a_member_skip_a_nested_member_and_target() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for path in [
        "crates/a/BUILD.bazel",
        "crates/a/Cargo.toml",
        "crates/a/src/lib.rs",
        "crates/a/tests/features/x.feature",
        "crates/a/target/debug/x.rs",
        "crates/a/sys/Cargo.toml",
        "crates/a/sys/src/lib.rs",
        "crates/a/README.md",
    ] {
        let file = root.join(path);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, "").unwrap();
    }
    let members = ["crates/a".to_owned(), "crates/a/sys".to_owned()];
    let unit = &units(&members)[0];
    assert_eq!(
        files(root, unit, &members),
        [
            ("crates/a/BUILD.bazel".to_owned(), Syntax::Hash),
            ("crates/a/Cargo.toml".to_owned(), Syntax::Hash),
            ("crates/a/src/lib.rs".to_owned(), Syntax::Rust),
            (
                "crates/a/tests/features/x.feature".to_owned(),
                Syntax::Prose
            ),
        ]
    );
}

#[test]
fn a_file_unit_is_its_one_file() {
    let members = [];
    let units = units(&members);
    let unit = units.iter().find(|unit| unit.name == ".bazelrc").unwrap();
    assert_eq!(
        files(Path::new("/nowhere"), unit, &members),
        [(".bazelrc".to_owned(), Syntax::Hash)]
    );
}

#[test]
fn scan_requires_a_members_build_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("crates/a/src")).unwrap();
    fs::write(root.join("crates/a/src/lib.rs"), "// see #11\n").unwrap();
    let members = ["crates/a".to_owned()];
    let unit = &units(&members)[0];
    let error = scan(root, unit, &members).unwrap_err();
    assert!(error.contains("its BUILD.bazel is not among"), "{error}");
    fs::write(root.join("crates/a/BUILD.bazel"), "# see #12\n").unwrap();
    let numbers: Vec<String> = scan(root, unit, &members)
        .unwrap()
        .into_iter()
        .map(|hit| hit.number)
        .collect();
    assert_eq!(numbers, ["#12", "#11"]);
}

#[test]
fn hits_name_the_line_the_number_and_the_trimmed_comment() {
    let text =
        "fn f() {}\n    // the probe rejects it (issue #254, see #255)\n    let x = \"#256\";\n";
    let hit = |number: &str| Hit {
        path: "src/lib.rs".to_owned(),
        line: 2,
        number: number.to_owned(),
        text: "// the probe rejects it (issue #254, see #255)".to_owned(),
    };
    assert_eq!(
        hits_in("src/lib.rs", text, Syntax::Rust),
        [hit("#254"), hit("#255")]
    );
}

/// A hit of `number` in `path`.
fn hit(path: &str, number: &str) -> Hit {
    Hit {
        path: path.to_owned(),
        line: 1,
        number: number.to_owned(),
        text: format!("// see {number}"),
    }
}

#[test]
fn verdict_accepts_rows_that_match_the_files() {
    let hits = [hit("a.rs", "#11"), hit("a.rs", "#11"), hit("b.rs", "#12")];
    assert_eq!(
        verdict(&hits, &[("a.rs", "#11", 2), ("b.rs", "#12", 1)]),
        Vec::<String>::new()
    );
    assert_eq!(verdict(&[], &[]), Vec::<String>::new());
}

#[test]
fn verdict_names_new_numbers_and_the_rows_the_cleanup_reached() {
    let hits = [
        hit("a.rs", "#11"),
        hit("a.rs", "#11"),
        hit("b.rs", "#12"),
        hit("c.rs", "#13"),
    ];
    let problems = verdict(
        &hits,
        &[
            ("a.rs", "#11", 1),
            ("b.rs", "#12", 2),
            ("d.rs", "#14", 1),
            ("d.rs", "#14", 1),
        ],
    );
    assert_eq!(problems.len(), 3, "{problems:#?}");
    assert!(
        problems[0].contains("holds the row for d.rs #14 twice"),
        "{problems:#?}"
    );
    assert!(
        problems[1].contains("  a.rs:1: // see #11 (the row holds 1)")
            && problems[1].ends_with("  c.rs:1: // see #13")
            && !problems[1].contains("b.rs"),
        "{problems:#?}"
    );
    assert!(
        problems[2].contains("  (\"b.rs\", \"#12\", 2),  now 1")
            && problems[2].ends_with("  (\"d.rs\", \"#14\", 1),  now 0"),
        "{problems:#?}"
    );
}
