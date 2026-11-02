//! Comments name no issue or pull request by bare number.
//!
//! A comment is a living document: it says what the code does and why it is
//! not otherwise, in the present tense, and cites a record only as a full
//! URL or a `docs/` path (AGENTS.md rule 16, docs/workspace.md § Comments).
//! A `#` followed by a number does neither: every reader of the line pays to
//! fetch what it withholds. This test fails `bazel test //...` on one in
//!
//! - every workspace member: the comments of its Rust and TOML files, and
//!   the whole of its feature files, which are prose end to end;
//! - the repository's own files: the comments of the root `Cargo.toml`,
//!   `MODULE.bazel` and `.bazelrc`, of the workflows, and of the local
//!   actions' JavaScript and `action.yml`.
//!
//! A number is two to four digits, the first not 0: every number the
//! tracker has given past its first nine, and no list item (`#1`) or hex
//! colour (`#000`). A `&#39;` is an entity and `testing.md#68-do-not-assert`
//! a heading's anchor. A full URL has no `#` before a number, so a citation
//! passes as it is.
//!
//! [`PENDING`] lists the units the cleanup has not reached, each with the
//! count of comment lines it holds today. The count must match exactly: a
//! lower one is the cleanup's progress, recorded by lowering the entry and
//! removing it at zero; a higher one is a new breach, fixed in the comment
//! and never in the count. So a clean unit stays clean, and a pending one
//! grows no worse.
//!
//! What is a comment is decided by syntax: a Rust lexer that knows strings,
//! raw strings, char literals and lifetimes, so a `//` inside a string is
//! none; the same for JavaScript with its template literals; and for TOML,
//! Starlark, `.bazelrc` and YAML a `#` outside quotes that starts the line
//! or follows a space — YAML's own definition of a comment, which the other
//! three as written here meet. A YAML parser drops comments, so this is the
//! one guard that reads a workflow's lines (docs/skills/ci-logic.md).
//!
//! Under Bazel the test sees only its `data`: every member's `sources`
//! filegroup, which its BUILD file declares, and the root's files. A unit
//! whose files are missing fails the test rather than passing unread.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crate::repo_root;

/// The units the cleanup has not reached, in name order.
///
/// Each is a workspace member's path, or one of [`root_units`]' names, with
/// the comment lines in it that name an issue or pull request by bare
/// number today.
const PENDING: &[(&str, usize)] = &[
    (".bazelrc", 8),
    (".github/actions", 5),
    (".github/workflows", 11),
    ("Cargo.toml", 1),
    ("MODULE.bazel", 8),
    ("crates/bdd-infra", 18),
    ("crates/qhyccd-rs", 1),
    ("crates/rp-catalog", 3),
    ("crates/rp-focus-curve", 1),
    ("crates/rp-guider", 3),
    ("crates/rp-plate-solver", 3),
    ("crates/rusty-photon-driver", 1),
    ("crates/rusty-photon-service-lifecycle", 2),
    ("crates/rusty-photon-shared-transport", 15),
    ("crates/rusty-photon-tls", 4),
    ("crates/skywatcher-motor-protocol", 4),
    ("crates/svbony-rs/libsvbony-sys", 2),
    ("crates/zwo-rs", 6),
    ("services/calibrator-flats", 1),
    ("services/doctor", 35),
    ("services/dsd-fp2", 1),
    ("services/focus-model", 2),
    ("services/pa-falcon-rotator", 9),
    ("services/pa-scops-oag", 1),
    ("services/phd2-guider", 6),
    ("services/plate-solver", 6),
    ("services/ppba-driver", 1),
    ("services/qhy-camera", 2),
    ("services/qhy-focuser", 3),
    ("services/rp", 23),
    ("services/sentinel", 6),
    ("services/session-runner", 3),
    ("services/star-adventurer-gti", 60),
    ("services/svbony-camera", 1),
    ("services/ui-htmx", 12),
    ("services/zwo-camera", 1),
];

/// This file, for the messages.
const HERE: &str = "tools/rusty-photon-ci/src/bare_refs.rs";

/// How a file's comments are found.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Syntax {
    /// `//` and nested `/* */`, outside strings, raw strings and char
    /// literals.
    Rust,
    /// `//` and `/* */`, outside strings and template literals.
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
];

/// The workflows, as the root BUILD file's `workflows` filegroup globs them.
const WORKFLOW_FILES: &[(&str, Syntax)] = &[("yml", Syntax::Hash), ("yaml", Syntax::Hash)];

/// The local actions' files, as `.github/actions`' `sources` filegroup
/// globs them.
const ACTION_FILES: &[(&str, Syntax)] = &[
    ("mjs", Syntax::Js),
    ("yml", Syntax::Hash),
    ("yaml", Syntax::Hash),
];

/// Where the test reads: a workspace member's directory, or one of the
/// repository's own directories or files.
struct Unit {
    /// The name [`PENDING`] lists it by: the member's path, or the
    /// directory's or file's.
    name: String,
    /// The target that gives the unit's files to the test under Bazel,
    /// named when they are missing.
    label: String,
    reads: Reads,
}

/// The repository's own files and directories the test reads, with the
/// target that gives each to the test under Bazel.
fn root_units() -> Vec<Unit> {
    let file = |name: &str, label: &str| Unit {
        name: name.to_owned(),
        label: label.to_owned(),
        reads: Reads::File(Syntax::Hash),
    };
    let dir = |name: &str, label: &str, files: &'static [(&'static str, Syntax)]| Unit {
        name: name.to_owned(),
        label: label.to_owned(),
        reads: Reads::Dir(files),
    };
    vec![
        file("Cargo.toml", "//:Cargo.toml"),
        file("MODULE.bazel", "//:MODULE.bazel"),
        file(".bazelrc", "//:.bazelrc"),
        dir(".github/workflows", "//:workflows", WORKFLOW_FILES),
        dir(".github/actions", "//.github/actions:sources", ACTION_FILES),
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

/// A comment line that names an issue or pull request by bare number.
#[derive(Debug, PartialEq, Eq)]
struct Hit {
    path: String,
    line: usize,
    /// The comment, trimmed.
    text: String,
}

/// The hits in `text`, the file at `path`.
fn hits_in(path: &str, text: &str, syntax: Syntax) -> Vec<Hit> {
    comments(text, syntax)
        .into_iter()
        .filter(|(_, comment)| !bare_refs(comment).is_empty())
        .map(|(line, comment)| Hit {
            path: path.to_owned(),
            line,
            text: comment.trim().to_owned(),
        })
        .collect()
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
        match (self.peek(0), self.peek(1), self.syntax) {
            (Some('/'), Some('/'), _) => self.line_comment(),
            (Some('/'), Some('*'), _) => self.block_comment(),
            (Some(quote @ '"'), _, _) | (Some(quote @ ('\'' | '`')), _, Syntax::Js) => {
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

/// The hits of `scanned`, each unit's by name, against `pending`.
///
/// A problem for each unit that disagrees with its entry, or has hits and
/// none, and for each entry naming none of `known`.
fn verdict(
    known: &[String],
    scanned: &[(String, Vec<Hit>)],
    pending: &[(&str, usize)],
) -> Vec<String> {
    let mut problems = Vec::new();
    let mut listed: BTreeMap<&str, usize> = BTreeMap::new();
    for &(name, count) in pending {
        if listed.insert(name, count).is_some() {
            problems.push(format!("PENDING in {HERE} lists {name} twice."));
        }
    }
    for (name, hits) in scanned {
        let found = hits.len();
        match listed.get(name.as_str()) {
            None if found > 0 => problems.push(format!(
                "{name}: {found} comment line(s) name an issue or pull request by bare number. \
                 A comment states what is true, in the present tense, and cites a record only \
                 as a full URL or a docs/ path (AGENTS.md rule 16, docs/workspace.md \
                 § Comments):\n{}",
                list(hits)
            )),
            Some(&count) if found > count => problems.push(format!(
                "{name}: {found} comment lines name an issue or pull request by bare number, \
                 where PENDING in {HERE} lists {count}. The new ones are reworded (AGENTS.md \
                 rule 16, docs/workspace.md § Comments), never counted:\n{}",
                list(hits)
            )),
            Some(&count) if found < count => problems.push(format!(
                "{name}: {found} comment lines name an issue or pull request by bare number, \
                 where PENDING in {HERE} lists {count}: the cleanup reached it, so set the \
                 entry to {found}, or remove it at 0."
            )),
            _ => {}
        }
    }
    for name in listed.keys() {
        if !known.iter().any(|unit| unit == name) {
            problems.push(format!(
                "PENDING in {HERE} names {name}, which is neither a workspace member nor a file \
                 the test reads: remove the entry."
            ));
        }
    }
    problems
}

/// `hits`, one per line, for a message.
fn list(hits: &[Hit]) -> String {
    hits.iter()
        .map(|hit| format!("  {}:{}: {}", hit.path, hit.line, hit.text))
        .collect::<Vec<_>>()
        .join("\n")
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

#[test]
fn comments_name_no_issue_or_pull_request_by_bare_number() {
    let root = repo_root();
    let members = members(&read(&root.join("Cargo.toml"))).unwrap_or_else(|e| panic!("{e}"));
    let units = units(&members);
    let known: Vec<String> = units.iter().map(|unit| unit.name.clone()).collect();
    let mut problems = Vec::new();
    let mut scanned = Vec::new();
    for unit in &units {
        match scan(&root, unit, &members) {
            Ok(hits) => scanned.push((unit.name.clone(), hits)),
            Err(problem) => problems.push(problem),
        }
    }
    problems.extend(verdict(&known, &scanned, PENDING));
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
    let unit = &units(&members)[2];
    assert_eq!(unit.name, ".bazelrc");
    assert_eq!(
        files(Path::new("/nowhere"), unit, &members),
        [(".bazelrc".to_owned(), Syntax::Hash)]
    );
}

#[test]
fn hits_name_the_line_and_the_trimmed_comment() {
    let text = "fn f() {}\n    // the probe rejects it (issue #254)\n    let x = \"#255\";\n";
    assert_eq!(
        hits_in("src/lib.rs", text, Syntax::Rust),
        [Hit {
            path: "src/lib.rs".to_owned(),
            line: 2,
            text: "// the probe rejects it (issue #254)".to_owned(),
        }]
    );
}

/// A hit in `path`.
fn hit(path: &str) -> Hit {
    Hit {
        path: path.to_owned(),
        line: 1,
        text: "// #11".to_owned(),
    }
}

#[test]
fn verdict_accepts_clean_units_and_exact_pending_counts() {
    let known = ["a".to_owned(), "b".to_owned()];
    let scanned = [
        ("a".to_owned(), vec![]),
        ("b".to_owned(), vec![hit("b/x.rs"), hit("b/y.rs")]),
    ];
    assert_eq!(verdict(&known, &scanned, &[("b", 2)]), Vec::<String>::new());
}

#[test]
fn verdict_names_each_disagreement() {
    let known = ["a".to_owned(), "b".to_owned(), "c".to_owned()];
    let scanned = [
        ("a".to_owned(), vec![hit("a/x.rs")]),
        ("b".to_owned(), vec![hit("b/x.rs"), hit("b/y.rs")]),
        ("c".to_owned(), vec![]),
    ];
    let problems = verdict(&known, &scanned, &[("b", 1), ("c", 1), ("d", 1), ("d", 2)]);
    assert_eq!(problems.len(), 5, "{problems:#?}");
    assert!(problems[0].contains("lists d twice"), "{problems:#?}");
    assert!(
        problems[1].starts_with("a: 1 comment line(s)")
            && problems[1].ends_with("  a/x.rs:1: // #11"),
        "{problems:#?}"
    );
    assert!(
        problems[2].starts_with("b: 2 comment lines")
            && problems[2].contains("lists 1. The new ones"),
        "{problems:#?}"
    );
    assert!(
        problems[3].starts_with("c: 0 comment lines") && problems[3].contains("set the entry to 0"),
        "{problems:#?}"
    );
    assert!(
        problems[4].contains("names d, which is neither"),
        "{problems:#?}"
    );
}
