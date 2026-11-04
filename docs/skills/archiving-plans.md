# Skill: Archiving a completed plan

Plans live in [`docs/plans/`](../plans/) while their work is in flight and
move to [`docs/plans/archive/`](../plans/archive/) once delivered. Archiving
is a deliberate step: it keeps the active `docs/plans/` directory a true
to-do list, and it preserves the plan as a historical record (rationale,
hardware-validation notes, design decisions) that code and design docs link
back to.

## When to archive

Archive a plan when **every in-scope deliverable has shipped to `main`** and
been verified against the code — not just ticked off inside the plan. A plan
is *not* done merely because its own status header says so; confirm the
symbols / files / commits actually exist (`rg`, `git log --grep`, `git show`).

A plan is also archivable when it has been **superseded or obsoleted** (the
approach was abandoned, or another plan absorbed it). Mark it that way rather
than `COMPLETE`.

Do **not** archive a plan that still has open in-scope work, even if a large
chunk landed. Follow-ups the plan *explicitly parks* (clearly labelled
"future work", "deferred", "on the shelf", or tracked as separate issues) do
not block archiving — record them in the status header instead.

## Procedure

1. **Verify completion.** Enumerate the plan's deliverables (phases,
   checkboxes, "definition of done") and confirm each against the codebase.
   Treat the plan's self-reported status as a claim to check, in both
   directions: a ticked box may have regressed, and an unticked one may
   actually be done.

2. **Rewrite the status header** to the archive convention (see below).

3. **Move the file:**

   ```
   git mv docs/plans/<plan>.md docs/plans/archive/<plan>.md
   ```

4. **Fix the plan's own outbound links.** The file moved one directory
   deeper, so relative links to other `docs/` subdirs need an extra `../`
   (e.g. `](../crates/…)` → `](../../crates/…)`, `](../services/…)` →
   `](../../services/…)`). A link to a sibling that is *also* in `archive/`
   (e.g. another archived plan) becomes correct after the move — leave those.

5. **Update inbound references.** Re-point anything that linked to the old
   `docs/plans/<plan>.md`:
   - The plans index in [`docs/workspace.md`](../workspace.md) lists **in-flight
     initiatives only** — **remove** the plan's row entirely. Do *not* relist it
     under the `archive/` path; archived plans live in
     [`docs/plans/archive/`](../plans/archive/) and are not indexed in
     `workspace.md`. (Cross-links elsewhere in `workspace.md` or in design
     docs follow the next bullet.)
   - Rationale / "see the plan" links in design docs, skill docs, crate docs,
     and source doc-comments (`//!` / `///`). A living document links a
     record only where its text would otherwise be disbelieved (AGENTS.md
     rule 16), and a completed plan does not *need* inbound references, so
     most such pointers are dropped rather than re-pointed; never leave a
     link dangling at the old path.

6. **Run the pre-push gate** if steps 4–5 touched any Rust source
   (`bazel build //... && bazel test //...`, `cargo fmt`, and `cargo clippy`); a
   docs-only archive needs none of them. See [pre-push.md](pre-push.md).

7. **Commit** on a feature branch (never `main` — see [AGENTS.md](../AGENTS.md)
   rule 5) with the configured author (rule 6).

## Status-header convention

Lead the archived plan with a single bold status line, then keep the
existing summary beneath it — you are reframing the header, not deleting the
plan's content.

```
**Status: COMPLETE (archived YYYY-MM-DD).** <one-paragraph summary: which
PR(s) / issue(s) delivered it and — if relevant — what was deliberately
deferred or reversed, with the tracking issue or code location.>
```

Use `COMPLETE`, or `SUPERSEDED` / `OBSOLETE` with a pointer to whatever
replaced it. Worked examples already in the archive:
[`plate-solver.md`](../plans/archive/plate-solver.md),
[`shared-transport-extraction.md`](../plans/archive/shared-transport-extraction.md),
[`service-lifecycle-unification.md`](../plans/archive/service-lifecycle-unification.md).

Date format is `YYYY-MM-DD`; use today's date. Cite real PR / issue numbers
only — verify them in `git log`, never invent them.
