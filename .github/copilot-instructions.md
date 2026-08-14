# Code review instructions

rusty-photon is a Rust workspace of astrophotography services running
unattended overnight on remote hardware. A missed defect wastes an
imaging night or damages equipment; a wrong comment costs a maintainer
a researched rebuttal. Comment accordingly.

## Assume the build is green

Every PR has already passed, on Linux, macOS and Windows: `bazel build
//... && bazel test //...`, `cargo clippy --all-targets -- -D warnings`
(with and without `--all-features`), `cargo fmt --check`, and the BDD
suites.

Never predict a build, lint or format outcome. Do not report that code
will not compile, that a borrow or move is invalid, that a trait impl
or import is missing, that a lint will fire, or that formatting is
wrong. These are settled before you see the diff, and such comments
have been wrong nearly every time they were made here.

## Verify existence before claiming absence

You see a diff, not the branch. Never assert that a type, function,
field, file, label or config key "does not exist", "was removed" or
"is undefined" — earlier commits on the branch, or files outside the
diff, routinely define it. If a reference looks unresolved, open the
file and confirm, or say nothing.

## Only claim behavior you can derive from the diff

Do not assert how an external tool behaves unless the diff shows it.
Claims about udev precedence, systemd unit resolution, container
networking, GitHub Actions contexts and label APIs, shell redirection
order, platform tool flags and third-party crate semantics have been
the most expensive wrong comments here. If a claim rests on a tool's
documented behavior, cite that documentation or do not raise it.

## Priorities

Report these, highest first. They are where review has caught defects
no compiler, lint or test could:

1. **Concurrency and lifetime** — TOCTOU windows, guards held across
   `.await`, unlocked read-modify-write, detached tasks outliving
   their purpose, missing abort or rollback on the error path.
2. **Security** — operator-controlled values rendered unescaped,
   credentials or tokens reaching logs, `Debug` output or non-TLS
   peers, secrets created before permissions are tightened,
   symlink-following writes, unvalidated input reaching a path join.
3. **Silent wrongness** — values dropped, truncated, defaulted or
   coerced instead of failing: `unwrap_or` masking an error, casts
   that wrap, missing `deny_unknown_fields`, division by an
   unvalidated zero, errors flattened to strings.
4. **Missing timeouts and bounds** — network or device calls with no
   timeout, retries that cannot observe a stall, unbounded reads.
5. **The other place needing the same change** — a new service, port
   or dependency wired into some registration sites but not all; a
   validator fixed on one field but not its twin; a packaged artifact
   linking a library the package manifest omits.
6. **Tests that cannot fail** — assertions that hold regardless of the
   code under test, fixtures degenerate enough to prove nothing,
   scenarios the prose promises but no step exercises.

## Raise the bar for everything else

Comment on documentation, comments and naming **only when following
the text would cause a wrong action** — wrong units, a stated contract
the code contradicts, an operator procedure missing a required step.
Not spelling, grammar, tense, phrasing, stale phase or status labels,
heading structure, or example values.

Do not restate the PR's scope back to the author, and do not ask for
work the PR deliberately defers.

## One comment per finding

Never repeat a finding. If the same issue appears in several files,
comment once on the clearest instance and name the others in that
comment. Before re-raising anything in a later review round, check
whether an intervening commit already fixed it.

State the consequence concretely: the input or state that triggers it,
and what goes wrong. If you are not confident it is real, say nothing.
