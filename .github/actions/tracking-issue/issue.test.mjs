import assert from 'node:assert/strict';
import test from 'node:test';

import { describe } from '../lib/helpers.mjs';
import { capture, serve } from '../lib/testing.mjs';
import { runContext, trackFailure } from './issue.mjs';

// --- Fixtures -----------------------------------------------------------------

const TITLE = 'nightly: check clippy failing';
const RUN = 'https://github.example/o/r/actions/runs/123';

function json(res, status, value, headers = {}) {
  res.writeHead(status, { 'content-type': 'application/json', ...headers }).end(JSON.stringify(value));
}

// A fake GitHub API for repository o/r. Its issue list answers `open`,
// `pageSize` at a time, with a Link header to the next page. Opening an
// issue answers `create(res)`, by default issue 42; a comment answers 201.
async function fakeGitHub(open, { pageSize = 100, create } = {}) {
  const server = await serve((req, res) => {
    const url = new URL(req.url, server.url);
    if (req.method === 'GET' && url.pathname === '/repos/o/r/issues') {
      const page = Number(url.searchParams.get('page') ?? '1');
      const link =
        page * pageSize < open.length ? { link: `<${server.url}/repos/o/r/issues?page=${page + 1}>; rel="next"` } : {};
      json(res, 200, open.slice((page - 1) * pageSize, page * pageSize), link);
    } else if (req.method === 'POST' && url.pathname === '/repos/o/r/issues') {
      if (create) create(res);
      else json(res, 201, { number: 42, html_url: 'https://github.example/o/r/issues/42' });
    } else if (req.method === 'POST' && /^\/repos\/o\/r\/issues\/\d+\/comments$/.test(url.pathname)) {
      json(res, 201, { id: 1 });
    } else {
      json(res, 404, { message: 'Not Found' });
    }
  });
  return server;
}

function track(api, options = {}) {
  return capture(() =>
    trackFailure({
      api: api.url,
      repo: 'o/r',
      token: 'tok',
      title: TITLE,
      label: 'check-nightly',
      body: 'A clippy job failed.\n\nSuspect a lint.\n',
      runUrl: RUN,
      delayMs: 1,
      ...options,
    }),
  );
}

const posts = (api) => api.requests.filter((r) => r.method === 'POST');

// --- Opening or commenting ----------------------------------------------------

test('with no open issue, opens one with the run, the note, and both labels', async (t) => {
  const api = await fakeGitHub([]);
  t.after(api.close);
  const { value, stdout } = await track(api);
  assert.deepEqual(value, { number: 42, created: true });
  assert.match(stdout, /^Opened #42: https:\/\/github\.example\/o\/r\/issues\/42$/m);

  const list = new URL(api.requests[0].url, api.url);
  assert.equal(list.pathname, '/repos/o/r/issues');
  assert.deepEqual(Object.fromEntries(list.searchParams), { state: 'open', labels: 'check-nightly', per_page: '100' });
  assert.equal(api.requests[0].headers.authorization, 'Bearer tok');
  assert.equal(api.requests[0].headers.accept, 'application/vnd.github+json');

  const [create] = posts(api);
  assert.equal(create.url, '/repos/o/r/issues');
  assert.equal(create.headers.authorization, 'Bearer tok');
  assert.deepEqual(JSON.parse(create.body), {
    title: TITLE,
    labels: ['check-nightly', 'ci-flake'],
    body: [
      'A clippy job failed.',
      '',
      'Suspect a lint.',
      '',
      `Run: ${RUN}`,
      '',
      'This issue was opened automatically by a scheduled run that',
      'failed. While it stays open, each subsequent scheduled failure',
      'is appended here as a comment linking its own run. Closing it',
      'marks the failure handled — it is never re-opened; the next',
      'scheduled failure opens a fresh issue instead.',
    ].join('\n'),
  });
});

test('the open issue with the exact title gets a comment, and no issue is opened', async (t) => {
  const api = await fakeGitHub([{ number: 7, title: TITLE, html_url: 'https://github.example/o/r/issues/7' }]);
  t.after(api.close);
  const { value, stdout } = await track(api);
  assert.deepEqual(value, { number: 7, created: false });
  assert.match(stdout, /^Commented on #7: https:\/\/github\.example\/o\/r\/issues\/7$/m);
  assert.deepEqual(
    posts(api).map((r) => [r.url, JSON.parse(r.body)]),
    [['/repos/o/r/issues/7/comments', { body: `Failed again — run: ${RUN}` }]],
  );
});

test('an issue whose title only resembles the title is not the tracking issue', async (t) => {
  // clippy-beta's per-lint census issues also carry check-nightly.
  const api = await fakeGitHub([
    { number: 8, title: `${TITLE}: clippy::needless_return` },
    { number: 9, title: 'nightly: check clippy' },
  ]);
  t.after(api.close);
  const { value } = await track(api);
  assert.deepEqual(value, { number: 42, created: true });
  assert.deepEqual(posts(api).map((r) => r.url), ['/repos/o/r/issues']);
});

test('a pull request with the title is not the tracking issue', async (t) => {
  const api = await fakeGitHub([{ number: 5, title: TITLE, pull_request: { url: 'x' } }]);
  t.after(api.close);
  const { value } = await track(api);
  assert.deepEqual(value, { number: 42, created: true });
  assert.deepEqual(posts(api).map((r) => r.url), ['/repos/o/r/issues']);
});

test('the tracking issue is found on a later page', async (t) => {
  const api = await fakeGitHub(
    [
      { number: 1, title: 'one' },
      { number: 2, title: 'two' },
      { number: 3, title: TITLE },
    ],
    { pageSize: 2 },
  );
  t.after(api.close);
  const { value } = await track(api);
  assert.deepEqual(value, { number: 3, created: false });
  assert.equal(api.requests[1].url, '/repos/o/r/issues?page=2');
  assert.equal(api.requests[1].headers.authorization, 'Bearer tok');
  assert.deepEqual(posts(api).map((r) => r.url), ['/repos/o/r/issues/3/comments']);
});

test('a next page on another origin is refused, and the token never goes there', async (t) => {
  const elsewhere = await serve((req, res) => json(res, 200, []));
  t.after(elsewhere.close);
  const api = await serve((req, res) => json(res, 200, [], { link: `<${elsewhere.url}/issues?page=2>; rel="next"` }));
  t.after(api.close);
  await assert.rejects(track(api), (err) => {
    assert.match(describe(err), /^Listing the open issues labelled check-nightly failed: the next page is on http:\/\/127\.0\.0\.1:\d+, not the API's origin$/);
    return true;
  });
  assert.equal(elsewhere.requests.length, 0);
  assert.equal(posts(api).length, 0);
});

// --- Failures -----------------------------------------------------------------

test("a failed open is sent once, and the error carries GitHub's answer", async (t) => {
  const api = await fakeGitHub([], { create: (res) => json(res, 502, { message: 'Server Error' }) });
  t.after(api.close);
  await assert.rejects(track(api), (err) => {
    assert.match(
      describe(err),
      /^Opening the issue "nightly: check clippy failing" failed \(GitHub answered: \{"message":"Server Error"\}\): POST http:\/\/127\.0\.0\.1:\d+\/repos\/o\/r\/issues: 502 Bad Gateway$/,
    );
    return true;
  });
  assert.equal(posts(api).length, 1, 'a repeated POST could open a second issue');
});

test('a failed list names what failed', async (t) => {
  const api = await serve((req, res) => json(res, 404, { message: 'Not Found' }));
  t.after(api.close);
  await assert.rejects(track(api), (err) => {
    assert.match(describe(err), /^Listing the open issues labelled check-nightly failed \(GitHub answered: \{"message":"Not Found"\}\): GET /);
    return true;
  });
});

// --- The run's context --------------------------------------------------------

test("runContext reads the API, the repository and the run's link", () => {
  const env = {
    GITHUB_API_URL: 'https://api.github.com',
    GITHUB_REPOSITORY: 'o/r',
    GITHUB_SERVER_URL: 'https://github.com',
    GITHUB_RUN_ID: '123',
  };
  assert.deepEqual(runContext(env), {
    api: 'https://api.github.com',
    repo: 'o/r',
    runUrl: 'https://github.com/o/r/actions/runs/123',
  });
  assert.throws(() => runContext({ ...env, GITHUB_RUN_ID: '' }), /^Error: GITHUB_RUN_ID is not set; the runner sets it for every step$/);
});
