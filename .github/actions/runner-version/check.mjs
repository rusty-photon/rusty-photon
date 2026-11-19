// Whether the runner software in the self-hosted pool's templates is falling
// behind actions/runner's releases. The pool's just-in-time runners cannot
// update themselves, and GitHub stops queueing jobs to a runner that has not
// taken an available update within 30 days, so the templates are rebuilt
// before then (docs/skills/proxmox-runner-pool.md §Runner releases).
// runner-version.yml runs it daily.

import { github } from '../lib/github.mjs';

/**
 * How long after a release GitHub keeps queueing jobs to a runner that has
 * not taken it: "If you do not perform a software update within 30 days, the
 * GitHub Actions service will not queue jobs to your runner" (GitHub's docs,
 * "Controlling runner software updates on self-hosted runners").
 */
export const DEADLINE_DAYS = 30;

const DAY_MS = 24 * 60 * 60 * 1000;
const VERSION = /^(\d+)\.(\d+)\.(\d+)$/;

/**
 * The version `text` holds: one `major.minor.patch` line, as
 * tools/ci/runner-guest/RUNNER_VERSION records the templates' runner.
 */
export function parseVersion(text) {
  const version = text.trim();
  if (!VERSION.test(version)) {
    throw new Error(`the runner version file holds ${JSON.stringify(version)}, not one major.minor.patch line`);
  }
  return version;
}

/**
 * How many days after a release the check fails: a whole number from 0 to
 * DEADLINE_DAYS - 1, so the failure always comes before GitHub's deadline.
 */
export function parseGraceDays(text) {
  const days = /^\d+$/.test(text) ? Number(text) : NaN;
  if (!(days < DEADLINE_DAYS)) {
    throw new Error(`grace-days is ${JSON.stringify(text)}, not a whole number from 0 to ${DEADLINE_DAYS - 1}`);
  }
  return days;
}

/** Negative, zero or positive as version `a` is older than, the same as, or newer than `b`. */
export function compareVersions(a, b) {
  const pa = a.match(VERSION).slice(1).map(Number);
  const pb = b.match(VERSION).slice(1).map(Number);
  for (let i = 0; i < 3; i += 1) {
    if (pa[i] !== pb[i]) return pa[i] - pb[i];
  }
  return 0;
}

/**
 * The releases of actions/runner that are made available to runners, as
 * `{ version, published }`: the first page of the list, which reaches months
 * back, without drafts, pre-releases or a tag that is not `v` and a version.
 */
export async function releases({ api, token }) {
  const response = await github('listing the releases of actions/runner', `${api}/repos/actions/runner/releases?per_page=30`, { token });
  const answer = await response.json();
  return answer
    .filter((release) => !release.draft && !release.prerelease && /^v\d+\.\d+\.\d+$/.test(release.tag_name))
    .map((release) => ({ version: release.tag_name.slice(1), published: new Date(release.published_at) }));
}

/**
 * Where the templates' runner `pinned` stands against the `published`
 * releases at `now`. Current when no release is newer. Otherwise behind:
 * GitHub's 30 days run from the first release newer than `pinned`, so that
 * release sets the deadline, and the check is due `graceDays` after it.
 */
export function assess({ pinned, published, now, graceDays }) {
  const newer = published.filter((release) => compareVersions(release.version, pinned) > 0);
  if (newer.length === 0) return { status: 'current', pinned };
  const first = newer.reduce((a, b) => (b.published < a.published ? b : a));
  const latest = newer.reduce((a, b) => (compareVersions(b.version, a.version) > 0 ? b : a));
  return {
    status: 'behind',
    pinned,
    first,
    latest,
    deadline: new Date(first.published.getTime() + DEADLINE_DAYS * DAY_MS),
    due: now.getTime() >= first.published.getTime() + graceDays * DAY_MS,
  };
}

/** What the check reports about `assessment`. */
export function report(assessment) {
  const day = (date) => date.toISOString().slice(0, 10);
  if (assessment.status === 'current') {
    return `The pool templates' runner ${assessment.pinned} is the newest release of actions/runner.`;
  }
  const { pinned, first, latest, deadline } = assessment;
  return (
    `The pool templates' runner ${pinned} is behind actions/runner ${latest.version}. ` +
    `${first.version} was released on ${day(first.published)}, so GitHub stops queueing jobs to the pool's ` +
    `runners from about ${day(deadline)}. Rebuild both templates with ${latest.version} and update ` +
    'tools/ci/runner-guest/RUNNER_VERSION (docs/skills/proxmox-runner-pool.md §Runner releases).'
  );
}
