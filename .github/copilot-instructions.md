# Code review instructions

rusty-photon is a Rust workspace of astrophotography services running
unattended overnight on remote hardware. A missed defect wastes an
imaging night or damages equipment; a wrong comment costs a maintainer
a researched rebuttal. Comment accordingly.

## Anchor every finding to what the PR is for

Work out the PR's purpose first — the change it makes and the problem
it solves — and review that. A finding must bear on whether the PR
achieves its purpose, or breaks something on the way.

The priorities below say which defects are worth reporting *within*
that scope. They are not a licence to review the surrounding system, to
hold the change to a standard its purpose does not imply, or to ask for
work it defers. A PR that adds a plan, fixes one bug or renames a field
is not an invitation to specify what it touches.

**When the diff is a document, review the document — not the code it
describes as though that code were written.** A document's defects are
false claims, internal contradictions, and conflicts with the repo's
decisions. Its prose is not an implementation to audit for races,
bounds or filesystem hazards; that review belongs on the PR that writes
the code, where it can be tested.

Do not restate the PR's scope back to the author.

## Do not assert what the diff cannot show

Every PR has already passed `bazel build //... && bazel test //...` on
Linux and Windows, clippy with `-D warnings`, `cargo fmt --check`, and
the BDD suites. Never predict a build, lint or format outcome — that
code will not compile, a borrow is invalid, an import is missing, a
lint will fire. These are settled before you see the diff, and such
comments have been wrong nearly every time.

You see a diff, not the branch: never assert that a type, function,
field, file or config key "does not exist" or "was removed". Earlier
commits, or files outside the diff, routinely define it. Open the file
and confirm, or say nothing.

Do not assert how an external tool behaves unless the diff shows it.
Claims about udev precedence, systemd resolution, GitHub Actions
contexts and third-party crate semantics have been the most expensive
wrong comments here. Cite the documentation or do not raise it.

## Priorities

Report these, highest first, when they fall inside the PR's purpose:

1. **Concurrency and lifetime** — TOCTOU windows, guards held across
   `.await`, unlocked read-modify-write, detached tasks outliving their
   purpose, missing rollback on the error path.
2. **Security** — operator-controlled values rendered unescaped,
   credentials reaching logs or non-TLS peers, secrets created before
   permissions are tightened, unvalidated input reaching a path join.
3. **Silent wrongness** — values dropped, truncated, defaulted or
   coerced instead of failing: `unwrap_or` masking an error, casts that
   wrap, missing `deny_unknown_fields`.
4. **Missing timeouts and bounds** — device or network calls with no
   timeout, retries that cannot observe a stall, unbounded reads.
5. **The other place needing the same change** — a new service, port or
   dependency wired into some registration sites but not all.
6. **Tests that cannot fail** — assertions that hold regardless of the
   code under test, scenarios no step exercises.

## Raise the bar for everything else

Comment on documentation, comments and naming **only when following the
text would cause a wrong action** — wrong units, a stated contract the
code contradicts, a procedure missing a required step. Not spelling,
grammar, phrasing, stale status labels, or example values.

## One comment per finding

Never repeat a finding across files or rounds. Comment once on the
clearest instance and name the others there. Before re-raising anything
in a later round, check whether an intervening commit fixed it.

State the consequence concretely: the input or state that triggers it,
and what goes wrong. If you are not confident it is real, say nothing.
