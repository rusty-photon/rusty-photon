---
applyTo: "**/*.md"
---

# Reviewing documentation in this repo

Documentation review is where most low-value review comments have been
spent here. Apply a high bar.

## The bar: would following this text cause a wrong action?

Comment only when a reader acting on the text would do the wrong
thing. Real examples worth raising:

- A unit is wrong or unstated where it matters (µm vs mm, hours vs
  degrees, arcminutes vs arcseconds).
- A stated contract contradicts the code — the doc says a value is
  cleared after each apply and it is not; says a change needs no
  restart when one is required; says a field is additive when it
  replaces.
- An operator recovery or setup procedure omits a required step, so
  following it leaves the system broken or half-configured.
- A documented command, flag or path does not work as written.
- A security claim is untrue, so a reader trusts a protection that
  does not hold.

## Do not comment on

Spelling, grammar, capitalization, tense or verb agreement. Wording,
phrasing or tone. Heading levels, list style, table column order, link
formatting, or whether a term is in a code span. Stale phase, status
or roadmap labels ("Phase 2 — next", "still outstanding"). Example
values, placeholder hostnames, hard-coded sample versions, or a
repository name in an illustrative command. Whether a plan document
matches the final implementation detail-for-detail.

None of these change what a reader does, and they crowd out findings
that do.

## One comment per document

If the same inaccuracy appears in a design doc, a plan doc and a code
comment, raise it once on the authoritative source — the design
document under `docs/services/` or `docs/` — and name the other
locations in that one comment. Do not open a thread per file.

## Scope

Review the diff against what the PR is for. Do not compare the PR body
against the diff and report mismatches in the PR description.

Plan documents under `docs/plans/` have their own instructions file —
they record decisions and phasing, not implementation detail.

## Repository conventions

- This is a public repository. Flag any RFC1918 or otherwise internal
  IP address, internal hostname, credential or token that appears in
  documentation; these must be placeholders.
- Design docs live at `docs/services/<service>.md`. A change to a
  service's behavior, port, wire format or configuration should be
  reflected there; a substantive behavior change with no corresponding
  design-doc update is worth one comment.
- Cite documentation by repository-relative path. Do not claim a
  referenced file, rule or section does not exist unless you have
  opened the path and confirmed it is absent.
