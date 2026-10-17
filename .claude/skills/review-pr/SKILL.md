---
name: review-pr
description: Runs the bundled /code-review on Opus from a session on any model. Usage `/review-pr high <pr> --max-findings all`; the arguments reach /code-review verbatim. Returns the review's findings, unrated and unfixed.
argument-hint: "high <pr> --max-findings all"
model: opus
context: fork
agent: general-purpose
allowed-tools:
  - Skill(code-review)
---

Run the project's code review on Opus, whatever model the calling session
uses. The bundled review forks whoever calls it and inherits that caller's
model and context; this skill is the caller, so the review runs on Opus
with only this prompt behind it (docs/skills/code-review.md §Running a
review).

1. Call the Skill tool once: skill `code-review`, args exactly
   `$ARGUMENTS`.
2. The review runs as a background agent of your own. Wait for its
   completion notification; do not review anything yourself meanwhile.
3. Return the findings it reported, verbatim and complete: every finding
   with its file, line, summary and failure scenario. Add nothing — no
   summary, no rating, no fix. If the Skill call errored, return the
   error text verbatim instead.

Change nothing in the checkout: the reviewer reads the working tree.
