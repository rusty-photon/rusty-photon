---
applyTo: "docs/plans/**"
---

# Reviewing plan documents

A plan under `docs/plans/` records **decisions, phasing and open
questions** for work that is mostly not written yet. Each phase gets its
own design-doc phase later, where the detail is written against real
code and covered by BDD.

Review the plan as a plan. Do not review it as the implementation it
describes.

## Worth a comment

These are where review has caught real defects in plans here:

- **A claim about existing code that is false.** The plan says a helper,
  config field or contract exists, or behaves a certain way, and it does
  not. Open the file and check; cite the path.
- **A conflict with a project tenet, an ADR, or a design doc.** A plan
  proposing something `docs/decisions/` or `docs/workspace.md` forbids
  is a real finding however reasonable the proposal sounds.
- **A conflict with another plan.** Two plans that both own a tool name,
  retire and depend on the same API, or schedule incompatible changes.
- **An internal contradiction.** Two sections of the plan state
  different rules for the same thing.
- **A decision that cannot be implemented as stated** against a pinned
  dependency or an existing contract.
- **Status that overstates settledness** — a phase table or summary
  calling something decided that the plan elsewhere blocks. This misdirects
  whoever picks the work up.

## Not worth a comment

The plan is not the design doc. Do not ask it to specify:

- lock ordering, arbitration protocols, cancellation or deadlock
  freedom;
- filesystem race windows, TOCTOU sequences, or the exact syscalls used;
- permission, ACL or umask recipes;
- timeout values, retry budgets, buffer bounds;
- API shapes, error enum variants, or config field types;
- anything for a phase the plan marks deferred.

Each is a genuine concern **on the PR that implements it**, where it can
be tested. Raised against prose it produces detail that later rounds
then find fault with, without an implementation to settle the argument.

## Do not review your own previous round's additions

When a plan grows in response to review, the new prose is more surface,
not more risk. Before raising a finding, ask whether it exists only
because an earlier round asked the author to add that paragraph. If so,
it is churn: the plan is now more specific than a plan needs to be, and
the right outcome is fewer comments, not more.

Multiple review rounds on one plan should converge. If yours are not,
prefer silence to a new round of detail findings.
