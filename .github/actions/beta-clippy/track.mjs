// check.yml's nightly beta clippy, after its census (report.mjs): one open
// issue per lint the census lists, its body rewritten each run, and an issue
// closed once its lint is no longer reported, so a lint new on beta is fixed
// on our schedule before the toolchain enforcing it reaches stable.
// Conventions: docs/skills/ci-logic.md.

import { readFileSync } from 'node:fs';

import { github, openIssues } from '../lib/github.mjs';
import { fail, io } from '../lib/helpers.mjs';

/** The label every issue this job opens carries, which finds them again. */
export const LABEL = 'beta-clippy';

/** The start of each issue's title; the lint's name follows it. */
export const PREFIX = 'beta clippy: ';

// A toolchain that renames or splits many lints at once would open a flood of
// issues, and tracking only some of them would close the issues of the rest.
// Past this many lints the step fails instead, for someone to triage the
// census in the run's summary; on a schedule, notify-clippy-failure says so.
export const MAX_LINTS = 20;

// The sites an issue lists; the census keeps more, and the count is the total.
const SHOWN_SITES = 20;

// The writes a repeat leaves as they were, which fetchWithRetry may send again
// after a 5xx or no answer: a PATCH sets the body or the state to the same
// value again. Creating an issue or a label and commenting are sent once: a
// repeat of one GitHub had acted on would make a second, or fail on the first.
const REPEATABLE = { attempts: 3 };

/** The census in the file `file`, as beta_clippy_census.py writes it; throws for anything else. */
export function readCensus(file) {
  const census = JSON.parse(readFileSync(file, 'utf8'));
  if (typeof census?.toolchain !== 'string') throw new Error(`${file}: no toolchain`);
  if (!Array.isArray(census.lints)) throw new Error(`${file}: no list of lints`);
  for (const entry of census.lints) {
    if (typeof entry?.lint !== 'string' || entry.lint === '') throw new Error(`${file}: a lint with no name: ${JSON.stringify(entry)}`);
    if (!Number.isInteger(entry.count) || !Array.isArray(entry.sites)) {
      throw new Error(`${file}: ${entry.lint} has no count of sites or no list of them`);
    }
  }
  return census;
}

/** The issue's body for the census's lint `entry`, seen in the run `runUrl`. */
export function issueBody(entry, toolchain, runUrl) {
  const shown = entry.sites.slice(0, SHOWN_SITES);
  const lines = [
    `\`${entry.lint}\` fires **${entry.count}×** under the beta toolchain.`,
    ``,
    `\`stable / clippy\` is a required gate running \`-D warnings\`, so`,
    `\`main\` is silent on stable — this lint is new on the beta channel`,
    `and will start failing that gate when beta becomes stable. Fixing`,
    `it in current code now avoids a rushed sweep on the bump.`,
    ``,
    `> ${entry.example}`,
    ``,
  ];
  if (entry.docs) lines.push(`Lint reference: ${entry.docs}`, ``);
  lines.push(
    `**Sites** (showing ${shown.length} of ${entry.count}):`,
    ``,
    ...shown.map((site) => `- \`${site}\``),
    ``,
    `Toolchain: \`${toolchain}\``,
    `Last seen: ${runUrl}`,
    ``,
    `Opened automatically by the nightly \`beta / clippy\` job. This`,
    `body is rewritten on every run, and the issue closes itself once`,
    `the lint stops being reported.`,
  );
  return lines.join('\n');
}

// LABEL, created when the repository lacks it: GitHub opens an issue without
// a label that does not exist, and says nothing, and the next run would not
// find that issue.
async function ensureLabel({ api, repo, token, ...options }) {
  const found = await github(`Reading the label ${LABEL}`, `${api}/repos/${repo}/labels/${encodeURIComponent(LABEL)}`, {
    token,
    missing: true,
    ...options,
  });
  if (found !== undefined) return;
  await github(`Creating the label ${LABEL}`, `${api}/repos/${repo}/labels`, {
    token,
    json: { name: LABEL, color: 'ededed', description: 'a lint beta clippy reports that stable does not yet' },
    ...options,
  });
  io.stdout(`Created the label ${LABEL}\n`);
}

/**
 * The action: brings the open issues labelled LABEL in `repo`, through the
 * GitHub API at `api` with `token`, in line with `census` (readCensus), seen
 * in the run `runUrl`. A lint's issue gets the census's sites, or is opened
 * with LABEL and check-nightly; an issue titled PREFIX and a lint the census
 * no longer lists gets a comment and is closed. A census of more than
 * MAX_LINTS lints fails the step and touches nothing. Resolves to the issues'
 * numbers, `{ updated, opened, closed }`.
 */
export async function track({ api, repo, token, runUrl, census, ...options }) {
  const done = { updated: [], opened: [], closed: [] };
  if (census.lints.length > MAX_LINTS) {
    fail(
      `beta clippy reported ${census.lints.length} distinct lints (cap ${MAX_LINTS}); ` +
        'triage the census in the run summary rather than opening an issue per lint.',
    );
    return done;
  }
  const context = { api, repo, token, ...options };
  const issues = `${api}/repos/${repo}/issues`;
  await ensureLabel(context);
  const open = await openIssues({ ...context, label: LABEL });
  const existing = new Map(open.map((issue) => [issue.title, issue]));
  const reported = new Set();
  for (const entry of census.lints) {
    const title = `${PREFIX}${entry.lint}`;
    reported.add(title);
    const body = issueBody(entry, census.toolchain, runUrl);
    const match = existing.get(title);
    if (match !== undefined) {
      await github(`Updating #${match.number}`, `${issues}/${match.number}`, { token, method: 'PATCH', json: { body }, ...REPEATABLE, ...options });
      io.stdout(`updated #${match.number} — ${title} (${entry.count} sites)\n`);
      done.updated.push(match.number);
    } else {
      const response = await github(`Opening the issue "${title}"`, issues, { token, json: { title, body, labels: [LABEL, 'check-nightly'] }, ...options });
      const { number } = await response.json();
      io.stdout(`opened #${number} — ${title} (${entry.count} sites)\n`);
      done.opened.push(number);
    }
  }
  // A lint no longer reported was fixed here or dropped upstream. Only the
  // issues this job opens are closed.
  for (const issue of open) {
    if (reported.has(issue.title) || !issue.title.startsWith(PREFIX)) continue;
    await github(`Commenting on #${issue.number}`, `${issues}/${issue.number}/comments`, {
      token,
      json: { body: `No longer reported by \`beta / clippy\` — closing.\n\nRun: ${runUrl}` },
      ...options,
    });
    await github(`Closing #${issue.number}`, `${issues}/${issue.number}`, {
      token,
      method: 'PATCH',
      json: { state: 'closed', state_reason: 'completed' },
      ...REPEATABLE,
      ...options,
    });
    io.stdout(`closed #${issue.number} — ${issue.title}\n`);
    done.closed.push(issue.number);
  }
  return done;
}
