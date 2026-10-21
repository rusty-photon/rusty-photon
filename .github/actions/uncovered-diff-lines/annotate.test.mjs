import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { existsSync, mkdirSync, mkdtempSync, readFileSync } from 'node:fs';
import path from 'node:path';
import { test } from 'node:test';

import { capture, serve } from '../lib/testing.mjs';
import { CHECK, DIFF_COVER, POSTER, annotate, execute } from './annotate.mjs';

// --- Fixtures -----------------------------------------------------------------

const RUN = 'https://github.example/o/r/actions/runs/123';
const LCOV = 'combined-coverage.info';
const DIFF = 'diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,0 +2,1 @@\n+fn added() {}\n';
const USER_BASE = path.join('home', 'ci', '.local');
const ENV = { ...process.env, ACTIONS_RUNTIME_TOKEN: 'runtime', INPUT_TOKEN: 'tok', KEPT: 'yes' };
delete ENV.GITHUB_TOKEN;

function tempDir() {
  const base = process.env.TEST_TMPDIR ?? path.join(process.cwd(), '.tmp');
  mkdirSync(base, { recursive: true });
  return mkdtempSync(path.join(base, 'uncovered-'));
}

function json(res, status, value) {
  res.writeHead(status, { 'content-type': 'application/json' }).end(JSON.stringify(value));
}

// A program the fake spawn starts in each program's place: `node -e FAKE
// <answer>`, which prints the answer's stdout and stderr and exits with its
// code. The answer goes as base64 JSON, which no platform's quoting alters.
const FAKE = `
const { stdout, stderr, code } = JSON.parse(Buffer.from(process.argv[1], 'base64').toString());
process.stdout.write(stdout);
process.stderr.write(stderr);
process.exitCode = code;
`;

// Which of the four programs a call starts.
function which(cmd, args) {
  if (args[0] === '-m') return 'pip';
  if (args[0] === '-c') return 'user-base';
  if (args[0] === POSTER) return 'poster';
  if (path.basename(cmd) === 'diff-cover') return 'diff-cover';
  return 'unknown';
}

// A spawn that answers each program as `answers` says ({ stdout, stderr,
// code }, all optional), USER_BASE for pip's user base and success for the
// rest, recording what it was asked to start.
function fakeSpawn(answers = {}) {
  const calls = [];
  const spawnFake = (cmd, args, options) => {
    calls.push({ cmd, args, options, program: which(cmd, args) });
    const answer = answers[which(cmd, args)] ?? (which(cmd, args) === 'user-base' ? { stdout: `${USER_BASE}\n` } : {});
    const { stdout = '', stderr = '', code = 0 } = answer;
    const encoded = Buffer.from(JSON.stringify({ stdout, stderr, code })).toString('base64');
    return spawn(process.execPath, ['-e', FAKE, encoded], options);
  };
  return { spawn: spawnFake, calls };
}

// A fake GitHub API for pull request 7 of o/r: its diff, and the check runs.
async function fakeGitHub({ diff = (res) => res.writeHead(200).end(DIFF), checks = (res) => json(res, 201, { id: 1 }) } = {}) {
  return serve((req, res) => {
    if (req.method === 'GET' && req.url === '/repos/o/r/pulls/7') diff(res);
    else if (req.method === 'POST' && req.url === '/repos/o/r/check-runs') checks(res);
    else json(res, 404, { message: 'Not Found' });
  });
}

async function score(api, fake, options = {}) {
  const dir = tempDir();
  const result = await capture(() =>
    annotate({ lcov: LCOV, token: 'tok', pr: '7', sha: 'abc123', dir, api: api.url, repo: 'o/r', runUrl: RUN, env: ENV, spawn: fake.spawn, delayMs: 1, ...options }),
  );
  return { ...result, dir };
}

function posted(api) {
  return api.requests.filter((r) => r.method === 'POST').map((r) => JSON.parse(r.body));
}

// The check posted when `stage` failed.
function assertUnscored(api, stage) {
  const [body] = posted(api);
  const { output, ...rest } = body;
  assert.deepEqual(rest, { name: CHECK, head_sha: 'abc123', status: 'completed', conclusion: 'neutral', details_url: RUN });
  assert.equal(output.title, `Not scored: ${stage} failed`);
  assert.ok(output.summary.includes(`${stage} failed`), output.summary);
  assert.ok(output.summary.includes(`(${RUN})`), output.summary);
}

// --- annotate -----------------------------------------------------------------

test('annotate installs diff-cover, scores the diff and runs the poster, with no stdin', async () => {
  const api = await fakeGitHub();
  const fake = fakeSpawn();
  try {
    const { stdout, dir } = await score(api, fake);
    const diff = path.join(dir, 'pr.diff');
    const report = path.join(dir, 'diff-cover.json');
    assert.deepEqual(
      fake.calls.map(({ cmd, args }) => [cmd, ...args]),
      [
        ['python3', '-m', 'pip', 'install', '--user', '--quiet', DIFF_COVER],
        ['python3', '-c', 'import site; print(site.USER_BASE)'],
        [path.join(USER_BASE, 'bin', 'diff-cover'), LCOV, '--diff-file', diff, '--format', `json:${report}`, '--total-percent-float', '--quiet'],
        ['python3', POSTER, report, '--diff', diff, '--lcov', LCOV, '--repo', 'o/r', '--sha', 'abc123', '--details-url', RUN],
      ],
    );
    for (const { options } of fake.calls) assert.deepEqual(options.stdio, ['ignore', 'pipe', 'pipe']);
    assert.equal(readFileSync(diff, 'utf8'), DIFF);
    // The poster posts the check; the action posts none of its own.
    assert.deepEqual(
      api.requests.map((r) => [r.method, r.url, r.headers.accept, r.headers.authorization]),
      [['GET', '/repos/o/r/pulls/7', 'application/vnd.github.diff', 'Bearer tok']],
    );
    assert.doesNotMatch(stdout, /::warning::/);
  } finally {
    await api.close();
  }
});

test('annotate gives each program the environment without the runner\'s variables, and only the poster the token', async () => {
  const api = await fakeGitHub();
  const fake = fakeSpawn();
  try {
    await score(api, fake);
    for (const { options } of fake.calls) {
      assert.equal(options.env.ACTIONS_RUNTIME_TOKEN, undefined);
      assert.equal(options.env.INPUT_TOKEN, undefined);
      assert.equal(options.env.KEPT, 'yes');
    }
    assert.deepEqual(
      fake.calls.map(({ program, options }) => [program, options.env.GITHUB_TOKEN]),
      [
        ['pip', undefined],
        ['user-base', undefined],
        ['diff-cover', undefined],
        ['poster', 'tok'],
      ],
    );
  } finally {
    await api.close();
  }
});

for (const [program, stage] of [
  ['pip', 'installing diff-cover'],
  ['user-base', 'installing diff-cover'],
  ['diff-cover', 'running diff-cover'],
  ['poster', 'posting the check run'],
]) {
  test(`annotate posts the check as not scored when ${program} fails, and runs nothing after it`, async () => {
    const api = await fakeGitHub();
    const fake = fakeSpawn({ [program]: { stderr: 'some detail\nthe last line\n', code: 1 } });
    try {
      const { stdout } = await score(api, fake);
      assert.equal(fake.calls.at(-1).program, program);
      const name = program === 'diff-cover' ? 'diff-cover' : 'python3';
      assert.ok(
        stdout.includes(`::warning::${CHECK}: ${stage} failed, so the check is posted as not scored: ${name} exited with 1: the last line\n`),
        stdout,
      );
      assertUnscored(api, stage);
    } finally {
      await api.close();
    }
  });
}

test('annotate posts the check as not scored when the diff cannot be fetched, with GitHub\'s answer in the log', async () => {
  const api = await fakeGitHub({ diff: (res) => json(res, 406, { message: 'Sorry, the diff exceeded the maximum number of files (300).' }) });
  const fake = fakeSpawn();
  try {
    const { stdout } = await score(api, fake);
    assert.deepEqual(fake.calls.map((c) => c.program), ['pip', 'user-base']);
    assert.match(stdout, /::warning::uncovered-diff-lines: fetching the pull request's diff failed, .*Fetching pull request #7's diff failed \(GitHub answered: .*the diff exceeded the maximum number of files/);
    assertUnscored(api, "fetching the pull request's diff");
  } finally {
    await api.close();
  }
});

test('annotate keeps what a program printed out of the check, which is public', async () => {
  const api = await fakeGitHub();
  const fake = fakeSpawn({ pip: { stderr: 'ProxyError: http://192.0.2.1:3128 refused the connection\n', code: 1 } });
  try {
    const { stdout } = await score(api, fake);
    assert.match(stdout, /::warning::.*192\.0\.2\.1:3128 refused/);
    assert.doesNotMatch(api.requests.find((r) => r.method === 'POST').body.toString(), /192\.0\.2\.1/);
  } finally {
    await api.close();
  }
});

test('annotate posts the check as not scored when a program cannot start', async () => {
  const api = await fakeGitHub();
  const missing = (cmd, args, options) => spawn(path.join(tempDir(), 'no-such-python3'), args, options);
  try {
    const { stdout } = await score(api, { spawn: missing });
    assert.match(stdout, /::warning::uncovered-diff-lines: installing diff-cover failed, .*ENOENT/);
    assertUnscored(api, 'installing diff-cover');
  } finally {
    await api.close();
  }
});

test('annotate posts the check as not scored when spawn throws', async () => {
  const api = await fakeGitHub();
  const throwing = () => {
    throw new TypeError('bad arguments');
  };
  try {
    const { stdout } = await score(api, { spawn: throwing });
    assert.match(stdout, /::warning::uncovered-diff-lines: installing diff-cover failed, .*: bad arguments\n/);
    assertUnscored(api, 'installing diff-cover');
  } finally {
    await api.close();
  }
});

test('annotate retries posting the not-scored check, and only warns when it cannot', async () => {
  const api = await fakeGitHub({ checks: (res) => json(res, 502, { message: 'Bad Gateway' }) });
  const fake = fakeSpawn({ pip: { code: 1 } });
  try {
    const { stdout } = await score(api, fake);
    assert.equal(posted(api).length, 3);
    assert.match(stdout, /::warning::uncovered-diff-lines: could not post the check either, so this pull request lacks it: Posting uncovered-diff-lines as not scored failed/);
  } finally {
    await api.close();
  }
});

test('annotate posts the not-scored check once when the token cannot write checks, as a fork\'s cannot', async () => {
  const api = await fakeGitHub({ checks: (res) => json(res, 403, { message: 'Resource not accessible by integration' }) });
  const fake = fakeSpawn({ poster: { stderr: 'error: creating the uncovered-diff-lines check run failed: HTTP 403\n', code: 1 } });
  try {
    const { stdout } = await score(api, fake);
    assert.equal(posted(api).length, 1);
    assert.match(stdout, /could not post the check either, .*Resource not accessible by integration/);
  } finally {
    await api.close();
  }
});

// --- execute ------------------------------------------------------------------

test('execute copies a program\'s output to the log and resolves to its stdout', async () => {
  const fake = fakeSpawn({ pip: { stdout: 'out\n', stderr: 'err\n' } });
  const { value, stdout, stderr } = await capture(() => execute('python3', ['-m', 'pip'], { env: ENV, spawn: fake.spawn }));
  assert.equal(value, 'out\n');
  assert.equal(stdout, 'out\n');
  assert.equal(stderr, 'err\n');
});

test('execute rejects with the exit code and the last line of stderr, or the code alone', async () => {
  const failing = fakeSpawn({ pip: { stderr: 'first\nlast\n', code: 2 } });
  await capture(() =>
    assert.rejects(execute('python3', ['-m', 'pip'], { env: ENV, spawn: failing.spawn }), { message: 'python3 exited with 2: last' }),
  );
  const silent = fakeSpawn({ pip: { code: 3 } });
  await capture(() => assert.rejects(execute('python3', ['-m', 'pip'], { env: ENV, spawn: silent.spawn }), { message: 'python3 exited with 3' }));
});

// --- The poster ---------------------------------------------------------------

test('the poster is where the action runs it from', () => {
  assert.ok(existsSync(POSTER), POSTER);
});
