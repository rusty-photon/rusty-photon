//! Self-hosted runners stay behind reviewed routes.
//!
//! Four workflows send a job to the self-hosted pool with a copy of one
//! `runs-on` expression. For each OS it holds the kill-switch variable, the
//! trusted-event test and the pool's labels, and every falsy branch falls
//! back to a hosted runner. ADR-020 makes the expression a security
//! boundary. It is copied because `runs-on` is evaluated before the job
//! exists, so neither `env` nor a job output can hold it. Two more workflows
//! name self-hosted runners by label, and only a schedule or a maintainer's
//! dispatch starts them.
//!
//! These tests hold every workflow to what is reviewed here:
//! - each copy of the pool's expression to the reviewed clauses and
//!   fallback. A pull request that changes one copy fails
//!   `bazel test //...`, and one that changes them all changes this file
//!   too, in the diff its reviewer reads;
//! - each label list to the reviewed one, in a workflow that nothing else
//!   starts;
//! - the pool's labels to those `runs-on` values, so that no matrix or other
//!   value carries one into a job's runner.
//!
//! A job whose `runs-on` names a self-hosted runner and is not listed in
//! [`RUNNERS`] fails them.
//!
//! They read the workflows and `.github/actionlint.yaml` with a YAML parser,
//! and refuse a file they would read otherwise than GitHub ([`load`]). They
//! catch a copy that drifts and a new route, in review. A route disguised
//! on purpose gets past them, such as a label built at runtime; the plan's
//! R6 section lists those. Against a fork's pull request, the defence
//! remains the approval its workflows wait for (ADR-020 layer 2).

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use saphyr::{LoadableYamlNode, MarkedYaml, Scalar, ScanError, YamlData};
use saphyr_parser::{Event, Parser};

/// The Linux pool, as reviewed: its kill switch, the trusted-event test (a
/// push, or a pull request from this repository) and its labels.
const LINUX: &str = r#"
    vars.RP_POOL_LINUX == 'on'
    && (github.event_name == 'push'
        || (github.event_name == 'pull_request'
            && github.event.pull_request.head.repo.full_name == github.repository))
    && fromJSON('["self-hosted", "proxmox-ephemeral"]')
"#;

/// The Windows pool, as reviewed: the same test, under its own kill switch.
const WINDOWS: &str = r#"
    vars.RP_POOL_WINDOWS == 'on'
    && (github.event_name == 'push'
        || (github.event_name == 'pull_request'
            && github.event.pull_request.head.repo.full_name == github.repository))
    && fromJSON('["self-hosted", "proxmox-ephemeral-windows"]')
"#;

/// A copy of the pool's routing expression, as reviewed.
struct Route {
    /// Each pool branch in order: the `matrix.os` label it sits behind, when
    /// the job has a matrix, and its clause.
    branches: &'static [(Option<&'static str>, &'static str)],
    /// The hosted runner every falsy branch lands on.
    fallback: &'static str,
}

/// A job without a matrix, on the Linux pool.
const LINUX_JOB: Route = Route {
    branches: &[(None, LINUX)],
    fallback: "'ubuntu-latest'",
};

/// A `runs-on` that names a self-hosted runner, as reviewed.
enum Runner {
    /// A copy of the pool's routing expression.
    Pool(Route),
    /// A label list, in a workflow that only [`MAINTAINER_EVENTS`] start.
    Labels(&'static [&'static str]),
}

/// Every job whose `runs-on` names a self-hosted runner, by workflow and
/// job id, in workflow-name order.
const RUNNERS: &[(&str, &str, Runner)] = &[
    ("bazel-coverage.yml", "coverage", Runner::Pool(LINUX_JOB)),
    (
        "bazel.yml",
        "build-test",
        Runner::Pool(Route {
            branches: &[
                (Some("ubuntu-latest"), LINUX),
                (Some("windows-latest"), WINDOWS),
            ],
            // The matrix's own label, a hosted one: no pool label may appear
            // outside a listed `runs-on`.
            fallback: "matrix.os",
        }),
    ),
    ("check.yml", "clippy", Runner::Pool(LINUX_JOB)),
    (
        "pi-nightly.yml",
        "arm64-stable",
        Runner::Labels(&["self-hosted", "Linux", "ARM64", "raspberry-pi"]),
    ),
    (
        "proxmox-runner-test.yml",
        "build-test",
        Runner::Labels(&["self-hosted", "Linux", "X64", "proxmox-ephemeral"]),
    ),
    (
        "proxmox-runner-test.yml",
        "build-test-windows",
        Runner::Labels(&["self-hosted", "Windows", "X64", "proxmox-ephemeral-windows"]),
    ),
    ("repin-bazel.yml", "repin", Runner::Pool(LINUX_JOB)),
];

/// The events that may start a workflow naming self-hosted runners by
/// label. A schedule runs the default branch's copy of the workflow; a
/// dispatch runs the copy on the ref its dispatcher picks, and dispatching
/// needs write access to the repository. No pull request starts either.
const MAINTAINER_EVENTS: [&str; 2] = ["schedule", "workflow_dispatch"];

/// The label every self-hosted runner carries. Our runners' own labels are
/// read from `.github/actionlint.yaml`, the list actionlint checks `runs-on`
/// against, so one list serves both.
const SELF_HOSTED: &str = "self-hosted";

/// The labels that name a self-hosted runner: [`SELF_HOSTED`] and our
/// runners' own.
struct Labels(Vec<String>);

impl Labels {
    /// The labels, lowercased: `has_word` compares lowercased text.
    fn new(ours: impl IntoIterator<Item = String>) -> Self {
        Self(
            std::iter::once(SELF_HOSTED.to_owned())
                .chain(ours)
                .map(|label| label.to_lowercase())
                .collect(),
        )
    }

    /// Whether `text` names one of the labels, in any case, as GitHub
    /// matches them, and as a whole label rather than part of a longer word
    /// such as a file name.
    fn named_in(&self, text: &str) -> bool {
        self.0.iter().any(|label| has_word(text, label))
    }
}

/// The end of every refusal.
const MOVED: &str = "A divergence here is a security boundary moving (ADR-020). \
     If the change is meant, make it in every copy and in \
     tools/rusty-photon-ci/src/pool_routing.rs.";

/// A workflow file's name and text.
struct Workflow {
    name: String,
    text: String,
}

/// A node of a YAML file, with its place in the file. The parser copies
/// every string out of the file's text, so a node outlives the text.
type Node = MarkedYaml<'static>;

/// A job's `runs-on`, as the tests read it.
#[derive(Debug, PartialEq, Eq)]
enum RunsOn {
    /// A string: an expression, or one label.
    One(String),
    /// A list of strings: labels.
    List(Vec<String>),
    /// Anything else, such as a runner group, in YAML's flow style.
    Other(String),
}

impl RunsOn {
    /// `value`, read as a `runs-on`.
    fn new(value: &Node) -> Self {
        if let Some(text) = string(value) {
            return Self::One(text.to_owned());
        }
        let list = match &value.data {
            YamlData::Sequence(items) => items
                .iter()
                .map(|item| string(item).map(str::to_owned))
                .collect(),
            _ => None,
        };
        list.map_or_else(|| Self::Other(render(value)), Self::List)
    }
}

impl fmt::Display for RunsOn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::One(text) | Self::Other(text) => f.write_str(text),
            Self::List(labels) => write!(f, "[{}]", labels.join(", ")),
        }
    }
}

/// What the tests read in one workflow.
#[derive(Debug, Default, PartialEq, Eq)]
struct Read {
    /// The events its `on` names, or `None` if the tests do not read them.
    triggers: Option<Vec<String>>,
    /// Each job whose `runs-on` names a self-hosted runner: the job's id,
    /// the line of its `runs-on` and the value.
    runners: Vec<(String, usize, RunsOn)>,
    /// Each other string, key or value, that names one of the labels: the
    /// line of the file that names one, and the string's line of text that
    /// does. When only an escape spells the label, the line is the one the
    /// string starts on.
    stray: Vec<(usize, String)>,
}

/// The repository's root, two levels above the crate. Cargo sets
/// `CARGO_MANIFEST_DIR` when it runs a test. For Bazel the BUILD file sets
/// it, relative to the runfiles root the test starts in, which holds the
/// workflows and `.github/actionlint.yaml` as the test's data.
fn repo_root() -> PathBuf {
    let crate_dir = std::env::var_os("CARGO_MANIFEST_DIR")
        .expect("CARGO_MANIFEST_DIR is set by Cargo, and for Bazel by the BUILD file");
    Path::new(&crate_dir)
        .ancestors()
        .nth(2)
        .expect("the crate sits two levels below the repository root")
        .to_path_buf()
}

/// The one document in a YAML file's `text`, as GitHub reads a workflow:
/// YAML 1.2's core schema types each plain scalar, so `on` is a string, and
/// an alias reads as a copy of its anchor's node. The tests read a file as
/// GitHub does or not at all, so what they would read otherwise fails,
/// naming its line:
/// - a tag of any kind, which no workflow needs. The parser drops some, such
///   as `!!str` on a list, so the tests could not say how they read it;
/// - a key that is not a string;
/// - two keys of a mapping that differ only in case, which GitHub refuses
///   (the parser refuses an exact pair itself);
/// - a `<<` key, which GitHub reads as a key and actionlint's parser as a
///   merge;
/// - no document, or more than one.
fn load(text: &str) -> Result<Node, String> {
    let unparsed = |e: ScanError| {
        format!(
            "{}: does not parse as YAML: {}",
            e.marker().line(),
            e.info()
        )
    };
    // The parser's events carry every tag; the node it builds keeps some.
    for event in Parser::new_from_str(text) {
        let (event, span) = event.map_err(unparsed)?;
        if let Event::Scalar(.., Some(tag))
        | Event::SequenceStart(_, Some(tag))
        | Event::MappingStart(_, Some(tag)) = event
        {
            // The parser stores a tag resolved, and its `Display` writes `!`
            // as `!!` and `!<tag:x>` as `!tag:x`; these are YAML's spellings.
            let tag = match (tag.handle.as_str(), tag.suffix.as_str()) {
                _ if tag.is_yaml_core_schema() => format!("!!{}", tag.suffix),
                ("!", suffix) => format!("!{suffix}"),
                ("", "!") => "!".to_owned(),
                (handle, suffix) => format!("!<{handle}{suffix}>"),
            };
            return Err(format!(
                "{}: has the tag `{tag}`, where the test reads none",
                span.start.line()
            ));
        }
    }
    let mut documents = Node::load_from_str(text).map_err(unparsed)?;
    if let Some(second) = documents.get(1) {
        return Err(at(
            second,
            "starts a second YAML document, where the test reads one",
        ));
    }
    let document = documents.pop().ok_or("1: holds no YAML document")?;
    refuse(&document)?;
    Ok(document)
}

/// Why the tests would read `node` otherwise than GitHub, at its line, if
/// they would: see [`load`].
fn refuse(node: &Node) -> Result<(), String> {
    match &node.data {
        YamlData::Value(_) => Ok(()),
        YamlData::Sequence(items) => items.iter().try_for_each(refuse),
        YamlData::Mapping(map) => {
            // Each key so far, uppercased as GitHub compares them.
            let mut keys: Vec<(String, &Node)> = Vec::new();
            for (key, value) in map {
                let Some(name) = string(key) else {
                    return Err(at(
                        key,
                        format_args!(
                            "has the key `{}`, where the test reads only a string",
                            render(key)
                        ),
                    ));
                };
                if name == "<<" {
                    return Err(at(
                        key,
                        "has a `<<` key, which GitHub reads as a key and actionlint as a merge",
                    ));
                }
                let upper = name.to_uppercase();
                if let Some((_, first)) = keys.iter().find(|(other, _)| *other == upper) {
                    return Err(at(
                        key,
                        format_args!(
                            "has the key `{name}`, which differs only in case from `{}` on line {}: \
                             GitHub refuses the pair",
                            render(first),
                            line(first)
                        ),
                    ));
                }
                keys.push((upper, key));
                refuse(value)?;
            }
            Ok(())
        }
        // Not reached: `load` refuses every tag first, and the parser reads
        // an alias as its anchor's node. Should that change, the test fails.
        YamlData::Tagged(..)
        | YamlData::Representation(..)
        | YamlData::Alias(_)
        | YamlData::BadValue => Err(at(node, "has a value the test does not read")),
    }
}

/// The line `node` starts on. A block scalar starts on its first line of
/// text, below its key.
fn line(node: &Node) -> usize {
    node.span.start.line()
}

/// `why`, at `node`'s line.
fn at(node: &Node, why: impl fmt::Display) -> String {
    format!("{}: {why}", line(node))
}

/// The string `node` holds, if it holds one.
fn string(node: &Node) -> Option<&str> {
    match &node.data {
        YamlData::Value(Scalar::String(text)) => Some(text.as_ref()),
        _ => None,
    }
}

/// The value of `key` in the mapping `node`, with the key, if it has the
/// key. A key matches in any case, so the tests read at least what GitHub
/// does.
fn get<'a>(node: &'a Node, key: &str) -> Option<(&'a Node, &'a Node)> {
    let YamlData::Mapping(map) = &node.data else {
        return None;
    };
    map.iter()
        .find(|(name, _)| string(name).is_some_and(|name| name.eq_ignore_ascii_case(key)))
}

/// `node` in YAML's flow style, for a message.
fn render(node: &Node) -> String {
    let join = |parts: Vec<String>| parts.join(", ");
    match &node.data {
        YamlData::Value(Scalar::String(text)) | YamlData::Representation(text, ..) => {
            text.to_string()
        }
        YamlData::Value(Scalar::Null) => "null".to_owned(),
        YamlData::Value(Scalar::Boolean(value)) => value.to_string(),
        YamlData::Value(Scalar::Integer(value)) => value.to_string(),
        YamlData::Value(Scalar::FloatingPoint(value)) => value.to_string(),
        YamlData::Sequence(items) => format!("[{}]", join(items.iter().map(render).collect())),
        YamlData::Mapping(map) => format!(
            "{{{}}}",
            join(
                map.iter()
                    .map(|(key, value)| format!("{}: {}", render(key), render(value)))
                    .collect()
            )
        ),
        YamlData::Tagged(tag, value) => format!("{tag} {}", render(value)),
        YamlData::Alias(_) | YamlData::BadValue => "?".to_owned(),
    }
}

/// Our runners' own labels, as `.github/actionlint.yaml` lists them under
/// `self-hosted-runner` and `labels`. Each must be a plain string (letters,
/// digits, `.`, `_`, `-`), the characters `has_word` reads as part of a
/// label. Nor may one be a label that YAML reads as other than a string
/// where a workflow writes it unquoted, in any case, as GitHub matches it:
/// `2024`, or `tRUE` written `true`. The tests read only a workflow's
/// strings, so they would not see it. Anything else fails, naming its line.
fn actionlint_labels(text: &str) -> Result<Vec<String>, String> {
    let document = load(text)?;
    let (key, runner) = get(&document, "self-hosted-runner")
        .ok_or_else(|| at(&document, "has no `self-hosted-runner` key"))?;
    let none = || at(key, "lists no labels under `self-hosted-runner`");
    let (_, list) = get(runner, "labels").ok_or_else(none)?;
    let items = match &list.data {
        YamlData::Sequence(items) => items.as_slice(),
        YamlData::Value(Scalar::Null) => &[],
        _ => {
            return Err(at(
                list,
                format_args!(
                    "has `labels: {}`, where the test reads a list",
                    render(list)
                ),
            ))
        }
    };
    if items.is_empty() {
        return Err(none());
    }
    items
        .iter()
        .map(|item| {
            let label = string(item)
                .filter(|label| is_plain_label(label))
                .ok_or_else(|| {
                    at(
                        item,
                        format_args!(
                            "lists `{}`, which the test does not read as a plain label \
                             (letters, digits, `.`, `_`, `-`)",
                            render(item)
                        ),
                    )
                })?;
            // YAML's spellings of a number, a boolean or null each have a
            // lowercase form, such as `true` for `TRUE` and `0x1f` for `0X1F`.
            let lower = label.to_lowercase();
            if !matches!(
                Scalar::parse_from_cow(lower.as_str().into()),
                Scalar::String(_)
            ) {
                return Err(at(
                    item,
                    format_args!(
                        "lists `{label}`, which YAML reads as other than a string where a \
                         workflow writes it unquoted as `{lower}`, so the test would not \
                         see it there"
                    ),
                ));
            }
            Ok(label.to_owned())
        })
        .collect()
}

/// Whether `label` is non-empty and only letters, digits, `.`, `_` and `-`.
fn is_plain_label(label: &str) -> bool {
    !label.is_empty()
        && label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// The workflows in `dir`, in name order.
fn read_workflows(dir: &Path) -> Vec<Workflow> {
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()))
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|ext| ext == "yml" || ext == "yaml")
        })
        .collect();
    paths.sort();
    paths
        .iter()
        .map(|path| Workflow {
            name: path.file_name().unwrap().to_string_lossy().into_owned(),
            text: fs::read_to_string(path)
                .unwrap_or_else(|e| panic!("reading {}: {e}", path.display())),
        })
        .collect()
}

/// Every way `workflows` differ from the `reviewed` runners, as messages.
fn review(
    workflows: &[Workflow],
    reviewed: &[(&str, &str, Runner)],
    labels: &Labels,
) -> Vec<String> {
    let mut problems = Vec::new();
    let mut found: Vec<(&str, String)> = Vec::new();
    // The workflows the test does not read: their listed runners are not
    // missing, only unread.
    let mut unread: Vec<&str> = Vec::new();
    for workflow in workflows {
        let name = workflow.name.as_str();
        let read = match read(&workflow.text, labels) {
            Ok(read) => read,
            Err(e) => {
                problems.push(format!("{name}:{e}"));
                unread.push(name);
                continue;
            }
        };
        for (line, text) in &read.stray {
            problems.push(format!(
                "{name}:{line}: `{text}` names a self-hosted label outside a job's \
                 `runs-on`.\n{MOVED}"
            ));
        }
        for (job, line, value) in &read.runners {
            found.push((name, job.clone()));
            let Some((_, _, runner)) = reviewed
                .iter()
                .find(|(file, id, _)| *file == name && id == job)
            else {
                problems.push(format!(
                    "{name}:{line}: job `{job}`'s `runs-on` names a self-hosted runner, \
                     and RUNNERS does not list it. A new one is reviewed against ADR-020 \
                     and listed there.\n{MOVED}"
                ));
                continue;
            };
            let checked = match runner {
                Runner::Pool(route) => check_route(value, route),
                Runner::Labels(list) => check_labels(value, list, read.triggers.as_deref()),
            };
            if let Err(e) = checked {
                problems.push(format!("{name}:{line}: {e}\n{MOVED}"));
            }
        }
    }
    for (file, job, _) in reviewed {
        if !unread.contains(file) && !found.iter().any(|(name, id)| name == file && id == job) {
            problems.push(format!(
                "RUNNERS lists job `{job}` in {file}, where the test found no `runs-on` \
                 that names a self-hosted runner.\n{MOVED}"
            ));
        }
    }
    problems
}

/// What the tests read in a workflow's `text`, or why they do not read it.
fn read(text: &str, labels: &Labels) -> Result<Read, String> {
    let document = load(text)?;
    let mut read = Read {
        triggers: get(&document, "on").and_then(|(_, on)| events(on)),
        ..Read::default()
    };
    walk(&document, &mut Vec::new(), text, labels, &mut read);
    Ok(read)
}

/// Reads `node`, a node of the file `text`, into `read`. `path` holds the
/// keys above it, `None` for a list's item, so that a job's `runs-on` is the
/// one under `jobs` and the job's id.
fn walk<'a>(
    node: &'a Node,
    path: &mut Vec<Option<&'a str>>,
    text: &str,
    labels: &Labels,
    read: &mut Read,
) {
    match &node.data {
        YamlData::Sequence(items) => {
            for item in items {
                path.push(None);
                walk(item, path, text, labels, read);
                path.pop();
            }
        }
        YamlData::Mapping(map) => {
            for (key, value) in map {
                walk(key, path, text, labels, read);
                let name = string(key);
                let job = job_of(path, name).filter(|_| names_self_hosted(value, labels));
                if let Some(job) = job {
                    read.runners
                        .push((job.to_owned(), line(key), RunsOn::new(value)));
                    continue;
                }
                path.push(name);
                walk(value, path, text, labels, read);
                path.pop();
            }
        }
        _ => {
            let named = string(node).and_then(|value| value.lines().find(|l| labels.named_in(l)));
            if let Some(named) = named {
                read.stray
                    .push((line_naming(node, text, labels), named.trim().to_owned()));
            }
        }
    }
}

/// The line of the file `text` where `node` names one of the `labels`, or
/// the line `node` starts on if no line of it does, as when an escape spells
/// the label.
fn line_naming(node: &Node, text: &str, labels: &Labels) -> usize {
    let (start, end) = (node.span.start, node.span.end);
    // The parser places a node in characters, not bytes.
    let source: String = text
        .chars()
        .skip(start.index())
        .take(end.index().saturating_sub(start.index()))
        .collect();
    let offset = source.lines().position(|l| labels.named_in(l));
    start.line().saturating_add(offset.unwrap_or_default())
}

/// The job's id, if `path` and `key` place a value at a job's `runs-on`:
/// under `jobs` and the job's id.
fn job_of<'a>(path: &[Option<&'a str>], key: Option<&str>) -> Option<&'a str> {
    match (path, key) {
        ([Some(jobs), Some(job)], Some(key))
            if jobs.eq_ignore_ascii_case("jobs") && key.eq_ignore_ascii_case("runs-on") =>
        {
            Some(*job)
        }
        _ => None,
    }
}

/// Whether a `runs-on` value names a self-hosted runner: a string in it,
/// key or value, mentions a pool's kill switch (`RP_POOL_`), names one of
/// the `labels`, or picks a runner group. This organization's only group is
/// the default one, which holds the pool's runners.
fn names_self_hosted(value: &Node, labels: &Labels) -> bool {
    strings(value).into_iter().any(|text| {
        text.to_lowercase().contains("rp_pool_") || labels.named_in(text) || has_word(text, "group")
    })
}

/// Every string in `node`, keys included.
fn strings(node: &Node) -> Vec<&str> {
    match &node.data {
        YamlData::Sequence(items) => items.iter().flat_map(strings).collect(),
        YamlData::Mapping(map) => map
            .iter()
            .flat_map(<[_; 2]>::from)
            .flat_map(strings)
            .collect(),
        _ => string(node).into_iter().collect(),
    }
}

/// The events a workflow's `on` names: one, a list of them, or a mapping's
/// keys. `None` for anything else.
fn events(on: &Node) -> Option<Vec<String>> {
    let names: Vec<&Node> = match &on.data {
        YamlData::Sequence(items) => items.iter().collect(),
        YamlData::Mapping(map) => map.keys().collect(),
        _ => vec![on],
    };
    names
        .into_iter()
        .map(|name| string(name).map(str::to_owned))
        .collect()
}

/// Whether `text` holds `word`, in any case, as a whole word: not part of a
/// longer label or name.
fn has_word(text: &str, word: &str) -> bool {
    let text = text.to_lowercase();
    let is_label_char = |c: char| c.is_alphanumeric() || matches!(c, '-' | '_' | '.');
    text.match_indices(word).any(|(at, _)| {
        let before = text.get(..at).and_then(|s| s.chars().next_back());
        let after = text
            .get(at.saturating_add(word.len())..)
            .and_then(|s| s.chars().next());
        !before.is_some_and(is_label_char) && !after.is_some_and(is_label_char)
    })
}

/// `expr` on one line: whitespace outside quoted strings collapsed to one
/// space, and none just inside a parenthesis, so a copy reads the same
/// however its lines break.
fn normalize(expr: &str) -> String {
    let mut out = String::new();
    let (mut quoted, mut space) = (false, false);
    for c in expr.chars() {
        if quoted {
            out.push(c);
            quoted = c != '\'';
        } else if c.is_whitespace() {
            space = true;
        } else {
            if space && !out.is_empty() && !out.ends_with('(') && c != ')' {
                out.push(' ');
            }
            space = false;
            out.push(c);
            quoted = c == '\'';
        }
    }
    out
}

/// The byte offsets in `expr` reached outside every parenthesis and quoted
/// string. A group's opening parenthesis is one; its closing one is not.
fn top_level(expr: &str) -> Vec<usize> {
    let (mut depth, mut quoted) = (0_u32, false);
    let mut offsets = Vec::new();
    for (at, c) in expr.char_indices() {
        if depth == 0 && !quoted {
            offsets.push(at);
        }
        match c {
            '\'' => quoted = !quoted,
            '(' if !quoted => depth = depth.saturating_add(1),
            ')' if !quoted => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    offsets
}

/// `expr` split at each `op` outside parentheses and quoted strings.
fn split_top_level<'a>(expr: &'a str, op: &str) -> Vec<&'a str> {
    let mut parts = Vec::new();
    let mut start = 0;
    for at in top_level(expr) {
        if at >= start && expr.get(at..).is_some_and(|rest| rest.starts_with(op)) {
            parts.extend(expr.get(start..at).map(str::trim));
            start = at.saturating_add(op.len());
        }
    }
    parts.extend(expr.get(start..).map(str::trim));
    parts
}

/// `term` without the parentheses around all of it, if it has them.
fn unwrap_parens(term: &str) -> &str {
    match (
        top_level(term).as_slice(),
        term.strip_prefix('(').and_then(|t| t.strip_suffix(')')),
    ) {
        ([0], Some(inner)) => inner.trim(),
        _ => term,
    }
}

/// A routing expression's pool branches, each without the parentheses
/// around it, and its fallback, the last `||` term.
fn parse(value: &str) -> Result<(Vec<String>, String), String> {
    let expr = value
        .strip_prefix("${{")
        .and_then(|v| v.strip_suffix("}}"))
        .ok_or_else(|| format!("`runs-on` is not one ${{{{ }}}} expression: {value}"))?;
    let expr = normalize(expr);
    let mut terms: Vec<String> = split_top_level(&expr, "||")
        .into_iter()
        .map(|term| unwrap_parens(term).to_owned())
        .collect();
    let fallback = terms.pop().unwrap_or_default();
    Ok((terms, fallback))
}

/// Why the `runs-on` `value` is not the reviewed `route`, if it is not.
fn check_route(value: &RunsOn, route: &Route) -> Result<(), String> {
    let RunsOn::One(text) = value else {
        return Err(format!(
            "`runs-on` is not one ${{{{ }}}} expression: {value}"
        ));
    };
    let (branches, fallback) = parse(text)?;
    let reviewed: Vec<String> = route
        .branches
        .iter()
        .map(|(label, clause)| {
            let matrix = label.map_or_default(|label| format!("matrix.os == '{label}' && "));
            format!("{matrix}{}", normalize(clause))
        })
        .collect();
    if branches != reviewed {
        return Err(format!(
            "its pool branches are not the reviewed ones.\n  found:\n    {}\n  \
             reviewed:\n    {}",
            branches.join("\n    "),
            reviewed.join("\n    "),
        ));
    }
    if fallback != route.fallback {
        return Err(format!(
            "it falls back to {fallback}, where the reviewed copy falls back to the \
             hosted {}.",
            route.fallback,
        ));
    }
    Ok(())
}

/// Why the `runs-on` `value` is not the reviewed label list `labels`, in a
/// workflow only [`MAINTAINER_EVENTS`] start, if it is not. `triggers` are
/// the events the workflow's `on` names, `None` if the tests do not read
/// them.
fn check_labels(
    value: &RunsOn,
    labels: &[&str],
    triggers: Option<&[String]>,
) -> Result<(), String> {
    if !matches!(value, RunsOn::List(found) if found == labels) {
        return Err(format!(
            "it runs on {value}, where the reviewed list is [{}].",
            labels.join(", ")
        ));
    }
    let events = triggers
        .ok_or("it names self-hosted runners, and the test does not read its workflow's `on`.")?;
    let others: Vec<&str> = events
        .iter()
        .map(String::as_str)
        .filter(|event| !MAINTAINER_EVENTS.contains(event))
        .collect();
    if !others.is_empty() {
        return Err(format!(
            "it names self-hosted runners in a workflow that {others:?} also start; \
             only {MAINTAINER_EVENTS:?} may."
        ));
    }
    Ok(())
}

#[test]
fn every_self_hosted_runner_a_workflow_names_is_a_reviewed_one() {
    let root = repo_root();
    let path = root.join(".github/actionlint.yaml");
    let actionlint =
        fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    let ours = actionlint_labels(&actionlint).unwrap_or_else(|e| panic!("{}:{e}", path.display()));
    let workflows = read_workflows(&root.join(".github/workflows"));
    let problems = review(&workflows, RUNNERS, &Labels::new(ours));
    assert!(problems.is_empty(), "{}", problems.join("\n\n"));
}

/// Labels for the tests: a pool's two, one the start of the other.
fn labels() -> Labels {
    Labels::new(["pool".to_owned(), "pool-windows".to_owned()])
}

/// `list` as owned strings.
fn owned(list: &[&str]) -> Vec<String> {
    list.iter().map(|item| (*item).to_owned()).collect()
}

#[test]
fn load_refuses_two_keys_that_differ_only_in_case() {
    assert_eq!(
        load("a:\n  runs-on: x\n  Runs-On: y\n").unwrap_err(),
        "3: has the key `Runs-On`, which differs only in case from `runs-on` on line 2: \
         GitHub refuses the pair"
    );
}

#[test]
fn load_refuses_two_keys_that_differ_only_in_case_in_a_list() {
    assert_eq!(
        load("steps:\n  - name: x\n    Name: y\n").unwrap_err(),
        "3: has the key `Name`, which differs only in case from `name` on line 2: \
         GitHub refuses the pair"
    );
}

#[test]
fn load_refuses_an_exact_pair_of_keys() {
    let err = load("on: push\n\"on\": pull_request\n").unwrap_err();
    assert!(
        err.starts_with("2: does not parse as YAML: duplicated key"),
        "{err}"
    );
}

#[test]
fn load_refuses_a_key_that_is_not_a_string() {
    assert_eq!(
        load("a:\n  1: x\n").unwrap_err(),
        "2: has the key `1`, where the test reads only a string"
    );
}

#[test]
fn load_refuses_a_merge_key() {
    assert_eq!(
        load("a: &a {x: 1}\nb:\n  <<: *a\n").unwrap_err(),
        "3: has a `<<` key, which GitHub reads as a key and actionlint as a merge"
    );
}

#[test]
fn load_refuses_a_tag() {
    assert_eq!(
        load("a: !pool x\n").unwrap_err(),
        "1: has the tag `!pool`, where the test reads none"
    );
}

#[test]
fn load_refuses_a_tag_inside_a_list() {
    assert_eq!(
        load("a:\n  - x\n  - !pool y\n").unwrap_err(),
        "3: has the tag `!pool`, where the test reads none"
    );
}

#[test]
fn load_refuses_a_core_schema_tag_on_a_list() {
    assert_eq!(
        load("a: !!str [x]\n").unwrap_err(),
        "1: has the tag `!!str`, where the test reads none"
    );
}

#[test]
fn load_names_the_tag_bang_as_written() {
    assert_eq!(
        load("a: ! x\n").unwrap_err(),
        "1: has the tag `!`, where the test reads none"
    );
}

#[test]
fn load_names_a_verbatim_tag_as_written() {
    assert_eq!(
        load("a: !<tag:x> y\n").unwrap_err(),
        "1: has the tag `!<tag:x>`, where the test reads none"
    );
}

#[test]
fn load_refuses_a_tag_on_a_mapping() {
    assert_eq!(
        load("a: !!map {b: 1}\n").unwrap_err(),
        "1: has the tag `!!map`, where the test reads none"
    );
}

#[test]
fn refuse_refuses_a_value_the_parser_could_not_type() {
    assert_eq!(
        refuse(&Node::from_bare_yaml(saphyr::Yaml::BadValue)).unwrap_err(),
        "0: has a value the test does not read"
    );
}

#[test]
fn load_refuses_a_second_document() {
    assert_eq!(
        load("a: 1\n---\nb: 2\n").unwrap_err(),
        "3: starts a second YAML document, where the test reads one"
    );
}

#[test]
fn load_refuses_a_file_without_a_document() {
    assert_eq!(load("# a: 1\n").unwrap_err(), "1: holds no YAML document");
}

#[test]
fn load_places_a_parse_error_at_its_line() {
    let err = load("a: 1\nb: [x\n").unwrap_err();
    assert!(err.contains(": does not parse as YAML: "), "{err}");
}

#[test]
fn get_reads_a_key_in_any_case() {
    let document = load("On: push\n").unwrap();
    let (key, value) = get(&document, "on").unwrap();
    assert_eq!((string(key), string(value)), (Some("On"), Some("push")));
}

#[test]
fn render_writes_flow_style() {
    let document = load("a: {b: [x, 1, true, null]}\n").unwrap();
    assert_eq!(render(&document), "{a: {b: [x, 1, true, null]}}");
}

#[test]
fn actionlint_labels_reads_the_list() {
    let text = "# labels\nself-hosted-runner:\n  labels:\n    # the Pi\n    - pi\n    - \"pool\"  # Proxmox\nconfig-variables: null\n";
    assert_eq!(actionlint_labels(text).unwrap(), ["pi", "pool"]);
}

#[test]
fn actionlint_labels_reads_an_escape_as_actionlint_does() {
    let backslash = char::from(92_u8);
    let text = format!("self-hosted-runner:\n  labels:\n    - \"pool{backslash}x2dgpu\"\n");
    assert_eq!(actionlint_labels(&text).unwrap(), ["pool-gpu"]);
}

#[test]
fn actionlint_labels_reads_a_folded_label_as_one() {
    let text = "self-hosted-runner:\n  labels:\n    - pool-\n      gpu\n    - pi\n";
    assert_eq!(
        actionlint_labels(text).unwrap_err(),
        "3: lists `pool- gpu`, which the test does not read as a plain label \
         (letters, digits, `.`, `_`, `-`)"
    );
}

#[test]
fn actionlint_labels_refuses_a_label_that_is_not_a_string() {
    assert_eq!(
        actionlint_labels("self-hosted-runner:\n  labels:\n    - 1\n").unwrap_err(),
        "3: lists `1`, which the test does not read as a plain label \
         (letters, digits, `.`, `_`, `-`)"
    );
}

#[test]
fn actionlint_labels_refuses_a_label_yaml_reads_as_a_number() {
    assert_eq!(
        actionlint_labels("self-hosted-runner:\n  labels:\n    - \"2024\"\n").unwrap_err(),
        "3: lists `2024`, which YAML reads as other than a string where a workflow writes \
         it unquoted as `2024`, so the test would not see it there"
    );
}

#[test]
fn actionlint_labels_refuses_a_label_yaml_reads_as_a_boolean_in_lowercase() {
    assert_eq!(
        actionlint_labels("self-hosted-runner:\n  labels:\n    - tRUE\n").unwrap_err(),
        "3: lists `tRUE`, which YAML reads as other than a string where a workflow writes \
         it unquoted as `true`, so the test would not see it there"
    );
}

#[test]
fn actionlint_labels_refuses_labels_that_are_not_a_list() {
    assert_eq!(
        actionlint_labels("self-hosted-runner:\n  labels: pool\n").unwrap_err(),
        "2: has `labels: pool`, where the test reads a list"
    );
}

#[test]
fn actionlint_labels_without_the_key_is_an_error() {
    assert_eq!(
        actionlint_labels("paths: {}\n").unwrap_err(),
        "1: has no `self-hosted-runner` key"
    );
}

#[test]
fn actionlint_labels_without_labels_is_an_error() {
    assert_eq!(
        actionlint_labels("self-hosted-runner:\n  labels:\nconfig-variables: null\n").unwrap_err(),
        "1: lists no labels under `self-hosted-runner`"
    );
}

#[test]
fn actionlint_labels_with_an_empty_list_is_an_error() {
    assert_eq!(
        actionlint_labels("self-hosted-runner:\n  labels: []\n").unwrap_err(),
        "1: lists no labels under `self-hosted-runner`"
    );
}

#[test]
fn read_finds_a_job_s_runs_on_that_names_a_self_hosted_runner() {
    let read = read("jobs:\n  a:\n    runs-on: [self-hosted, x]\n", &labels()).unwrap();
    assert_eq!(
        read.runners,
        [(
            "a".to_owned(),
            3,
            RunsOn::List(owned(&["self-hosted", "x"]))
        )]
    );
}

#[test]
fn read_finds_a_kill_switch_in_a_folded_runs_on() {
    let text = "jobs:\n  a:\n    runs-on: >-\n      ${{ vars.RP_POOL_LINUX\n      || 'x' }}\n";
    assert_eq!(
        read(text, &labels()).unwrap().runners,
        [(
            "a".to_owned(),
            3,
            RunsOn::One("${{ vars.RP_POOL_LINUX || 'x' }}".to_owned())
        )]
    );
}

#[test]
fn read_counts_a_runner_group_as_self_hosted() {
    let read = read(
        "jobs:\n  a:\n    runs-on:\n      group: Default\n",
        &labels(),
    )
    .unwrap();
    assert_eq!(
        read.runners,
        [(
            "a".to_owned(),
            3,
            RunsOn::Other("{group: Default}".to_owned())
        )]
    );
}

#[test]
fn read_reads_a_runs_on_key_quoted_or_in_capitals() {
    let text = "jobs:\n  a:\n    \"runs-on\": [pool]\n  b:\n    Runs-On: [pool]\n";
    let read = read(text, &labels()).unwrap();
    assert_eq!(
        read.runners
            .iter()
            .map(|(job, line, _)| (job.as_str(), *line))
            .collect::<Vec<_>>(),
        [("a", 3), ("b", 5)]
    );
}

#[test]
fn read_leaves_out_hosted_runners() {
    let text = "jobs:\n  a:\n    runs-on: ubuntu-latest\n  b:\n    runs-on: ${{ matrix.os }}\n";
    assert_eq!(read(text, &labels()).unwrap(), Read::default());
}

#[test]
fn read_reads_a_runs_on_only_as_a_job_s_key() {
    let text = "a:\n  b:\n    runs-on: [pool]\njobs:\n  c:\n    d:\n      runs-on: [pool]\n";
    let read = read(text, &labels()).unwrap();
    assert_eq!(read.runners, []);
    assert_eq!(read.stray, [(3, "pool".to_owned()), (7, "pool".to_owned())]);
}

#[test]
fn read_reads_an_alias_as_its_anchor_s_value() {
    let text = "x: &pool [self-hosted]\njobs:\n  a:\n    runs-on: *pool\n";
    let read = read(text, &labels()).unwrap();
    assert_eq!(
        read.runners,
        [("a".to_owned(), 4, RunsOn::List(owned(&["self-hosted"])))]
    );
    assert_eq!(read.stray, [(1, "self-hosted".to_owned())]);
}

#[test]
fn read_reports_a_label_in_a_matrix() {
    let text =
        "jobs:\n  a:\n    strategy:\n      matrix:\n        os: [ubuntu-latest, Self-Hosted]\n";
    assert_eq!(
        read(text, &labels()).unwrap().stray,
        [(5, "Self-Hosted".to_owned())]
    );
}

#[test]
fn read_reports_a_label_in_a_key() {
    assert_eq!(
        read("pool: x\n", &labels()).unwrap().stray,
        [(1, "pool".to_owned())]
    );
}

#[test]
fn read_reports_a_label_spelled_with_an_escape() {
    let backslash = char::from(92_u8);
    let text = format!("a: \"{backslash}u0073elf-hosted\"\n");
    assert_eq!(
        read(&text, &labels()).unwrap().stray,
        [(1, "self-hosted".to_owned())]
    );
}

#[test]
fn read_reports_a_block_scalar_s_line_that_names_a_label() {
    let text = "run: |\n  echo hi\n  # runs on self-hosted\n";
    assert_eq!(
        read(text, &labels()).unwrap().stray,
        [(3, "# runs on self-hosted".to_owned())]
    );
}

#[test]
fn read_reports_a_quoted_string_s_line_that_names_a_label() {
    let text = "a: [pool, \"x\n  y pool\"]\n";
    assert_eq!(
        read(text, &labels()).unwrap().stray,
        [(1, "pool".to_owned()), (2, "x y pool".to_owned())]
    );
}

#[test]
fn read_skips_comments() {
    let text = "# runs-on: [self-hosted]\na: b  # self-hosted\n";
    assert_eq!(read(text, &labels()).unwrap(), Read::default());
}

#[test]
fn read_reads_one_key_per_event() {
    let text = "on:\n  schedule:\n    - cron: '0 7 * * *'\n  workflow_dispatch:\n";
    assert_eq!(
        read(text, &labels()).unwrap().triggers.unwrap(),
        ["schedule", "workflow_dispatch"]
    );
}

#[test]
fn read_reads_a_list_of_events() {
    assert_eq!(
        read("on: [push, pull_request]\n", &labels())
            .unwrap()
            .triggers
            .unwrap(),
        ["push", "pull_request"]
    );
}

#[test]
fn read_reads_one_event() {
    assert_eq!(
        read("on: push\n", &labels()).unwrap().triggers.unwrap(),
        ["push"]
    );
}

#[test]
fn read_does_not_read_an_on_that_names_no_event() {
    assert_eq!(read("on:\n", &labels()).unwrap().triggers, None);
}

#[test]
fn review_refuses_a_runner_it_does_not_list() {
    let workflows = [Workflow {
        name: "w.yml".to_owned(),
        text: "on: push\njobs:\n  a:\n    runs-on: [self-hosted, x]\n".to_owned(),
    }];
    let problems = review(&workflows, &[], &labels());
    assert_eq!(problems.len(), 1);
    assert!(
        problems[0].starts_with(
            "w.yml:4: job `a`'s `runs-on` names a self-hosted runner, and RUNNERS \
             does not list it."
        ),
        "{}",
        problems[0]
    );
}

#[test]
fn review_refuses_a_runner_listed_for_another_job() {
    let workflows = [Workflow {
        name: "w.yml".to_owned(),
        text: "on: workflow_dispatch\njobs:\n  a:\n    runs-on: [self-hosted]\n  b:\n    runs-on: [self-hosted]\n"
            .to_owned(),
    }];
    let problems = review(
        &workflows,
        &[("w.yml", "a", Runner::Labels(&["self-hosted"]))],
        &labels(),
    );
    assert_eq!(problems.len(), 1);
    assert!(
        problems[0].starts_with(
            "w.yml:6: job `b`'s `runs-on` names a self-hosted runner, and RUNNERS \
             does not list it."
        ),
        "{}",
        problems[0]
    );
}

#[test]
fn review_refuses_a_listed_runner_it_does_not_find() {
    let workflows = [Workflow {
        name: "w.yml".to_owned(),
        text: "jobs:\n  a:\n    runs-on: ubuntu-latest\n".to_owned(),
    }];
    let problems = review(
        &workflows,
        &[("w.yml", "a", Runner::Labels(&["self-hosted"]))],
        &labels(),
    );
    assert_eq!(problems.len(), 1);
    assert!(
        problems[0].starts_with(
            "RUNNERS lists job `a` in w.yml, where the test found no `runs-on` that \
             names a self-hosted runner."
        ),
        "{}",
        problems[0]
    );
}

#[test]
fn review_refuses_a_pool_label_in_a_matrix() {
    let workflows = [Workflow {
        name: "w.yml".to_owned(),
        text:
            "jobs:\n  a:\n    strategy:\n      matrix:\n        os: [ubuntu-latest, self-hosted]\n"
                .to_owned(),
    }];
    let problems = review(&workflows, &[], &labels());
    assert_eq!(problems.len(), 1);
    assert!(
        problems[0].starts_with(
            "w.yml:5: `self-hosted` names a self-hosted label outside a job's `runs-on`."
        ),
        "{}",
        problems[0]
    );
}

#[test]
fn review_names_a_workflow_it_does_not_read() {
    let workflows = [Workflow {
        name: "w.yml".to_owned(),
        text: "jobs:\n  a: !pool x\n".to_owned(),
    }];
    assert_eq!(
        review(&workflows, &[], &labels()),
        ["w.yml:2: has the tag `!pool`, where the test reads none"]
    );
}

#[test]
fn review_does_not_look_for_the_runners_of_a_workflow_it_does_not_read() {
    let workflows = [Workflow {
        name: "w.yml".to_owned(),
        text: "jobs:\n  a: !pool x\n".to_owned(),
    }];
    assert_eq!(
        review(
            &workflows,
            &[("w.yml", "a", Runner::Labels(&["self-hosted"]))],
            &labels()
        ),
        ["w.yml:2: has the tag `!pool`, where the test reads none"]
    );
}

#[test]
fn review_places_a_failed_check_at_its_line() {
    let workflows = [Workflow {
        name: "w.yml".to_owned(),
        text: "on:\n  workflow_dispatch:\njobs:\n  a:\n    runs-on: [self-hosted, y]\n".to_owned(),
    }];
    let problems = review(
        &workflows,
        &[("w.yml", "a", Runner::Labels(&["self-hosted", "x"]))],
        &labels(),
    );
    assert_eq!(problems.len(), 1);
    assert!(
        problems[0].starts_with(
            "w.yml:5: it runs on [self-hosted, y], where the reviewed list is [self-hosted, x]."
        ),
        "{}",
        problems[0]
    );
}

#[test]
fn normalize_joins_lines_and_drops_spaces_inside_parentheses() {
    assert_eq!(
        normalize("  ( a\n      &&   b )\n  || c  "),
        "(a && b) || c"
    );
}

#[test]
fn normalize_keeps_quoted_text_as_written() {
    assert_eq!(normalize("x == 'a  ( b'"), "x == 'a  ( b'");
}

#[test]
fn split_top_level_leaves_nested_and_quoted_operators() {
    assert_eq!(
        split_top_level("(a || b) || 'c || d' || e", "||"),
        ["(a || b)", "'c || d'", "e"]
    );
}

#[test]
fn unwrap_parens_strips_a_pair_around_the_whole_term() {
    assert_eq!(unwrap_parens("(a && (b))"), "a && (b)");
}

#[test]
fn unwrap_parens_keeps_two_groups() {
    assert_eq!(unwrap_parens("(a) && (b)"), "(a) && (b)");
}

#[test]
fn labels_named_in_any_case() {
    assert!(labels().named_in("os: [ubuntu-latest, Self-Hosted]"));
}

#[test]
fn labels_named_in_matches_a_label_listed_with_capitals() {
    let labels = Labels::new(["Pool".to_owned()]);
    assert!(labels.named_in("runs-on: [pool]"));
}

#[test]
fn labels_named_in_skips_a_longer_word() {
    assert!(!labels().named_in("see docs/skills/pool-runner.md"));
}

#[test]
fn labels_named_in_reads_a_label_that_extends_another() {
    assert!(labels().named_in("[\"pool-windows\"]"));
}

/// A copy of the Linux route, as a job without a matrix spells it.
fn linux_copy(clause: &str, fallback: &str) -> RunsOn {
    RunsOn::One(["${{ (", clause, ") || ", fallback, " }}"].concat())
}

#[test]
fn check_route_accepts_a_reviewed_copy() {
    check_route(&linux_copy(LINUX, "'ubuntu-latest'"), &LINUX_JOB).unwrap();
}

#[test]
fn check_route_refuses_a_copy_without_the_same_repository_test() {
    let clause = normalize(LINUX).replace(
        " && github.event.pull_request.head.repo.full_name == github.repository",
        "",
    );
    let err = check_route(&linux_copy(&clause, "'ubuntu-latest'"), &LINUX_JOB).unwrap_err();
    assert!(
        err.starts_with("its pool branches are not the reviewed ones."),
        "{err}"
    );
}

#[test]
fn check_route_refuses_a_fallback_to_the_pool() {
    let pool = "fromJSON('[\"self-hosted\", \"proxmox-ephemeral\"]')";
    let err = check_route(&linux_copy(LINUX, pool), &LINUX_JOB).unwrap_err();
    assert!(
        err.starts_with(&format!("it falls back to {pool},")),
        "{err}"
    );
}

#[test]
fn check_route_refuses_a_missing_branch() {
    let both = Route {
        branches: &[
            (Some("ubuntu-latest"), LINUX),
            (Some("windows-latest"), WINDOWS),
        ],
        fallback: "matrix.os",
    };
    let value = [
        "${{ (matrix.os == 'windows-latest' && ",
        WINDOWS,
        ") || matrix.os }}",
    ]
    .concat();
    let err = check_route(&RunsOn::One(value), &both).unwrap_err();
    assert!(
        err.starts_with("its pool branches are not the reviewed ones."),
        "{err}"
    );
}

#[test]
fn check_route_refuses_what_is_not_one_expression() {
    let value = "${{ vars.RP_POOL_LINUX == 'on' }} pool";
    assert_eq!(
        check_route(&RunsOn::One(value.to_owned()), &LINUX_JOB).unwrap_err(),
        format!("`runs-on` is not one ${{{{ }}}} expression: {value}")
    );
}

#[test]
fn check_route_refuses_a_list() {
    assert_eq!(
        check_route(&RunsOn::List(owned(&["self-hosted"])), &LINUX_JOB).unwrap_err(),
        "`runs-on` is not one ${{ }} expression: [self-hosted]"
    );
}

#[test]
fn check_labels_accepts_a_dispatched_workflow() {
    let events = owned(&["workflow_dispatch"]);
    let value = RunsOn::List(owned(&["self-hosted", "x"]));
    check_labels(&value, &["self-hosted", "x"], Some(&events)).unwrap();
}

#[test]
fn check_labels_refuses_a_workflow_a_pull_request_starts() {
    let events = owned(&["pull_request", "workflow_dispatch"]);
    let value = RunsOn::List(owned(&["self-hosted", "x"]));
    assert_eq!(
        check_labels(&value, &["self-hosted", "x"], Some(&events)).unwrap_err(),
        "it names self-hosted runners in a workflow that [\"pull_request\"] also start; \
         only [\"schedule\", \"workflow_dispatch\"] may."
    );
}

#[test]
fn check_labels_refuses_a_workflow_whose_on_it_does_not_read() {
    let value = RunsOn::List(owned(&["self-hosted", "x"]));
    assert_eq!(
        check_labels(&value, &["self-hosted", "x"], None).unwrap_err(),
        "it names self-hosted runners, and the test does not read its workflow's `on`."
    );
}

#[test]
fn check_labels_refuses_another_list() {
    let events = owned(&["workflow_dispatch"]);
    let value = RunsOn::List(owned(&["self-hosted", "y"]));
    assert_eq!(
        check_labels(&value, &["self-hosted", "x"], Some(&events)).unwrap_err(),
        "it runs on [self-hosted, y], where the reviewed list is [self-hosted, x]."
    );
}

#[test]
fn check_labels_refuses_one_string() {
    let events = owned(&["workflow_dispatch"]);
    let value = RunsOn::One("self-hosted, x".to_owned());
    assert_eq!(
        check_labels(&value, &["self-hosted", "x"], Some(&events)).unwrap_err(),
        "it runs on self-hosted, x, where the reviewed list is [self-hosted, x]."
    );
}
