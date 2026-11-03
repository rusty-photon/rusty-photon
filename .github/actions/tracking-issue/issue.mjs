// A scheduled workflow's tracking issue: opened when a scheduled run fails,
// commented on by each later failure while it stays open. Plan item J7 in
// docs/plans/archive/ci-shell-to-code.md.
//
// Only OPEN issues are searched. Closing the issue marks the failure
// handled, so the next failure opens a fresh issue rather than resurrecting
// the closed one. Others find the issue by its exact title and label
// (refresh-astap links its rotation PR to install-astap's), so a workflow
// passes both unchanged from run to run.
//
// A notifier that needs a whole matrix runs when any leg failed. Given
// `job`, the name of the one leg it watches, it files only if that leg
// failed in this run attempt.

import { commentOrOpen, everyPage } from '../lib/github.mjs';
import { io } from '../lib/helpers.mjs';

/** Every tracking issue carries this label beside its workflow's own. */
export const FLAKE_LABEL = 'ci-flake';

/** A job's conclusions that file: one past its timeout-minutes concludes cancelled. */
const FAILED = new Set(['failure', 'cancelled', 'timed_out']);

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

/** This run's id and attempt, from the variables the runner sets for every step. */
export function runAttempt(env = process.env) {
  const get = (name) => {
    if (!env[name]) throw new Error(`${name} is not set; the runner sets it for every step`);
    return env[name];
  };
  return { runId: get('GITHUB_RUN_ID'), attempt: get('GITHUB_RUN_ATTEMPT') };
}

/**
 * The conclusion of the job named exactly `job` in attempt `attempt` of run
 * `runId`. A job the attempt lacks, or one still running, throws: a renamed
 * leg, or a notifier that does not need it, must not quietly file nothing.
 */
async function jobConclusion({ api, repo, token, runId, attempt, job, ...options }) {
  const jobs = await everyPage(
    `Listing the jobs of run ${runId}, attempt ${attempt}`,
    `${api}/repos/${repo}/actions/runs/${runId}/attempts/${attempt}/jobs?per_page=100`,
    { token, key: 'jobs', ...options },
  );
  const found = jobs.find((j) => j.name === job);
  if (!found) {
    const names = jobs.map((j) => `"${j.name}"`).join(', ');
    throw new Error(`Run ${runId}, attempt ${attempt} has no job named "${job}" (its jobs: ${names}): name the job as the run does`);
  }
  if (found.status !== 'completed') {
    throw new Error(`"${job}" has not finished (${found.status}): the job that files must need it`);
  }
  return found.conclusion;
}

/**
 * Comments on the open issue labelled `label` and titled exactly `title`,
 * or opens one with `body` (issueBody), labelled `label` and FLAKE_LABEL
 * (commentOrOpen). Returns the issue's number and whether it was opened now.
 * Given `job`, files only if that job of the run attempt (`runId`,
 * `attempt`) concluded failure, cancelled or timed_out, and otherwise says
 * why and returns undefined.
 */
export async function trackFailure({ api, repo, token, title, label, body, runUrl, job, runId, attempt, ...options }) {
  if (job) {
    const conclusion = await jobConclusion({ api, repo, token, runId, attempt, job, ...options });
    if (!FAILED.has(conclusion)) {
      io.stdout(`"${job}" concluded ${conclusion}, so no issue was opened or commented on\n`);
      return undefined;
    }
    io.stdout(`"${job}" concluded ${conclusion}\n`);
  }
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
