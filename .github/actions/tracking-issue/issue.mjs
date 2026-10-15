// A scheduled workflow's tracking issue: opened when a scheduled run fails,
// commented on by each later failure while it stays open. Plan item J7 in
// docs/plans/ci-shell-to-code.md.
//
// Only OPEN issues are searched. Closing the issue marks the failure
// handled, so the next failure opens a fresh issue rather than resurrecting
// the closed one. Others find the issue by its exact title and label
// (refresh-astap-shas.yml links its rotation PR to install-astap's), so a
// workflow passes both unchanged from run to run.

import { HttpError, fetchWithRetry, io } from '../lib/helpers.mjs';

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
 * The API endpoint, the repository and the run's link, from the variables
 * the runner sets for every step.
 */
export function runContext(env = process.env) {
  const get = (name) => {
    if (!env[name]) throw new Error(`${name} is not set; the runner sets it for every step`);
    return env[name];
  };
  const repo = get('GITHUB_REPOSITORY');
  return {
    api: get('GITHUB_API_URL'),
    repo,
    runUrl: `${get('GITHUB_SERVER_URL')}/${repo}/actions/runs/${get('GITHUB_RUN_ID')}`,
  };
}

// A request to the GitHub API. Its JSON error says why it refused, which
// HttpError keeps out of its message; GitHub's own answer is safe to show.
async function github(what, url, { token, json, ...options }) {
  const headers = {
    accept: 'application/vnd.github+json',
    authorization: `Bearer ${token}`,
    'user-agent': 'rusty-photon-tracking-issue',
    'x-github-api-version': '2022-11-28',
  };
  if (json !== undefined) headers['content-type'] = 'application/json';
  try {
    return await fetchWithRetry(url, {
      ...options,
      headers,
      ...(json !== undefined && { method: 'POST', body: JSON.stringify(json) }),
    });
  } catch (err) {
    const answer = err instanceof HttpError && err.body ? ` (GitHub answered: ${err.body})` : '';
    throw new Error(`${what} failed${answer}`, { cause: err });
  }
}

// The next page's URL from a Link header (RFC 8288), which is how the
// GitHub API pages a list.
function nextPage(link) {
  return link?.match(/<([^>]+)>\s*;\s*rel="next"/)?.[1];
}

/**
 * The open issues labelled `label`, every page. The issues endpoint lists
 * pull requests too; they are left out. A next page on another origin than
 * `api` is refused, since the token goes with each request.
 */
export async function openIssues({ api, repo, label, token, ...options }) {
  const what = `Listing the open issues labelled ${label}`;
  const query = new URLSearchParams({ state: 'open', labels: label, per_page: '100' });
  const issues = [];
  for (let url = `${api}/repos/${repo}/issues?${query}`; url; ) {
    const response = await github(what, url, { token, ...options });
    issues.push(...(await response.json()).filter((item) => !item.pull_request));
    url = nextPage(response.headers.get('link'));
    if (url && new URL(url).origin !== new URL(api).origin) {
      throw new Error(`${what} failed: the next page is on ${new URL(url).origin}, not the API's origin`);
    }
  }
  return issues;
}

/**
 * Comments on the open issue labelled `label` and titled exactly `title`,
 * or opens one with `body` (issueBody), labelled `label` and FLAKE_LABEL.
 * Neither POST is repeated on failure, since a repeated one that GitHub had
 * acted on would open a second issue. An issue opened without `label`
 * throws. Returns the issue's number and whether it was opened now.
 */
export async function trackFailure({ api, repo, token, title, label, body, runUrl, ...options }) {
  const match = (await openIssues({ api, repo, label, token, ...options })).find((i) => i.title === title);
  if (match) {
    await github(`Commenting on #${match.number}`, `${api}/repos/${repo}/issues/${match.number}/comments`, {
      token,
      json: { body: commentBody(runUrl) },
      ...options,
    });
    io.stdout(`Commented on #${match.number}: ${match.html_url}\n`);
    return { number: match.number, created: false };
  }
  const response = await github(`Opening the issue "${title}"`, `${api}/repos/${repo}/issues`, {
    token,
    json: { title, body: issueBody(body, runUrl), labels: [label, FLAKE_LABEL] },
    ...options,
  });
  const issue = await response.json();
  io.stdout(`Opened #${issue.number}: ${issue.html_url}\n`);
  // GitHub opens the issue without a label that does not exist in the
  // repository, and says nothing. The next failure would not find it, and
  // would open another, every time.
  if (!issue.labels?.some((l) => l.name === label)) {
    throw new Error(
      `#${issue.number} was opened without the label ${label}, which does not exist in the repository, so the next failure would open another issue: create the label`,
    );
  }
  return { number: issue.number, created: true };
}
