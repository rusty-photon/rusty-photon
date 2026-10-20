// A scheduled workflow's tracking issue: opened when a scheduled run fails,
// commented on by each later failure while it stays open. Plan item J7 in
// docs/plans/ci-shell-to-code.md.
//
// Only OPEN issues are searched. Closing the issue marks the failure
// handled, so the next failure opens a fresh issue rather than resurrecting
// the closed one. Others find the issue by its exact title and label
// (refresh-astap links its rotation PR to install-astap's), so a workflow
// passes both unchanged from run to run.

import { commentOrOpen } from '../lib/github.mjs';

/** Every tracking issue carries this label beside its workflow's own. */
export const FLAKE_LABEL = 'ci-flake';

const FOOTER = [
  'This issue was opened automatically by a scheduled run that',
  'failed. While it stays open, each subsequent scheduled failure',
  'is appended here as a comment linking its own run. Closing it',
  'marks the failure handled — it is never re-opened; the next',
  'scheduled failure opens a fresh issue instead.',
].join('\n');

/** A new issue's body: the workflow's own text, the run, and how the issue works. */
export function issueBody(body, runUrl) {
  return `${body.trimEnd()}\n\nRun: ${runUrl}\n\n${FOOTER}`;
}

/** The comment each later failure adds. */
export function commentBody(runUrl) {
  return `Failed again — run: ${runUrl}`;
}

/**
 * Comments on the open issue labelled `label` and titled exactly `title`,
 * or opens one with `body` (issueBody), labelled `label` and FLAKE_LABEL
 * (commentOrOpen). Returns the issue's number and whether it was opened now.
 */
export async function trackFailure({ api, repo, token, title, label, body, runUrl, ...options }) {
  return commentOrOpen({
    api,
    repo,
    token,
    label,
    title,
    body: issueBody(body, runUrl),
    comment: commentBody(runUrl),
    labels: [label, FLAKE_LABEL],
    ...options,
  });
}
