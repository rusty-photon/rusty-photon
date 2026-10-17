//! The pool's routing stays as reviewed.
//!
//! Four workflows send a job to the self-hosted pool with a copy of one
//! `runs-on` expression. For each OS it holds the kill-switch variable, the
//! trusted-event test and the pool's labels, and every falsy branch falls
//! back to a hosted runner. ADR-020 makes the expression a security
//! boundary. It is copied because `runs-on` is evaluated before the job
//! exists, so neither `env` nor a job output can hold it.
//!
//! These tests hold every copy to the clauses reviewed here. A pull request
//! that changes one copy fails `bazel test //...`, and one that changes them
//! all changes this file too, in the diff its reviewer reads. A new job that
//! routes to the pool fails them until it is listed in [`ROUTES`].

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

/// A job that routes to the pool, as reviewed.
struct Route {
    /// Its workflow, in `.github/workflows`.
    workflow: &'static str,
    /// Each pool branch in order: the `matrix.os` label it sits behind, when
    /// the job has a matrix, and its clause.
    branches: &'static [(Option<&'static str>, &'static str)],
    /// The hosted runner every falsy branch lands on.
    fallback: &'static str,
}

/// Every job that routes to the pool, in workflow-name order.
const ROUTES: &[Route] = &[
    Route {
        workflow: "bazel-coverage.yml",
        branches: &[(None, LINUX)],
        fallback: "'ubuntu-latest'",
    },
    Route {
        workflow: "bazel.yml",
        branches: &[
            (Some("ubuntu-latest"), LINUX),
            (Some("windows-latest"), WINDOWS),
        ],
        // The matrix's own label, always a hosted one.
        fallback: "matrix.os",
    },
    Route {
        workflow: "check.yml",
        branches: &[(None, LINUX)],
        fallback: "'ubuntu-latest'",
    },
    Route {
        workflow: "repin-bazel.yml",
        branches: &[(None, LINUX)],
        fallback: "'ubuntu-latest'",
    },
];

/// The end of every refusal.
const MOVED: &str = "A divergence here is a security boundary moving (ADR-020). \
     If the change is meant, make it in every copy and in \
     tools/rusty-photon-ci/src/pool_routing.rs.";

/// A `runs-on` expression that names the pool, and where it is.
struct Found {
    workflow: String,
    line: usize,
    expression: String,
}

/// `.github/workflows`, two levels above the crate. Cargo sets
/// `CARGO_MANIFEST_DIR` when it runs a test. For Bazel the BUILD file sets
/// it, relative to the runfiles root the test starts in, which holds the
/// workflows as the test's data.
fn workflows_dir() -> PathBuf {
    let crate_dir = std::env::var_os("CARGO_MANIFEST_DIR")
        .expect("CARGO_MANIFEST_DIR is set by Cargo, and for Bazel by the BUILD file");
    Path::new(&crate_dir)
        .ancestors()
        .nth(2)
        .expect("the crate sits two levels below the repository root")
        .join(".github/workflows")
}

/// Every pool `runs-on` in the workflows in `dir`, in workflow-name order.
fn pool_routes(dir: &Path) -> Vec<Found> {
    let mut workflows: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()))
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|ext| ext == "yml" || ext == "yaml")
        })
        .collect();
    workflows.sort();
    let mut found = Vec::new();
    for path in workflows {
        let text =
            fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        let workflow = path.file_name().unwrap().to_string_lossy().into_owned();
        for (line, expression) in pool_expressions(&text) {
            found.push(Found {
                workflow: workflow.clone(),
                line,
                expression,
            });
        }
    }
    found
}

/// Every `runs-on` in a workflow that is an expression naming the pool, with
/// its line. A block value (`>-`, `|`), or one on the lines below the key, is
/// joined into one line. A label list is not an expression and is left out:
/// proxmox-runner-test.yml, which only a maintainer can dispatch, names the
/// pool that way.
fn pool_expressions(text: &str) -> Vec<(usize, String)> {
    let lines: Vec<&str> = text.lines().collect();
    let mut found = Vec::new();
    for (at, line) in lines.iter().enumerate() {
        let Some(value) = line.trim_start().strip_prefix("runs-on:") else {
            continue;
        };
        let value = value.trim();
        let value = if value.is_empty() || value.starts_with(['>', '|']) {
            let below = lines.get(at.saturating_add(1)..).unwrap_or_default();
            below
                .iter()
                .take_while(|next| next.trim().is_empty() || indent(next) > indent(line))
                .map(|next| next.trim())
                .collect::<Vec<_>>()
                .join(" ")
        } else {
            value.to_owned()
        };
        if value.contains("${{")
            && (value.contains("RP_POOL_") || value.contains("proxmox-ephemeral"))
        {
            found.push((at.saturating_add(1), value));
        }
    }
    found
}

fn indent(line: &str) -> usize {
    line.len().saturating_sub(line.trim_start().len())
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
fn parse(value: &str) -> Result<(Vec<String>, String), &'static str> {
    let expr = value
        .strip_prefix("${{")
        .and_then(|v| v.strip_suffix("}}"))
        .ok_or("`runs-on` is not one ${{ }} expression")?;
    let expr = normalize(expr);
    let mut terms: Vec<String> = split_top_level(&expr, "||")
        .into_iter()
        .map(|term| unwrap_parens(term).to_owned())
        .collect();
    let fallback = terms.pop().unwrap_or_default();
    Ok((terms, fallback))
}

/// Why `found` is not the reviewed `route`, if it is not.
fn check(found: &Found, route: &Route) -> Result<(), String> {
    let at = format!("{}:{}", found.workflow, found.line);
    let (branches, fallback) = parse(&found.expression).map_err(|e| format!("{at}: {e}"))?;
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
            "{at}: its pool branches are not the reviewed ones.\n  found:\n    {}\n  \
             reviewed:\n    {}\n{MOVED}",
            branches.join("\n    "),
            reviewed.join("\n    "),
        ));
    }
    if fallback != route.fallback {
        return Err(format!(
            "{at}: it falls back to {fallback}, where the reviewed copy falls back to \
             the hosted {}.\n{MOVED}",
            route.fallback,
        ));
    }
    Ok(())
}

#[test]
fn every_job_that_routes_to_the_pool_holds_the_reviewed_clauses() {
    let found = pool_routes(&workflows_dir());
    let workflows: Vec<&str> = found.iter().map(|f| f.workflow.as_str()).collect();
    let listed: Vec<&str> = ROUTES.iter().map(|r| r.workflow).collect();
    assert_eq!(
        workflows, listed,
        "the jobs that route to the pool are not the ones ROUTES lists. A new one \
         is reviewed against ADR-020 and listed there."
    );
    let wrong: Vec<String> = found
        .iter()
        .zip(ROUTES)
        .filter_map(|(found, route)| check(found, route).err())
        .collect();
    assert!(wrong.is_empty(), "{}", wrong.join("\n\n"));
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
fn pool_expressions_joins_a_folded_block_and_gives_its_line() {
    let text = "jobs:\n  a:\n    runs-on: >-\n      ${{ (vars.RP_POOL_LINUX == 'on'\n           && x)\n          || 'ubuntu-latest' }}\n    name: a\n";
    assert_eq!(
        pool_expressions(text),
        [(
            3,
            "${{ (vars.RP_POOL_LINUX == 'on' && x) || 'ubuntu-latest' }}".to_owned()
        )]
    );
}

#[test]
fn pool_expressions_reads_an_inline_expression() {
    let text = "    runs-on: ${{ vars.RP_POOL_LINUX == 'on' && 'x' || 'ubuntu-latest' }}\n";
    assert_eq!(
        pool_expressions(text),
        [(
            1,
            "${{ vars.RP_POOL_LINUX == 'on' && 'x' || 'ubuntu-latest' }}".to_owned()
        )]
    );
}

#[test]
fn pool_expressions_leaves_out_hosted_runners_and_label_lists() {
    let text = "    runs-on: ubuntu-latest\n    runs-on: ${{ matrix.os }}\n    runs-on: [self-hosted, proxmox-ephemeral]\n";
    assert_eq!(pool_expressions(text), []);
}

/// A copy of the Linux route, as a standalone job spells it.
fn linux_copy(clause: &str, fallback: &str) -> Found {
    Found {
        workflow: "w.yml".to_owned(),
        line: 7,
        expression: ["${{ (", clause, ") || ", fallback, " }}"].concat(),
    }
}

const LINUX_ROUTE: Route = Route {
    workflow: "w.yml",
    branches: &[(None, LINUX)],
    fallback: "'ubuntu-latest'",
};

#[test]
fn check_accepts_a_reviewed_copy() {
    check(&linux_copy(LINUX, "'ubuntu-latest'"), &LINUX_ROUTE).unwrap();
}

#[test]
fn check_refuses_a_copy_without_the_same_repository_test() {
    let clause = normalize(LINUX).replace(
        " && github.event.pull_request.head.repo.full_name == github.repository",
        "",
    );
    let err = check(&linux_copy(&clause, "'ubuntu-latest'"), &LINUX_ROUTE).unwrap_err();
    assert!(
        err.starts_with("w.yml:7: its pool branches are not the reviewed ones."),
        "{err}"
    );
}

#[test]
fn check_refuses_a_fallback_to_the_pool() {
    let pool = "fromJSON('[\"self-hosted\", \"proxmox-ephemeral\"]')";
    let err = check(&linux_copy(LINUX, pool), &LINUX_ROUTE).unwrap_err();
    assert!(
        err.starts_with(&format!("w.yml:7: it falls back to {pool},")),
        "{err}"
    );
}

#[test]
fn check_refuses_a_missing_branch() {
    let both = Route {
        workflow: "w.yml",
        branches: &[
            (Some("ubuntu-latest"), LINUX),
            (Some("windows-latest"), WINDOWS),
        ],
        fallback: "matrix.os",
    };
    let found = Found {
        workflow: "w.yml".to_owned(),
        line: 7,
        expression: [
            "${{ (matrix.os == 'windows-latest' && ",
            WINDOWS,
            ") || matrix.os }}",
        ]
        .concat(),
    };
    let err = check(&found, &both).unwrap_err();
    assert!(
        err.starts_with("w.yml:7: its pool branches are not the reviewed ones."),
        "{err}"
    );
}

#[test]
fn check_refuses_a_quoted_expression() {
    let found = Found {
        workflow: "w.yml".to_owned(),
        line: 7,
        expression: "\"${{ vars.RP_POOL_LINUX == 'on' }}\"".to_owned(),
    };
    assert_eq!(
        check(&found, &LINUX_ROUTE).unwrap_err(),
        "w.yml:7: `runs-on` is not one ${{ }} expression"
    );
}
