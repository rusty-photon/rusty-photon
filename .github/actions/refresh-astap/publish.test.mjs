import assert from 'node:assert/strict';
import { mkdirSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, sep } from 'node:path';
import test from 'node:test';

import { formatTable, TABLE, TABLE_FILE } from '../install-astap/astap.mjs';
import { describe } from '../lib/helpers.mjs';
import { capture, serve, zip } from '../lib/testing.mjs';
import { detect } from './detect.mjs';
import { BLOCKER, BRANCH, publish, readResults, repin, row, TABLE_PATH, TITLE, TRACKER } from './publish.mjs';

// --- Fixtures -----------------------------------------------------------------

function tempDir() {
  return mkdtempSync(join(process.env.TEST_TMPDIR ?? tmpdir(), 'publish-'));
}

const RUN = 'https://github.example/o/r/actions/runs/123';
const BASE = 'c12b06c90458450823f82168e4dd3905d54da36f';
const TREE = 'd69052a88bc2ddb52f9a5cba857973a10aea161b';
const NEW_TREE = '1'.repeat(40);
const COMMIT = '2'.repeat(40);
const OLD_TIP = '3'.repeat(40);

// What a detect job writes for `runner`, upstream now serving `upstream`.
function result(runner, upstream, verified = 'passed', extra = {}) {
  return {
    runner,
    archive: TABLE.cli[runner].archive,
    pinned: TABLE.cli[runner].sha256,
    upstream,
    banner: verified === 'skipped' ? 'cross-mirror SHA only (no runner, no verification)' : 'ASTAP astrometric solver version CLI-2099.01.01',
    verified,
    ...extra,
  };
}

function json(res, status, value) {
  res.writeHead(status, { 'content-type': 'application/json' }).end(JSON.stringify(value));
}

// base64 as the contents API sends it, a line break every 60 characters.
const base64 = (text) => Buffer.from(text).toString('base64').replace(/.{60}/g, '$&\n');

const TRACKER_ISSUE = { number: 7, title: TRACKER.title, html_url: 'https://github.example/o/r/issues/7' };

// A fake GitHub API for repository o/r, answering in the shapes the real one
// gave for main (git/ref, git/commits, contents) and the docs give for the
// writes. `table` is main's assets.json; `branch` the refresh branch's tip,
// none when undefined; `pulls` its open pull requests; `issues` the open
// issues by label; `fail` answers a request `fail(req, res)` handles.
async function fakeGitHub({
  table = TABLE,
  encoding = 'base64',
  branch,
  pulls = [],
  issues = {},
  fail = () => false,
  labels = [BLOCKER.label, 'ci-flake'],
} = {}) {
  const server = await serve((req, res) => {
    if (fail(req, res)) return;
    const url = new URL(req.url, server.url);
    const route = `${req.method} ${url.pathname}`;
    const body = req.body.length > 0 ? JSON.parse(req.body) : undefined;
    switch (route) {
      case 'GET /repos/o/r/git/ref/heads/main':
        return json(res, 200, { ref: 'refs/heads/main', object: { sha: BASE, type: 'commit' } });
      case `GET /repos/o/r/git/commits/${BASE}`:
        return json(res, 200, { sha: BASE, tree: { sha: TREE }, parents: [{ sha: '4'.repeat(40) }] });
      case `GET /repos/o/r/contents/${TABLE_PATH}`:
        return json(res, 200, { type: 'file', encoding, path: TABLE_PATH, content: encoding === 'base64' ? base64(formatTable(table)) : '' });
      case `GET /repos/o/r/git/ref/heads/${BRANCH}`:
        return branch === undefined
          ? json(res, 404, { message: 'Not Found', status: '404' })
          : json(res, 200, { ref: `refs/heads/${BRANCH}`, object: { sha: branch, type: 'commit' } });
      case 'GET /repos/o/r/pulls':
        return json(res, 200, pulls);
      case 'GET /repos/o/r/issues':
        return json(res, 200, issues[url.searchParams.get('labels')] ?? []);
      case 'POST /repos/o/r/git/trees':
        return json(res, 201, { sha: NEW_TREE });
      case 'POST /repos/o/r/git/commits':
        return json(res, 201, { sha: COMMIT, tree: { sha: body.tree } });
      case 'POST /repos/o/r/git/refs':
        return json(res, 201, { ref: body.ref, object: { sha: body.sha } });
      case `PATCH /repos/o/r/git/refs/heads/${BRANCH}`:
        return json(res, 200, { ref: `refs/heads/${BRANCH}`, object: { sha: body.sha } });
      case 'POST /repos/o/r/pulls':
        return json(res, 201, { number: 10, html_url: 'https://github.example/o/r/pull/10' });
      case 'POST /repos/o/r/issues':
        return json(res, 201, {
          number: 43,
          html_url: 'https://github.example/o/r/issues/43',
          labels: body.labels.filter((l) => labels.includes(l)).map((name) => ({ name })),
        });
      default:
        if (/^(POST \/repos\/o\/r\/issues\/\d+\/comments|PATCH \/repos\/o\/r\/pulls\/\d+)$/.test(route)) return json(res, 200, { id: 1 });
        return json(res, 404, { message: 'Not Found' });
    }
  });
  return server;
}

function run(api, results, options = {}) {
  return capture(() => publish({ api: api.url, repo: 'o/r', token: 'tok', runUrl: RUN, results, delayMs: 1, ...options }));
}

// Each request as `METHOD path`, the query left out.
const routes = (api) => api.requests.map(({ method, url }) => `${method} ${url.split('?')[0]}`);
const writes = (api) => api.requests.filter(({ method }) => method !== 'GET');
const sent = (api, route) => {
  const found = api.requests.find(({ method, url }) => `${method} ${url.split('?')[0]}` === route);
  assert.ok(found, `no ${route}`);
  return JSON.parse(found.body);
};

const NEW = { 'Linux-X64': '5'.repeat(64), 'Windows-ARM64': '6'.repeat(64) };
const PASSED = [result('Linux-X64', NEW['Linux-X64']), result('Windows-ARM64', NEW['Windows-ARM64'], 'skipped')];

// --- Proposing the new pins ---------------------------------------------------------

test('no rotation is nothing to do, and nothing is asked of GitHub', async (t) => {
  const api = await fakeGitHub();
  t.after(api.close);
  const { value, stdout } = await run(api, []);
  assert.deepEqual(value, { outcome: 'none' });
  assert.match(stdout, /No archive rotated: nothing to do/);
  assert.deepEqual(api.requests, []);
});

test('new pins that passed are committed on main, on a new branch, in a new pull request', async (t) => {
  const api = await fakeGitHub({ issues: { [TRACKER.label]: [TRACKER_ISSUE] } });
  t.after(api.close);
  const { value } = await run(api, PASSED);
  assert.deepEqual(value, { outcome: 'proposed', url: 'https://github.example/o/r/pull/10' });
  assert.deepEqual(routes(api), [
    'GET /repos/o/r/git/ref/heads/main',
    `GET /repos/o/r/git/commits/${BASE}`,
    `GET /repos/o/r/contents/${TABLE_PATH}`,
    `GET /repos/o/r/git/ref/heads/${BRANCH}`,
    'GET /repos/o/r/pulls',
    'POST /repos/o/r/git/trees',
    'POST /repos/o/r/git/commits',
    'POST /repos/o/r/git/refs',
    'POST /repos/o/r/pulls',
    'GET /repos/o/r/issues',
    'POST /repos/o/r/issues/7/comments',
  ]);
  assert.ok(api.requests.every(({ headers }) => headers.authorization === 'Bearer tok'));
  // The table at main's commit, and main's open pull requests from the branch.
  assert.equal(new URL(api.requests[2].url, api.url).searchParams.get('ref'), BASE);
  assert.deepEqual(Object.fromEntries(new URL(api.requests[4].url, api.url).searchParams), {
    state: 'open',
    head: `o:${BRANCH}`,
    base: 'main',
  });

  // One file changes on main's tree: the table, at its two rotated pins.
  const { base_tree, tree } = sent(api, 'POST /repos/o/r/git/trees');
  assert.equal(base_tree, TREE);
  assert.equal(tree.length, 1);
  assert.deepEqual({ ...tree[0], content: undefined }, { path: TABLE_PATH, mode: '100644', type: 'blob', content: undefined });
  const before = formatTable(TABLE).split('\n');
  const after = tree[0].content.split('\n');
  assert.equal(after.length, before.length);
  assert.deepEqual(
    after.flatMap((line, i) => (line === before[i] ? [] : [[before[i], line]])),
    ['Linux-X64', 'Windows-ARM64'].map((runner) => [`      "sha256": "${TABLE.cli[runner].sha256}"`, `      "sha256": "${NEW[runner]}"`]),
  );

  const commit = sent(api, 'POST /repos/o/r/git/commits');
  assert.deepEqual({ ...commit, message: undefined }, { message: undefined, tree: NEW_TREE, parents: [BASE] });
  assert.match(commit.message, new RegExp(`^${TITLE.replace(/[()]/g, '\\$&')}\n\nUpstream rotated SourceForge archives\\.`));
  assert.deepEqual(sent(api, 'POST /repos/o/r/git/refs'), { ref: `refs/heads/${BRANCH}`, sha: COMMIT });

  const pr = sent(api, 'POST /repos/o/r/pulls');
  assert.deepEqual({ ...pr, body: undefined }, { title: TITLE, head: BRANCH, base: 'main', body: undefined });
  assert.match(pr.body, /^Auto-refresh detected upstream rotation of ASTAP CLI archives\.\n\n## Per-platform results\n\n- \*\*Linux-X64\*\*/);
  assert.ok(pr.body.includes(`${row(PASSED[0])}\n${row(PASSED[1])}\n`));
  assert.ok(pr.body.includes(`Full verification logs:\n${RUN}\n`));

  assert.deepEqual(sent(api, 'POST /repos/o/r/issues/7/comments'), {
    body: `Auto-refresh workflow opened https://github.example/o/r/pull/10 with new SHAs (M101 verified on all runner-backed platforms). Review and merge to close this.\n\nRun: ${RUN}`,
  });
});

test("an earlier run's branch is moved to the new commit, and its open pull request gets the new body", async (t) => {
  const open = { number: 9, html_url: 'https://github.example/o/r/pull/9' };
  const api = await fakeGitHub({ branch: OLD_TIP, pulls: [open], issues: { [TRACKER.label]: [TRACKER_ISSUE] } });
  t.after(api.close);
  const { value, stdout } = await run(api, PASSED);
  assert.deepEqual(value, { outcome: 'proposed', url: open.html_url });
  assert.deepEqual(
    writes(api).map(({ method, url }) => `${method} ${url}`),
    [
      'POST /repos/o/r/git/trees',
      'POST /repos/o/r/git/commits',
      `PATCH /repos/o/r/git/refs/heads/${BRANCH}`,
      'PATCH /repos/o/r/pulls/9',
      'POST /repos/o/r/issues/7/comments',
    ],
  );
  assert.deepEqual(sent(api, `PATCH /repos/o/r/git/refs/heads/${BRANCH}`), { sha: COMMIT, force: true });
  assert.match(sent(api, 'PATCH /repos/o/r/pulls/9').body, /^Auto-refresh detected upstream rotation/);
  assert.match(stdout, /^Updated #9: https:\/\/github\.example\/o\/r\/pull\/9$/m);
  assert.match(sent(api, 'POST /repos/o/r/issues/7/comments').body, /^Auto-refresh workflow opened https:\/\/github\.example\/o\/r\/pull\/9 with new SHAs/);
});

test('with no open tracking issue the proposal stands on its own, and nothing is commented', async (t) => {
  const api = await fakeGitHub({ issues: { [TRACKER.label]: [{ ...TRACKER_ISSUE, title: `${TRACKER.title}, again` }] } });
  t.after(api.close);
  const { value, stdout } = await run(api, PASSED);
  assert.equal(value.outcome, 'proposed');
  assert.ok(!routes(api).some((route) => route.endsWith('/comments')));
  assert.match(stdout, /install-astap's tracking issue is not open, so it gets no comment/);
});

test('when main already pins what upstream serves, nothing is proposed', async (t) => {
  const table = structuredClone(TABLE);
  table.cli['Linux-X64'].sha256 = NEW['Linux-X64'];
  table.cli['Windows-ARM64'].sha256 = NEW['Windows-ARM64'];
  const api = await fakeGitHub({ table, issues: { [TRACKER.label]: [TRACKER_ISSUE] } });
  t.after(api.close);
  const { value, stdout } = await run(api, PASSED);
  assert.deepEqual(value, { outcome: 'current' });
  assert.match(stdout, new RegExp(`main \\(${BASE}\\) already pins what upstream serves: nothing to propose`));
  assert.deepEqual(writes(api), []);
});

test('a table GitHub sends other than as base64 is refused, and nothing is written', async (t) => {
  // The contents API sends a file over 1 MB with encoding "none" and no content.
  const api = await fakeGitHub({ encoding: 'none' });
  t.after(api.close);
  await assert.rejects(run(api, PASSED), new RegExp(`GitHub sent ${TABLE_PATH.replace(/\./g, '\\.')} as "none", not base64`));
  assert.deepEqual(writes(api), []);
});

test('a dry run reads what it would act on, prints what it would write, and writes nothing', async (t) => {
  const api = await fakeGitHub({ branch: OLD_TIP, pulls: [{ number: 9, html_url: 'u' }], issues: { [TRACKER.label]: [TRACKER_ISSUE] } });
  t.after(api.close);
  const { value, stdout } = await run(api, PASSED, { dryRun: true });
  assert.deepEqual(value, { outcome: 'proposed' });
  assert.deepEqual(writes(api), []);
  assert.match(stdout, new RegExp(`Would commit ${TABLE_PATH.replace(/\./g, '\\.')} on ${BASE}, move ${BRANCH} to it, and give #9 this body:\nAuto-refresh detected`));
  assert.match(stdout, /Would comment on #7:\nAuto-refresh workflow would update #9 with new SHAs\./);
});

test('a pull request GitHub refuses to open fails the step with its answer, and is asked for once', async (t) => {
  const refused = { message: 'GitHub Actions is not permitted to create or approve pull requests.' };
  const api = await fakeGitHub({
    fail: (req, res) => {
      if (req.method !== 'POST' || req.url !== '/repos/o/r/pulls') return false;
      json(res, 403, refused);
      return true;
    },
  });
  t.after(api.close);
  await assert.rejects(run(api, PASSED), (err) => {
    assert.match(describe(err), new RegExp(`^Opening the pull request failed \\(GitHub answered: ${JSON.stringify(refused).replace(/[.()]/g, '\\$&')}\\)`));
    return true;
  });
  assert.equal(routes(api).filter((route) => route === 'POST /repos/o/r/pulls').length, 1);
});

test('a write GitHub answers 502 is sent again when a repeat makes the same thing, and the proposal goes on', async (t) => {
  const failedOnce = new Set();
  const flaky = ['POST /repos/o/r/git/trees', 'POST /repos/o/r/git/commits', `PATCH /repos/o/r/git/refs/heads/${BRANCH}`, 'PATCH /repos/o/r/pulls/9'];
  const api = await fakeGitHub({
    branch: OLD_TIP,
    pulls: [{ number: 9, html_url: 'https://github.example/o/r/pull/9' }],
    fail: (req, res) => {
      const route = `${req.method} ${req.url}`;
      if (!flaky.includes(route) || failedOnce.has(route)) return false;
      failedOnce.add(route);
      json(res, 502, { message: 'Server Error' });
      return true;
    },
  });
  t.after(api.close);
  const { value } = await run(api, PASSED);
  assert.deepEqual(value, { outcome: 'proposed', url: 'https://github.example/o/r/pull/9' });
  for (const route of flaky) assert.equal(routes(api).filter((r) => r === route).length, 2, route);
});

test('creating the branch and opening the pull request are each sent once, whatever GitHub answers', async (t) => {
  for (const route of ['POST /repos/o/r/git/refs', 'POST /repos/o/r/pulls']) {
    const api = await fakeGitHub({
      fail: (req, res) => {
        if (`${req.method} ${req.url}` !== route) return false;
        json(res, 502, { message: 'Server Error' });
        return true;
      },
    });
    t.after(api.close);
    await assert.rejects(run(api, PASSED), /failed \(GitHub answered: \{"message":"Server Error"\}\)/);
    assert.equal(routes(api).filter((r) => r === route).length, 1, route);
  }
});

// --- A failed check ---------------------------------------------------------------

const FAILED = [result('Linux-X64', NEW['Linux-X64']), result('macOS-ARM64', '7'.repeat(64), 'failed', { problem: 'M101 solve exited 1' })];

test('a failed check opens the blocker, touches no branch, and tells the tracking issue', async (t) => {
  const api = await fakeGitHub({ issues: { [TRACKER.label]: [TRACKER_ISSUE] } });
  t.after(api.close);
  const { value } = await run(api, FAILED);
  assert.deepEqual(value, { outcome: 'blocked' });
  assert.deepEqual(
    writes(api).map(({ method, url }) => `${method} ${url}`),
    ['POST /repos/o/r/issues', 'POST /repos/o/r/issues/7/comments'],
  );
  assert.ok(!routes(api).some((route) => route.includes('/git/') || route.includes('/pulls')));
  const issue = sent(api, 'POST /repos/o/r/issues');
  assert.deepEqual(issue.labels, [BLOCKER.label, 'ci-flake']);
  assert.equal(issue.title, BLOCKER.title);
  assert.ok(issue.body.startsWith('Auto-refresh detected upstream SHA rotation, but the new ASTAP binary failed to solve the M101 fixture'));
  assert.ok(issue.body.includes(`${row(FAILED[0])}\n${row(FAILED[1])}\n`));
  assert.ok(issue.body.includes('- **macOS-ARM64** (`ASTAP astrometric solver version CLI-2099.01.01`): '));
  assert.ok(issue.body.includes('M101 verify: **failed**: M101 solve exited 1'));
  assert.ok(issue.body.endsWith(`Workflow run: ${RUN}`));
  assert.deepEqual(sent(api, 'POST /repos/o/r/issues/7/comments'), {
    body: `Auto-refresh workflow detected rotation but M101 verification failed on at least one platform. See #43.\n\nRun: ${RUN}`,
  });
});

test('a failed check while the blocker is open comments on it', async (t) => {
  const blocker = { number: 44, title: BLOCKER.title, html_url: 'https://github.example/o/r/issues/44' };
  const api = await fakeGitHub({ issues: { [BLOCKER.label]: [blocker] } });
  t.after(api.close);
  await run(api, FAILED);
  assert.deepEqual(
    writes(api).map(({ method, url, body }) => [`${method} ${url}`, JSON.parse(body)]),
    [['POST /repos/o/r/issues/44/comments', { body: `Failed again — see ${RUN}` }]],
  );
});

test('a blocker GitHub opens without its label fails the step, as the next failure would open another', async (t) => {
  const api = await fakeGitHub({ labels: ['ci-flake'] });
  t.after(api.close);
  await assert.rejects(run(api, FAILED), /#43 was opened without the label astap-refresh-blocked, which does not exist in the repository/);
});

test('a dry run of a failed check writes nothing', async (t) => {
  const api = await fakeGitHub({ issues: { [TRACKER.label]: [TRACKER_ISSUE] } });
  t.after(api.close);
  const { stdout } = await run(api, FAILED, { dryRun: true });
  assert.deepEqual(writes(api), []);
  assert.match(stdout, new RegExp(`Would open or comment on "${BLOCKER.title}":\nAuto-refresh detected upstream SHA rotation`));
  assert.match(stdout, /See the open issue labelled `astap-refresh-blocked`\./);
});

// --- The results ------------------------------------------------------------------

// `results` laid out as download-artifact lays them out, a directory each.
function artifacts(results) {
  const dir = tempDir();
  for (const r of results) {
    mkdirSync(join(dir, `refresh-result-${r.runner}`));
    writeFileSync(join(dir, `refresh-result-${r.runner}`, `${r.runner}.json`), JSON.stringify(r));
  }
  return dir;
}

test('readResults reads every artifact download-artifact made, in order, and none when it made no directory', async () => {
  const dir = artifacts([result('Windows-X64', '8'.repeat(64)), result('Linux-X64', '9'.repeat(64))]);
  assert.deepEqual(
    (await readResults(dir)).map(({ runner }) => runner),
    ['Linux-X64', 'Windows-X64'],
  );
  assert.deepEqual(await readResults(join(tempDir(), 'artifacts')), []);
});

test('readResults refuses a result detect would not write', async () => {
  const bad = (extra) => readResults(artifacts([{ ...result('Linux-X64', '9'.repeat(64)), ...extra }]));
  await assert.rejects(bad({ upstream: 'A'.repeat(64) }), /refresh-result-Linux-X64\/Linux-X64\.json: upstream, "A{64}", is not a SHA-256/);
  await assert.rejects(bad({ verified: 'true' }), /verified is "true", not one of passed, failed, skipped/);
  await assert.rejects(bad({ banner: undefined }), /: no banner$/);
});

test('what detect writes is what publish reads', async (t) => {
  const cli = zip([{ path: 'astap_cli.exe', data: 'a new release' }]);
  const server = await serve((req, res) => res.end(cli));
  t.after(server.close);
  const results = join(tempDir(), 'refresh-result-Windows-ARM64');
  await capture(() =>
    detect({ runner: 'Windows-ARM64', verify: false, work: tempDir(), results, fixtures: tempDir(), server: server.url, download: { delayMs: 1 } }),
  );
  const [read] = await readResults(join(results, '..'));
  assert.equal(read.runner, 'Windows-ARM64');
  assert.equal(read.verified, 'skipped');
  assert.equal(repin(TABLE, [read]).changes.length, 1);
});

// --- The table ------------------------------------------------------------------

test('repin moves the pins that rotated, leaves a pin already moved, and changes no other field', () => {
  const table = structuredClone(TABLE);
  table.cli['Windows-ARM64'].sha256 = NEW['Windows-ARM64'];
  const { table: next, changes } = repin(table, PASSED);
  assert.deepEqual(changes, [{ runner: 'Linux-X64', archive: TABLE.cli['Linux-X64'].archive, from: TABLE.cli['Linux-X64'].sha256, to: NEW['Linux-X64'] }]);
  assert.equal(next.cli['Linux-X64'].sha256, NEW['Linux-X64']);
  const without = (t) => ({ ...t, cli: { ...t.cli, 'Linux-X64': { ...t.cli['Linux-X64'], sha256: undefined } } });
  assert.deepEqual(without(next), without(table));
  assert.equal(table.cli['Linux-X64'].sha256, TABLE.cli['Linux-X64'].sha256, 'the table it was given is left as it was');
});

test("repin refuses a result whose archive is not the one main's table names", () => {
  assert.throws(
    () => repin(TABLE, [{ ...PASSED[0], archive: 'other.zip' }]),
    /the table names astap_command-line_version_Linux_amd64\.zip for Linux-X64, not other\.zip, which the refresh downloaded/,
  );
  assert.throws(() => repin(TABLE, [{ ...PASSED[0], runner: 'Linux-RISCV' }]), /ASTAP has no asset for Linux-RISCV/);
});

test("row keeps a banner's backticks from ending its code span", () => {
  assert.equal(
    row({ ...PASSED[0], banner: 'ASTAP `@team` [x](y)' }),
    `- **Linux-X64** (\`ASTAP @team [x](y)\`): \`${TABLE.cli['Linux-X64'].sha256.slice(0, 8)}…\` → \`55555555…\` — M101 verify: **passed**`,
  );
});

test('the path the commit rewrites is where assets.json is', () => {
  assert.ok(TABLE_FILE.endsWith(TABLE_PATH.split('/').join(sep)), TABLE_FILE);
});

test("the tracking issue is the one install-astap's notify-on-failure opens", () => {
  const workflow = readFileSync(new URL('../../workflows/install-astap.yml', import.meta.url), 'utf8');
  assert.ok(workflow.includes(`title: "${TRACKER.title}"\n`), 'install-astap.yml opens its issue with another title');
  assert.ok(workflow.includes(`label: ${TRACKER.label}\n`), 'install-astap.yml labels its issue otherwise');
});
