// The GitHub REST API, for the actions that call it (tracking-issue,
// refresh-astap, uncovered-diff-lines, install-conformu, beta-clippy): the
// run's context, a request that says what failed with
// GitHub's own answer, the open issues with a label, and an issue that a
// failure comments on or opens. Conventions: docs/skills/ci-logic.md.

import { HttpError, fetchWithRetry, io } from './helpers.mjs';

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

/**
 * The response to a request of `url` with `token`: a GET, or with `json` a
 * POST of it, or `method` with it. Its JSON error says why it refused, which
 * HttpError keeps out of its message; GitHub's own answer is safe to show, so
 * the error says `what` failed and carries it. With `missing`, a 404 resolves
 * to undefined. With `accept`, the answer comes in that media type, such as a
 * pull request's diff (`application/vnd.github.diff`). A POST or PATCH is
 * sent once (fetchWithRetry). Without a token the request goes
 * unauthenticated, as a developer's install reads a public repository.
 */
export async function github(what, url, { token, json, method, accept = 'application/vnd.github+json', missing = false, ...options }) {
  const headers = {
    accept,
    ...(token && { authorization: `Bearer ${token}` }),
    'user-agent': 'rusty-photon-actions',
    'x-github-api-version': '2022-11-28',
  };
  if (json !== undefined) headers['content-type'] = 'application/json';
  try {
    return await fetchWithRetry(url, {
      ...options,
      headers,
      ...(json !== undefined && { method: method ?? 'POST', body: JSON.stringify(json) }),
    });
  } catch (err) {
    if (missing && err instanceof HttpError && err.status === 404) return undefined;
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
 * Comments `comment` on the open issue labelled `label` and titled exactly
 * `title`, or opens one with `body` and `labels`, `label` among them. Only
 * open issues are searched: closing one marks it handled, so the next
 * failure opens a fresh one. Neither POST is repeated on failure, since a
 * repeated one that GitHub had acted on would open a second issue. An issue
 * opened without `label` throws. Returns the issue's number and whether it
 * was opened now.
 */
export async function commentOrOpen({ api, repo, token, label, title, body, comment, labels, ...options }) {
  const match = (await openIssues({ api, repo, label, token, ...options })).find((i) => i.title === title);
  if (match) {
    await github(`Commenting on #${match.number}`, `${api}/repos/${repo}/issues/${match.number}/comments`, {
      token,
      json: { body: comment },
      ...options,
    });
    io.stdout(`Commented on #${match.number}: ${match.html_url}\n`);
    return { number: match.number, created: false };
  }
  const response = await github(`Opening the issue "${title}"`, `${api}/repos/${repo}/issues`, {
    token,
    json: { title, body, labels },
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
