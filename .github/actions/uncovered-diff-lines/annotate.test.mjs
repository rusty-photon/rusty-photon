import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { existsSync, mkdirSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
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
// code, or with `hang` runs until it is killed. The answer goes as base64
// JSON, which no platform's quoting alters.
const FAKE = `
const { stdout, stderr, code, hang } = JSON.parse(Buffer.from(process.argv[1], 'base64').toString());
process.stdout.write(stdout);
process.stderr.write(stderr);
process.exitCode = code;
if (hang) setInterval(() => {}, 1000);
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
// code, hang }, all optional, or 'real' to start the program itself),
// USER_BASE for pip's user base and success for the rest, recording what it
// was asked to start and the child it started.
function fakeSpawn(answers = {}) {
  const calls = [];
  const spawnFake = (cmd, args, options) => {
    const program = which(cmd, args);
    const answer = answers[program] ?? (program === 'user-base' ? { stdout: `${USER_BASE}\n` } : {});
    let child;
    if (answer === 'real') {
      child = spawn(cmd, args, options);
    } else {
      const { stdout = '', stderr = '', code = 0, hang = false } = answer;
      const encoded = Buffer.from(JSON.stringify({ stdout, stderr, code, hang })).toString('base64');
      child = spawn(process.execPath, ['-e', FAKE, encoded], options);
    }
    calls.push({ cmd, args, options, program, child });
    return child;
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

async function score(api, fake, { dir = tempDir(), ...options } = {}) {
  const result = await capture(() =>
    annotate({ lcov: LCOV, token: 'tok', pr: '7', sha: 'abc123', dir, api: api.url, repo: 'o/r', runUrl: RUN, env: ENV, spawn: fake.spawn, delayMs: 1, ...options }),
  );
  return { ...result, dir };
}

function posted(api) {
  return api.requests.filter((r) => r.method === 'POST').map((r) => JSON.parse(r.body));
}

// The check posted, last, when `stage` failed.
function assertUnscored(api, stage) {
  const body = posted(api).at(-1);
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

test('annotate stops the program under way when the step is stopped, and posts nothing', async () => {
  const api = await fakeGitHub();
  const stop = new AbortController();
  const fake = fakeSpawn({ pip: { hang: true } });
  // The runner's signal comes once the program is running.
  const spawnThenStop = (cmd, args, options) => fake.spawn(cmd, args, options).once('spawn', () => stop.abort());
  try {
    const { stdout } = await score(api, { spawn: spawnThenStop }, { signal: stop.signal });
    assert.deepEqual(fake.calls.map((c) => c.program), ['pip']);
    assert.equal(fake.calls[0].child.killed, true);
    assert.ok(stdout.includes(`::warning::${CHECK}: the step was stopped while installing diff-cover, so no check is posted\n`), stdout);
    assert.deepEqual(posted(api), []);
  } finally {
    await api.close();
  }
});

test('annotate stops the diff request when the step is stopped, and posts nothing', async () => {
  const stop = new AbortController();
  // The request arrives, the step is stopped, and no answer comes.
  const api = await fakeGitHub({ diff: () => stop.abort() });
  try {
    const { stdout } = await score(api, fakeSpawn(), { signal: stop.signal });
    assert.ok(stdout.includes(`::warning::${CHECK}: the step was stopped while fetching the pull request's diff, so no check is posted\n`), stdout);
    assert.deepEqual(posted(api), []);
  } finally {
    await api.close();
  }
});

// --- The real poster --------------------------------------------------------------
//
// The not-scored check hangs on the poster's exit code: 1 when it could not
// create the check run, 0 once the run exists. These run the poster itself,
// with python3, against a fake API. The coverage job, its one caller, runs on
// Linux, so they run there.

const onLinux = { skip: process.platform !== 'linux' && "the coverage job, the poster's one caller, runs on Linux" };

// A work directory holding a diff-cover report of `runs` runs of uncovered
// lines in src/lib.rs, and the LCOV report it was scored against.
function scored(runs) {
  const dir = tempDir();
  const lines = Array.from({ length: runs }, (_, i) => 2 * i + 1);
  const report = { src_stats: { 'src/lib.rs': { violation_lines: lines } }, total_num_lines: 2 * runs, total_num_violations: runs, total_percent_covered: 50 };
  writeFileSync(path.join(dir, 'diff-cover.json'), JSON.stringify(report));
  writeFileSync(path.join(dir, 'coverage.info'), 'SF:src/lib.rs\nDA:1,0\nend_of_record\n');
  return dir;
}

// A fake API that answers the poster's create with `create` and its updates
// with `update`, and the action's not-scored check with 201.
async function fakeChecks({ create = (res) => json(res, 201, { id: 9, html_url: 'https://github.example/o/r/runs/9' }), update = (res) => json(res, 200, { id: 9 }) } = {}) {
  return serve((req, res) => {
    if (req.method === 'GET' && req.url === '/repos/o/r/pulls/7') res.writeHead(200).end(DIFF);
    else if (req.method === 'POST' && req.url === '/repos/o/r/check-runs' && JSON.parse(req.body).output.title.startsWith('Not scored')) json(res, 201, { id: 10 });
    else if (req.method === 'POST' && req.url === '/repos/o/r/check-runs') create(res);
    else if (req.method === 'PATCH' && req.url === '/repos/o/r/check-runs/9') update(res);
    else json(res, 404, { message: 'Not Found' });
  });
}

// annotate with the real poster, which posts to `posterApi` (GITHUB_API_URL).
function scoreWithPoster(api, dir, { posterApi = api.url } = {}) {
  return score(api, fakeSpawn({ poster: 'real' }), { dir, lcov: path.join(dir, 'coverage.info'), env: { ...ENV, GITHUB_API_URL: posterApi } });
}

test('the real poster creates the check, and no not-scored check follows', onLinux, async () => {
  const api = await fakeChecks();
  try {
    const { stdout } = await scoreWithPoster(api, scored(3));
    const [check, ...more] = posted(api);
    assert.deepEqual(more, []);
    assert.equal(check.conclusion, 'neutral');
    assert.equal(check.output.annotations.length, 3);
    assert.ok(stdout.includes('uncovered-diff-lines: neutral (3 annotation(s)) https://github.example/o/r/runs/9\n'), stdout);
    assert.doesNotMatch(stdout, /::warning::/);
  } finally {
    await api.close();
  }
});

test('the real poster exits 1 when GitHub refuses the create, and the not-scored check follows', onLinux, async () => {
  const api = await fakeChecks({ create: (res) => json(res, 422, { message: 'Validation Failed' }) });
  try {
    const { stdout } = await scoreWithPoster(api, scored(3));
    assert.equal(posted(api).length, 2);
    assert.match(stdout, /::warning::uncovered-diff-lines: posting the check run failed, .*python3 exited with 1: error: creating the uncovered-diff-lines check run failed: HTTP 422 \{"message":"Validation Failed"\}\n/);
    assertUnscored(api, 'posting the check run');
  } finally {
    await api.close();
  }
});

test('the real poster exits 1 when the create gets no answer, and the not-scored check follows', onLinux, async () => {
  const api = await fakeChecks();
  const gone = await serve(() => {});
  await gone.close();
  try {
    const { stdout } = await scoreWithPoster(api, scored(3), { posterApi: gone.url });
    assert.equal(posted(api).length, 1);
    assert.match(stdout, /::warning::uncovered-diff-lines: posting the check run failed, .*python3 exited with 1: error: creating the uncovered-diff-lines check run failed: /);
    assertUnscored(api, 'posting the check run');
  } finally {
    await api.close();
  }
});

test('the real poster exits 0 when only its annotations past the first 50 fail, and its check stands', onLinux, async () => {
  const api = await fakeChecks({ update: (res) => json(res, 500, { message: 'Server Error' }) });
  try {
    const { stdout } = await scoreWithPoster(api, scored(60));
    assert.equal(posted(api).length, 1);
    assert.equal(api.requests.filter((r) => r.method === 'PATCH').length, 1);
    assert.ok(stdout.includes('::warning::adding annotations to uncovered-diff-lines failed: HTTP 500 {"message":"Server Error"}\n'), stdout);
    assert.doesNotMatch(stdout, /not scored/);
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
