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
//! A `runs-on` that names a self-hosted runner and is not listed in
//! [`RUNNERS`] fails them, and so does a mention of `runs-on` the scan does
//! not read.
//!
//! The scan reads the workflows as text, line by line. It catches a copy
//! that drifts and a new route written the plain way, in review. A route
//! disguised on purpose gets past it, such as a label built at runtime or
//! YAML that hides a line from a line-by-line reading; the plan's R6 section
//! lists those. Against a fork's pull request, the defence remains the
//! approval its workflows wait for (ADR-020 layer 2).

use std::fs;
use std::path::{Path, PathBuf};

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
    Labels(&'static str),
}

/// Every `runs-on` that names a self-hosted runner, in workflow-name order
/// and, within a workflow, in line order.
const RUNNERS: &[(&str, Runner)] = &[
    ("bazel-coverage.yml", Runner::Pool(LINUX_JOB)),
    (
        "bazel.yml",
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
    ("check.yml", Runner::Pool(LINUX_JOB)),
    (
        "pi-nightly.yml",
        Runner::Labels("[self-hosted, Linux, ARM64, raspberry-pi]"),
    ),
    (
        "proxmox-runner-test.yml",
        Runner::Labels("[self-hosted, Linux, X64, proxmox-ephemeral]"),
    ),
    (
        "proxmox-runner-test.yml",
        Runner::Labels("[self-hosted, Windows, X64, proxmox-ephemeral-windows]"),
    ),
    ("repin-bazel.yml", Runner::Pool(LINUX_JOB)),
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

/// What a scan of one workflow found, each with its line.
#[derive(Debug, Default, PartialEq, Eq)]
struct Scan {
    /// Each `runs-on` value that names a self-hosted runner.
    runners: Vec<(usize, String)>,
    /// Each other line that names one of the labels, or mentions `runs-on`
    /// in a form the scan does not read.
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

/// Our runners' own labels, as `.github/actionlint.yaml` lists them under
/// `self-hosted-runner:` and `labels:`, one `- label` per line, quoted or
/// not, or `None` if the scan finds none there.
fn actionlint_labels(text: &str) -> Option<Vec<String>> {
    let lines: Vec<&str> = text.lines().collect();
    let at = lines
        .iter()
        .position(|line| line.trim_end() == "self-hosted-runner:")?;
    let mut labels = Vec::new();
    let mut in_labels = false;
    for line in lines.get(at.saturating_add(1)..)? {
        let body = line.trim();
        if body.is_empty() || body.starts_with('#') {
            continue;
        }
        if indent(line) == 0 {
            break;
        }
        if body == "labels:" {
            in_labels = true;
        } else if let Some(label) = body.strip_prefix("- ").filter(|_| in_labels) {
            let label = label
                .split_once(" #")
                .map_or(label, |(label, _)| label)
                .trim();
            // YAML may quote a label; actionlint reads it without the quotes.
            let label = ['"', '\'']
                .iter()
                .find_map(|quote| label.strip_prefix(*quote)?.strip_suffix(*quote))
                .unwrap_or(label);
            labels.push(label.to_owned());
        } else {
            in_labels = false;
        }
    }
    (!labels.is_empty()).then_some(labels)
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
fn review(workflows: &[Workflow], reviewed: &[(&str, Runner)], labels: &Labels) -> Vec<String> {
    let mut problems = Vec::new();
    let mut found = Vec::new();
    for workflow in workflows {
        let scan = scan(&workflow.text, labels);
        for (line, text) in scan.stray {
            problems.push(format!(
                "{}:{line}: `{text}` names a self-hosted label outside a listed \
                 `runs-on`, or mentions `runs-on` in a form the test does not read.\n{MOVED}",
                workflow.name,
            ));
        }
        found.extend(
            scan.runners
                .into_iter()
                .map(|(line, value)| (workflow, line, value)),
        );
    }
    let names: Vec<&str> = found.iter().map(|(w, _, _)| w.name.as_str()).collect();
    let listed: Vec<&str> = reviewed.iter().map(|(name, _)| *name).collect();
    if names != listed {
        problems.push(format!(
            "the `runs-on` values that name a self-hosted runner are in {names:?}, \
             where RUNNERS lists {listed:?}. A new one is reviewed against ADR-020 \
             and listed there.\n{MOVED}"
        ));
        return problems;
    }
    for ((workflow, line, value), (_, runner)) in found.iter().zip(reviewed) {
        let result = match runner {
            Runner::Pool(route) => check_route(value, route),
            Runner::Labels(labels) => check_labels(value, labels, &workflow.text),
        };
        if let Err(e) = result {
            problems.push(format!("{}:{line}: {e}\n{MOVED}", workflow.name));
        }
    }
    problems
}

/// The `runs-on` values in a workflow that name a self-hosted runner (a
/// pool kill switch, one of the `labels` or a runner group), and the lines
/// that name one of the `labels` elsewhere. A value runs on over every line
/// indented deeper than its key, as a block scalar (`>-`, `|`) or a plain
/// one does, and is joined into one line. In a block scalar a line that
/// starts with `#` is text, not a comment.
fn scan(text: &str, labels: &Labels) -> Scan {
    let lines: Vec<&str> = text.lines().collect();
    let mut scan = Scan::default();
    let mut next = 0;
    // The indentation of the key whose block scalar the lines below belong to.
    let mut block = None;
    for (at, line) in lines.iter().enumerate() {
        if at < next {
            continue;
        }
        let number = at.saturating_add(1);
        let body = line.trim_start();
        if let Some(key) = block {
            if body.is_empty() || indent(line) > key {
                if labels.named_in(body) {
                    scan.stray.push((number, body.to_owned()));
                }
                continue;
            }
            block = None;
        }
        if body.starts_with('#') {
            continue;
        }
        let Some(first) = body.strip_prefix("runs-on:") else {
            if body.to_lowercase().contains("runs-on") || labels.named_in(body) {
                scan.stray.push((number, body.to_owned()));
            }
            if opens_block(body) {
                block = Some(indent(line));
            }
            continue;
        };
        let rest = lines.get(number..).unwrap_or_default();
        let deeper = rest
            .iter()
            .take_while(|next| next.trim().is_empty() || indent(next) > indent(line))
            .count();
        next = number.saturating_add(deeper);
        let first = first.trim();
        let is_block = first.starts_with(['>', '|']);
        let first = if is_block { "" } else { first };
        let value = std::iter::once(first)
            .chain(rest.iter().take(deeper).map(|next| next.trim()))
            .filter(|part| !part.is_empty() && (is_block || !part.starts_with('#')))
            .collect::<Vec<_>>()
            .join(" ");
        // A runner group picks self-hosted runners: this organization's only
        // group is the default one, which holds the pool's runners.
        if value.to_lowercase().contains("rp_pool_")
            || labels.named_in(&value)
            || has_word(&value, "group")
        {
            scan.runners.push((number, value));
        }
    }
    scan
}

fn indent(line: &str) -> usize {
    line.len().saturating_sub(line.trim_start().len())
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

/// Whether `body`, a line without its indentation, ends in a block scalar's
/// indicator (`>` or `|`, with its chomping and indentation marks), so the
/// lines indented below it are text.
fn opens_block(body: &str) -> bool {
    let value = body
        .split_once(": ")
        .map_or_else(|| body.strip_prefix("- ").unwrap_or(""), |(_, value)| value);
    let Some(marks) = value.trim().strip_prefix(['>', '|']) else {
        return false;
    };
    let rest = marks.trim_start_matches(|c: char| c == '-' || c == '+' || c.is_ascii_digit());
    rest.trim().is_empty() || rest.trim_start().starts_with('#')
}

/// The events a workflow's top-level `on:` names, or `None` if the scan
/// does not read it. It reads one event or a `[list]` after the key, or one
/// key per event on the lines below.
fn triggers(text: &str) -> Option<Vec<String>> {
    let lines: Vec<&str> = text.lines().collect();
    let at = lines.iter().position(|line| line.starts_with("on:"))?;
    let inline = lines.get(at)?.strip_prefix("on:")?.trim();
    if !inline.is_empty() {
        let list = inline
            .strip_prefix('[')
            .and_then(|list| list.strip_suffix(']'))
            .unwrap_or(inline);
        return Some(list.split(',').map(|e| e.trim().to_owned()).collect());
    }
    let body: Vec<&str> = lines
        .get(at.saturating_add(1)..)?
        .iter()
        .take_while(|line| line.trim().is_empty() || line.starts_with([' ', '#']))
        .filter(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'))
        .copied()
        .collect();
    let depth = indent(body.first()?);
    Some(
        body.iter()
            .filter(|line| indent(line) == depth)
            .map(|line| line.trim().split(':').next().unwrap_or_default().to_owned())
            .collect(),
    )
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
fn check_route(value: &str, route: &Route) -> Result<(), String> {
    let (branches, fallback) = parse(value)?;
    let reviewed: Vec<String> = route
        .branches
        .iter()
        .map(|(label, clause)| {
            let matrix = label
                .map(|label| format!("matrix.os == '{label}' && "))
                .unwrap_or_default();
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

/// Why the `runs-on` `value`, in the workflow `text`, is not the reviewed
/// label list `labels` in a workflow only [`MAINTAINER_EVENTS`] start.
fn check_labels(value: &str, labels: &str, text: &str) -> Result<(), String> {
    if value != labels {
        return Err(format!(
            "it runs on {value}, where the reviewed list is {labels}."
        ));
    }
    let events = triggers(text)
        .ok_or("it names self-hosted runners, and the test does not read its workflow's `on:`.")?;
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
    let ours = actionlint_labels(&actionlint)
        .unwrap_or_else(|| panic!("{} lists no self-hosted-runner labels", path.display()));
    let workflows = read_workflows(&root.join(".github/workflows"));
    let problems = review(&workflows, RUNNERS, &Labels::new(ours));
    assert!(problems.is_empty(), "{}", problems.join("\n\n"));
}

/// Labels for the tests: a pool's two, one the start of the other.
fn labels() -> Labels {
    Labels::new(["pool".to_owned(), "pool-windows".to_owned()])
}

#[test]
fn actionlint_labels_reads_the_list_without_comments() {
    let text = "# labels\nself-hosted-runner:\n  labels:\n    # the Pi\n    - pi\n    - pool  # Proxmox\nconfig-variables: null\n";
    assert_eq!(actionlint_labels(text).unwrap(), ["pi", "pool"]);
}

#[test]
fn actionlint_labels_reads_a_quoted_label_without_its_quotes() {
    let text = "self-hosted-runner:\n  labels:\n    - \"pool\"  # Proxmox\n    - 'pi'\n";
    assert_eq!(actionlint_labels(text).unwrap(), ["pool", "pi"]);
}

#[test]
fn actionlint_labels_without_the_list_is_none() {
    assert_eq!(actionlint_labels("paths: {}\n"), None);
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
            "the `runs-on` values that name a self-hosted runner are in [\"w.yml\"], \
             where RUNNERS lists []."
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
        problems[0].starts_with("w.yml:5: `os: [ubuntu-latest, self-hosted]` names"),
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
fn scan_joins_a_folded_block_and_gives_its_line() {
    let text = "jobs:\n  a:\n    runs-on: >-\n      ${{ (vars.RP_POOL_LINUX == 'on'\n           && x)\n          || 'ubuntu-latest' }}\n    name: a\n";
    assert_eq!(
        scan(text, &labels()).runners,
        [(
            3,
            "${{ (vars.RP_POOL_LINUX == 'on' && x) || 'ubuntu-latest' }}".to_owned()
        )]
    );
}

#[test]
fn scan_joins_a_plain_value_continued_below_its_key() {
    let text = "    runs-on: ${{ (github.event_name == 'push'\n      && 'self-hosted') || 'ubuntu-latest' }}\n    name: a\n";
    assert_eq!(
        scan(text, &labels()).runners,
        [(
            1,
            "${{ (github.event_name == 'push' && 'self-hosted') || 'ubuntu-latest' }}".to_owned()
        )]
    );
}

#[test]
fn scan_drops_the_blank_lines_after_a_value() {
    let text = "    runs-on: >-\n      ${{ vars.RP_POOL_LINUX }}\n\n\n    name: a\n";
    assert_eq!(
        scan(text, &labels()).runners,
        [(1, "${{ vars.RP_POOL_LINUX }}".to_owned())]
    );
}

#[test]
fn scan_reads_a_label_list() {
    let text = "    runs-on: [self-hosted, Linux]\n";
    assert_eq!(
        scan(text, &labels()).runners,
        [(1, "[self-hosted, Linux]".to_owned())]
    );
}

#[test]
fn scan_leaves_out_hosted_runners() {
    let text = "    runs-on: ubuntu-latest\n    runs-on: ${{ matrix.os }}\n";
    assert_eq!(scan(text, &labels()), Scan::default());
}

#[test]
fn scan_reports_a_runs_on_it_does_not_read() {
    let text = "    \"runs-on\": ubuntu-latest\n";
    assert_eq!(
        scan(text, &labels()).stray,
        [(1, "\"runs-on\": ubuntu-latest".to_owned())]
    );
}

#[test]
fn scan_counts_a_runner_group_as_self_hosted() {
    let text = "    runs-on:\n      group: Default\n    name: a\n";
    assert_eq!(
        scan(text, &labels()).runners,
        [(1, "group: Default".to_owned())]
    );
}

#[test]
fn scan_skips_comments() {
    let text = "    # runs-on: [self-hosted]\n";
    assert_eq!(scan(text, &labels()), Scan::default());
}

#[test]
fn scan_reads_a_hash_line_in_a_block_scalar_as_text() {
    let text = "        os: >-\n          ${{ fromJSON('[\"x\", \"\n          #\", \"self-hosted\"]') }}\n";
    assert_eq!(
        scan(text, &labels()).stray,
        [(3, "#\", \"self-hosted\"]') }}".to_owned())]
    );
}

#[test]
fn scan_keeps_a_hash_line_in_a_block_runs_on() {
    let text = "    runs-on: >-\n      ${{ vars.RP_POOL_LINUX\n      #x\n      }}\n";
    assert_eq!(
        scan(text, &labels()).runners,
        [(1, "${{ vars.RP_POOL_LINUX #x }}".to_owned())]
    );
}

#[test]
fn scan_ends_a_block_scalar_at_its_key() {
    let text = "      run: |\n        echo hi\n      # runs on self-hosted\n";
    assert_eq!(scan(text, &labels()), Scan::default());
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

#[test]
fn opens_block_reads_a_key_or_list_item_ending_in_an_indicator() {
    assert!(opens_block("run: |"));
    assert!(opens_block("if: >-  # why"));
    assert!(opens_block("- |2"));
    assert!(!opens_block("run: echo \"a: |\""));
    assert!(!opens_block("if: ${{ a || b }}"));
}

#[test]
fn review_places_a_failed_check_at_its_line() {
    let workflows = [Workflow {
        name: "w.yml".to_owned(),
        text: "on:\n  workflow_dispatch:\njobs:\n  a:\n    runs-on: [self-hosted, y]\n".to_owned(),
    }];
    let problems = review(
        &workflows,
        &[("w.yml", Runner::Labels("[self-hosted, x]"))],
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
fn triggers_reads_one_key_per_event() {
    let text =
        "on:\n  # nightly\n  schedule:\n    - cron: '0 7 * * *'\n\n  workflow_dispatch:\njobs:\n";
    assert_eq!(triggers(text).unwrap(), ["schedule", "workflow_dispatch"]);
}

#[test]
fn triggers_reads_an_inline_list() {
    assert_eq!(
        triggers("on: [push, pull_request]\n").unwrap(),
        ["push", "pull_request"]
    );
}

#[test]
fn triggers_without_an_on_key_is_none() {
    assert_eq!(triggers("\"on\": push\n"), None);
}

/// A copy of the Linux route, as a job without a matrix spells it.
fn linux_copy(clause: &str, fallback: &str) -> String {
    ["${{ (", clause, ") || ", fallback, " }}"].concat()
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
    let err = check_route(&value, &both).unwrap_err();
    assert!(
        err.starts_with("its pool branches are not the reviewed ones."),
        "{err}"
    );
}

#[test]
fn check_route_refuses_what_is_not_one_expression() {
    let value = "${{ vars.RP_POOL_LINUX == 'on' }} # pool";
    assert_eq!(
        check_route(value, &LINUX_JOB).unwrap_err(),
        format!("`runs-on` is not one ${{{{ }}}} expression: {value}")
    );
}

#[test]
fn check_labels_accepts_a_dispatched_workflow() {
    let text = "on:\n  workflow_dispatch:\n";
    check_labels("[self-hosted, x]", "[self-hosted, x]", text).unwrap();
}

#[test]
fn check_labels_refuses_a_workflow_a_pull_request_starts() {
    let text = "on:\n  pull_request:\n  workflow_dispatch:\n";
    assert_eq!(
        check_labels("[self-hosted, x]", "[self-hosted, x]", text).unwrap_err(),
        "it names self-hosted runners in a workflow that [\"pull_request\"] also start; \
         only [\"schedule\", \"workflow_dispatch\"] may."
    );
}

#[test]
fn check_labels_refuses_another_list() {
    let text = "on:\n  workflow_dispatch:\n";
    assert_eq!(
        check_labels("[self-hosted, y]", "[self-hosted, x]", text).unwrap_err(),
        "it runs on [self-hosted, y], where the reviewed list is [self-hosted, x]."
    );
}
