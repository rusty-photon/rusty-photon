// The last job of the refresh of install-astap's pins, once every detect job
// (detect.mjs) has finished. When every new binary that was checked passed,
// it proposes the new pins in a pull request from BRANCH, which each run
// replaces; when one failed, it opens or comments on the blocker issue
// instead. Either way it comments on install-astap's open tracking issue. It
// reads main's table and writes the commit through the GitHub API, so the job
// holds no checkout to push from. Plan item J8 in
// docs/plans/ci-shell-to-code.md.

import { readdir, readFile } from 'node:fs/promises';
import path from 'node:path';

import { formatTable, parseTable } from '../install-astap/astap.mjs';
import { commentOrOpen, github, openIssues } from '../lib/github.mjs';
import { io } from '../lib/helpers.mjs';
import { forRunner } from '../lib/install.mjs';
import { FLAKE_LABEL } from '../tracking-issue/issue.mjs';

const SHA256 = /^[0-9a-f]{64}$/;

/** The table's path in the repository, which the commit rewrites. */
export const TABLE_PATH = '.github/actions/install-astap/assets.json';

/** The branch each run's proposal replaces. */
export const BRANCH = 'chore/auto-refresh-astap-shas';

/** The proposal's title, and its commit's subject. */
export const TITLE = 'chore(install-astap): auto-refresh SHA-256 pins';

const COMMIT_MESSAGE = `${TITLE}

Upstream rotated SourceForge archives. The refresh-astap-shas
workflow verified each new binary by solving the committed
M101 fixture (services/plate-solver/tests/fixtures/m101_known.fits)
to RA 210.8099° / Dec 54.3469° within 0.01° tolerance, then
patched the pins in .github/actions/install-astap/assets.json.

See ADR-005 §"Pinned SHA-256 + refresh procedure" for the
manual procedure this workflow automates.
`;

/** The blocker a failed check opens, found again by its label and exact title. */
export const BLOCKER = { label: 'astap-refresh-blocked', title: 'astap: upstream rotated but new bytes fail M101 verification' };

/** install-astap's tracking issue, as its notify-on-failure job opens it. */
export const TRACKER = { label: 'install-astap-nightly', title: 'nightly: install-astap smoke failing (likely upstream SHA rotation)' };

const VERIFIED = new Set(['passed', 'failed', 'skipped']);

// Throws unless `result`, from the artifact `where`, is one detect writes.
function checkResult(result, where) {
  for (const field of ['runner', 'archive', 'banner']) {
    if (typeof result?.[field] !== 'string') throw new Error(`${where}: no ${field}`);
  }
  for (const field of ['pinned', 'upstream']) {
    if (!SHA256.test(result[field] ?? '')) throw new Error(`${where}: ${field}, ${JSON.stringify(result[field])}, is not a SHA-256 in lowercase hex`);
  }
  if (!VERIFIED.has(result.verified)) throw new Error(`${where}: verified is ${JSON.stringify(result.verified)}, not one of ${[...VERIFIED].join(', ')}`);
}

/**
 * The results detect wrote, read from `dir` as download-artifact lays them
 * out, a directory per artifact, in the order of their names. None when
 * download-artifact made no `dir`, as when no archive rotated.
 */
export async function readResults(dir) {
  let artifacts;
  try {
    artifacts = (await readdir(dir, { withFileTypes: true })).filter((entry) => entry.isDirectory()).map((entry) => entry.name);
  } catch (err) {
    if (err.code === 'ENOENT') return [];
    throw err;
  }
  const results = [];
  for (const artifact of artifacts.sort()) {
    const files = (await readdir(path.join(dir, artifact))).filter((name) => name.endsWith('.json')).sort();
    for (const file of files) {
      const result = JSON.parse(await readFile(path.join(dir, artifact, file), 'utf8'));
      checkResult(result, `${artifact}/${file}`);
      results.push(result);
    }
  }
  return results;
}

/**
 * `table` with each result's new pin in it, and what changed: a pin already
 * at its new SHA-256 is no change. A result's archive must be the one the
 * table names for its runner.
 */
export function repin(table, results) {
  const next = structuredClone(table);
  const changes = [];
  for (const { runner, archive, upstream } of results) {
    const entry = forRunner(next.cli, runner, 'ASTAP');
    if (entry.archive !== archive) {
      throw new Error(`the table names ${entry.archive} for ${runner}, not ${archive}, which the refresh downloaded`);
    }
    if (entry.sha256 === upstream) continue;
    changes.push({ runner, archive, from: entry.sha256, to: upstream });
    entry.sha256 = upstream;
  }
  return { table: next, changes };
}

/**
 * A result as a line of the proposal or the blocker. The banner is what the
 * new binary printed, so it goes in a code span, without the backticks that
 * would end one.
 */
export function row({ runner, banner, pinned, upstream, verified, problem }) {
  const why = problem === undefined ? '' : `: ${problem}`;
  return `- **${runner}** (\`${banner.replace(/`/g, '')}\`): \`${pinned.slice(0, 8)}…\` → \`${upstream.slice(0, 8)}…\` — M101 verify: **${verified}**${why}`;
}

/** The proposal's body. */
export function proposalBody(rows, runUrl) {
  return `Auto-refresh detected upstream rotation of ASTAP CLI archives.

## Per-platform results

${rows.join('\n')}

## Verification

Each rotated platform's new binary was executed against
\`services/plate-solver/tests/fixtures/m101_known.fits\` and
asserted to solve to RA 210.8099° / Dec 54.3469° within 0.01°
tolerance (matching \`real_astap_smoke.feature\`). The
\`degenerate_no_stars.fits\` failure-mode fixture was also
checked.

Windows-ARM64 has no GH-hosted runner, so its SHA is refreshed
cross-mirror with no verification — same posture ADR-005
documents.

Full verification logs:
${runUrl}

## PR CI

\`pull_request\`-triggered workflows (\`install-astap\`,
\`plate-solver-smoke\`, etc.) do not run on this PR. GitHub
Actions suppresses workflows triggered by GITHUB_TOKEN-authored
events, and this PR was opened by GITHUB_TOKEN — so the M101
verification above (run on this workflow's own runners) is the
only ASTAP validation that runs before merge.

To wake PR CI for a redundant signal, close and reopen this
PR — a human-authored \`pull_request: reopened\` event is not
suppressed.
`;
}

/** The blocker's body. */
export function blockerBody(rows, runUrl) {
  return `Auto-refresh detected upstream SHA rotation, but the new ASTAP binary failed to solve the M101 fixture on at least one platform. Manual investigation required — do **not** merge a refresh PR until this is understood.

## Per-platform results

${rows.join('\n')}

## Likely causes

- Upstream shipped a broken release
- CLI surface changed (e.g. \`-wcs\` or \`-d\` flags renamed)
- \`.wcs\` sidecar format changed (CRVAL keys renamed, PLTSOLVD card removed)
- New binary requires a newer D05 database

Workflow run: ${runUrl}`;
}

// The JSON answer to a request of the GitHub API (github), or undefined for a
// 404 when `missing` allows one.
async function answer(what, url, options) {
  const response = await github(what, url, options);
  return response === undefined ? undefined : response.json();
}

// The writes a repeat leaves as they were, which fetchWithRetry may send
// again after a 5xx or no answer: a tree or commit is named by its content,
// so a second POST of one makes the same object, and a PATCH sets the ref
// or the body to the same value again. Creating the branch, opening the pull
// request and commenting are sent once: a repeat of one GitHub had acted on
// would fail on the branch it made, or make a second of the others.
const REPEATABLE = { attempts: 3 };

// The pull request from BRANCH with `body`, its branch at a new commit on
// `base` that changes the table to `table`: the open one updated, or one
// opened. Resolves to its link.
async function propose({ api, repo, token, base, tree, table, body, open, branch, ...options }) {
  const repos = `${api}/repos/${repo}`;
  const madeTree = await answer('Writing the new table', `${repos}/git/trees`, {
    token,
    json: { base_tree: tree, tree: [{ path: TABLE_PATH, mode: '100644', type: 'blob', content: formatTable(table) }] },
    ...REPEATABLE,
    ...options,
  });
  const commit = await answer('Committing the new table', `${repos}/git/commits`, {
    token,
    json: { message: COMMIT_MESSAGE, tree: madeTree.sha, parents: [base] },
    ...REPEATABLE,
    ...options,
  });
  io.stdout(`Committed ${commit.sha} on ${base}\n`);
  // The commit replaces whatever the branch held, as each run's proposal
  // replaces the last one's.
  if (branch === undefined) {
    await github(`Creating ${BRANCH}`, `${repos}/git/refs`, { token, json: { ref: `refs/heads/${BRANCH}`, sha: commit.sha }, ...options });
  } else {
    await github(`Moving ${BRANCH}`, `${repos}/git/refs/heads/${BRANCH}`, {
      token,
      method: 'PATCH',
      json: { sha: commit.sha, force: true },
      ...REPEATABLE,
      ...options,
    });
  }
  if (open !== undefined) {
    await github(`Updating #${open.number}`, `${repos}/pulls/${open.number}`, {
      token,
      method: 'PATCH',
      json: { body },
      ...REPEATABLE,
      ...options,
    });
    io.stdout(`Updated #${open.number}: ${open.html_url}\n`);
    return open.html_url;
  }
  const pr = await answer('Opening the pull request', `${repos}/pulls`, {
    token,
    json: { title: TITLE, head: BRANCH, base: 'main', body },
    ...options,
  });
  io.stdout(`Opened #${pr.number}: ${pr.html_url}\n`);
  return pr.html_url;
}

// Comments `message` on install-astap's open tracking issue, if one is open:
// a closed one has been handled, and the proposal or the blocker stands on
// its own.
async function tellTracker({ api, repo, token, runUrl, message, dryRun, ...options }) {
  const issue = (await openIssues({ api, repo, label: TRACKER.label, token, ...options })).find((i) => i.title === TRACKER.title);
  if (issue === undefined) {
    io.stdout("install-astap's tracking issue is not open, so it gets no comment\n");
    return;
  }
  const body = `${message}\n\nRun: ${runUrl}`;
  if (dryRun) {
    io.stdout(`Would comment on #${issue.number}:\n${body}\n`);
    return;
  }
  await github(`Commenting on #${issue.number}`, `${api}/repos/${repo}/issues/${issue.number}/comments`, { token, json: { body }, ...options });
  io.stdout(`Commented on #${issue.number}: ${issue.html_url}\n`);
}

/**
 * Acts on what the detect jobs found, `results` (readResults), in `repo`
 * through the GitHub API at `api` with `token`, the run being `runUrl`.
 * Nothing rotated: nothing to do. A check failed: the blocker
 * (commentOrOpen). Otherwise: the new pins on main's table in the pull
 * request from BRANCH, unless main already pins them. Then a comment on
 * install-astap's tracking issue. With `dryRun`, it reads what it would act
 * on and prints what it would write, writing nothing. Resolves to what it
 * did: `{ outcome }`, 'none', 'blocked', 'current' or 'proposed', with the
 * proposal's `url` (none in a dry run).
 */
export async function publish({ api, repo, token, runUrl, results, dryRun = false, ...options }) {
  if (results.length === 0) {
    io.stdout('No archive rotated: nothing to do\n');
    return { outcome: 'none' };
  }
  const rows = results.map(row);
  io.stdout(`${rows.join('\n')}\n`);
  const context = { api, repo, token, ...options };

  if (results.some(({ verified }) => verified === 'failed')) {
    const body = blockerBody(rows, runUrl);
    let blocker = `the open issue labelled \`${BLOCKER.label}\``;
    if (dryRun) {
      io.stdout(`Would open or comment on "${BLOCKER.title}":\n${body}\n`);
    } else {
      const { number } = await commentOrOpen({
        ...context,
        ...BLOCKER,
        body,
        comment: `Failed again — see ${runUrl}`,
        labels: [BLOCKER.label, FLAKE_LABEL],
      });
      blocker = `#${number}`;
    }
    await tellTracker({
      ...context,
      runUrl,
      dryRun,
      message: `Auto-refresh workflow detected rotation but M101 verification failed on at least one platform. See ${blocker}.`,
    });
    return { outcome: 'blocked' };
  }

  const repos = `${api}/repos/${repo}`;
  const base = (await answer('Reading main', `${repos}/git/ref/heads/main`, context)).object.sha;
  const { tree } = await answer(`Reading main's commit ${base}`, `${repos}/git/commits/${base}`, context);
  const file = await answer(`Reading ${TABLE_PATH} on main`, `${repos}/contents/${TABLE_PATH}?ref=${base}`, context);
  if (file.encoding !== 'base64') throw new Error(`GitHub sent ${TABLE_PATH} as ${JSON.stringify(file.encoding)}, not base64`);
  const { table, changes } = repin(parseTable(Buffer.from(file.content, 'base64').toString('utf8'), `${TABLE_PATH} on main`), results);
  if (changes.length === 0) {
    io.stdout(`main (${base}) already pins what upstream serves: nothing to propose\n`);
    return { outcome: 'current' };
  }
  for (const { runner, archive, from, to } of changes) io.stdout(`${runner} (${archive}): ${from} -> ${to}\n`);

  const branch = await answer(`Reading ${BRANCH}`, `${repos}/git/ref/heads/${BRANCH}`, { ...context, missing: true });
  const owner = repo.split('/')[0];
  const query = new URLSearchParams({ state: 'open', head: `${owner}:${BRANCH}`, base: 'main' });
  const [open] = await answer(`Listing the open pull requests from ${BRANCH}`, `${repos}/pulls?${query}`, context);
  const body = proposalBody(rows, runUrl);
  if (dryRun) {
    const target = open === undefined ? 'a new pull request' : `#${open.number}`;
    io.stdout(
      `Would commit ${TABLE_PATH} on ${base}, ${branch === undefined ? 'create' : 'move'} ${BRANCH} to it, ` +
        `and give ${target} this body:\n${body}`,
    );
    await tellTracker({ ...context, runUrl, dryRun, message: `Auto-refresh workflow would update ${target} with new SHAs.` });
    return { outcome: 'proposed' };
  }
  const url = await propose({ ...context, base, tree: tree.sha, table, body, open, branch });
  await tellTracker({
    ...context,
    runUrl,
    message: `Auto-refresh workflow opened ${url} with new SHAs (M101 verified on all runner-backed platforms). Review and merge to close this.`,
  });
  return { outcome: 'proposed', url };
}
