import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { mkdirSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';

import { ARGS, CAP_MS, combinedReport, failure, runBazel } from './coverage.mjs';

function tempDir() {
  const base = process.env.TEST_TMPDIR ?? path.join(process.cwd(), '.tmp');
  mkdirSync(base, { recursive: true });
  return mkdtempSync(path.join(base, 'coverage-'));
}

// A program the fake spawn starts in Bazel's place: `node -e FAKE <what>`.
// It tells the test it is ready, then does `what`: exit-<n>, hang (until
// SIGINT, which it answers with Bazel's exit 8 for an interrupted run), or
// stubborn (hang through SIGINT).
const FAKE = `
const what = process.argv[1];
if (what === 'hang') process.on('SIGINT', () => process.exit(8));
if (what === 'stubborn') process.on('SIGINT', () => {});
process.send('ready', () => {
  if (what.startsWith('exit-')) process.exit(Number(what.slice(5)));
  setInterval(() => {}, 1000);
});
`;

// A spawn that runs FAKE as `what` in Bazel's place, recording what it was
// asked to start. `ready` is called once the fake has set itself up.
function fakeSpawn(what, { ready } = {}) {
  const calls = [];
  const spawnFake = (cmd, args, options) => {
    calls.push({ cmd, args, options });
    const child = spawn(process.execPath, ['-e', FAKE, what], { ...options, stdio: [...options.stdio, 'ipc'] });
    child.on('message', () => ready?.(child));
    return child;
  };
  return { spawn: spawnFake, calls };
}

test('runBazel runs bazel with the arguments, no stdin, and the step\'s environment without the runner\'s variables', async () => {
  const fake = fakeSpawn('exit-0');
  const env = { ACTIONS_RUNTIME_TOKEN: 'secret', INPUT_X: 'y', LAN_CACHE_WRITE_AUTH: 'kept', PATH: process.env.PATH };
  const result = await runBazel(['coverage', '//...'], { env, spawn: fake.spawn });
  assert.deepEqual(result, { code: 0, signal: null, error: undefined, capped: false, stopped: false });
  const [{ cmd, args, options }] = fake.calls;
  assert.equal(cmd, 'bazel');
  assert.deepEqual(args, ['coverage', '//...']);
  assert.deepEqual(options.stdio, ['ignore', 'inherit', 'inherit']);
  // The credential helper Bazel runs reads the write credential from Bazel's
  // own environment.
  assert.deepEqual(options.env, { LAN_CACHE_WRITE_AUTH: 'kept', PATH: process.env.PATH });
});

test('runBazel resolves to Bazel\'s exit code', async () => {
  const result = await runBazel(ARGS, { spawn: fakeSpawn('exit-3').spawn });
  assert.equal(result.code, 3);
  assert.equal(result.capped, false);
});

test('runBazel interrupts a run past its cap with SIGINT', async () => {
  const result = await runBazel(ARGS, { spawn: fakeSpawn('hang').spawn, capMs: 300, graceMs: 600_000 });
  assert.equal(result.capped, true);
  assert.equal(result.stopped, false);
  // The fake answers SIGINT with exit 8 once it has set itself up, and dies
  // of it before then, as every process on Windows does.
  assert.ok(result.code === 8 || result.signal === 'SIGINT', JSON.stringify(result));
});

test('runBazel interrupts Bazel at once when the step is stopped', async () => {
  const stop = new AbortController();
  const fake = fakeSpawn('hang', { ready: () => stop.abort() });
  const result = await runBazel(ARGS, { spawn: fake.spawn, signal: stop.signal, graceMs: 600_000 });
  assert.equal(result.stopped, true);
  assert.equal(result.capped, false);
  if (process.platform === 'win32') assert.equal(result.signal, 'SIGINT');
  else assert.equal(result.code, 8);
});

test('runBazel interrupts Bazel when the step was stopped before it started', async () => {
  const stop = new AbortController();
  stop.abort();
  const result = await runBazel(ARGS, { spawn: fakeSpawn('hang').spawn, signal: stop.signal, graceMs: 600_000 });
  assert.equal(result.stopped, true);
  assert.ok(result.code === 8 || result.signal === 'SIGINT', JSON.stringify(result));
});

test('runBazel kills a Bazel still running after the grace', { skip: process.platform === 'win32' && 'a kill on Windows is never ignored' }, async () => {
  const stop = new AbortController();
  const fake = fakeSpawn('stubborn', { ready: () => stop.abort() });
  const result = await runBazel(ARGS, { spawn: fake.spawn, signal: stop.signal, graceMs: 100 });
  assert.equal(result.stopped, true);
  assert.equal(result.signal, 'SIGKILL');
});

test('runBazel resolves with the error when bazel cannot start', async () => {
  const missing = (cmd, args, options) => spawn(path.join(tempDir(), 'no-such-bazel'), args, options);
  const result = await runBazel(ARGS, { spawn: missing });
  assert.equal(result.error.code, 'ENOENT');
  assert.match(failure(result), /^bazel coverage did not start: spawn .*no-such-bazel ENOENT$/);
});

test('runBazel resolves with the error when spawn throws', async () => {
  const throwing = () => {
    throw new TypeError('bad arguments');
  };
  const result = await runBazel(ARGS, { spawn: throwing });
  assert.equal(failure(result), 'bazel coverage did not start: bad arguments');
});

const ended = { code: 0, signal: null, error: undefined, capped: false, stopped: false };

test('failure passes exit 0 only', () => {
  assert.equal(failure(ended), undefined);
  assert.match(failure({ ...ended, code: 1 }), /^bazel coverage failed \(exit 1: 1 = build error/);
  assert.match(failure({ ...ended, code: 4 }), /^bazel coverage failed \(exit 4:/);
});

test('failure says a test failed for exit 3, and that its coverage is not published', () => {
  assert.match(failure({ ...ended, code: 3 }), /^A coverage test failed or hit its --test_timeout \(bazel exit 3\)\. .* not published\./);
});

test('failure names the cap, whatever Bazel then exited with', () => {
  const capped = { ...ended, capped: true };
  assert.match(failure({ ...capped, code: 8 }), /^bazel coverage hit its 50-minute cap and was interrupted \(exit 8\): /);
  assert.match(failure({ ...capped, code: 0 }), /hit its 50-minute cap .*\(exit 0\)/);
  assert.match(failure({ ...capped, code: null, signal: 'SIGKILL' }, { capMs: 120_000 }), /hit its 2-minute cap and was interrupted \(stopped by SIGKILL\)/);
  assert.equal(CAP_MS, 50 * 60_000);
});

test('failure names a stopped step, and a signal that was not this action\'s', () => {
  assert.equal(failure({ ...ended, stopped: true, code: 0 }), 'bazel coverage was stopped with the step (exit 0)');
  assert.equal(failure({ ...ended, code: null, signal: 'SIGTERM' }), 'bazel coverage was stopped by SIGTERM');
});

// What `bazel info output_path` prints: the path, and a line break.
function infoRun(outputPath) {
  const calls = [];
  const run = async (cmd, args) => {
    calls.push([cmd, ...args]);
    return `${outputPath}\n`;
  };
  return { run, calls };
}

test('combinedReport finds the report under Bazel\'s output path', async () => {
  const out = tempDir();
  mkdirSync(path.join(out, '_coverage'));
  const file = path.join(out, '_coverage', '_coverage_report.dat');
  writeFileSync(file, 'SF:services/rp/src/lib.rs\nend_of_record\n');
  const info = infoRun(out);
  assert.equal(await combinedReport({ run: info.run }), file);
  assert.deepEqual(info.calls, [['bazel', 'info', 'output_path']]);
});

test('combinedReport refuses a missing or empty report', async () => {
  const out = tempDir();
  await assert.rejects(combinedReport({ run: infoRun(out).run }), /^Error: the combined coverage report is missing or empty at .*_coverage_report\.dat$/);
  mkdirSync(path.join(out, '_coverage'));
  writeFileSync(path.join(out, '_coverage', '_coverage_report.dat'), '');
  await assert.rejects(combinedReport({ run: infoRun(out).run }), /missing or empty/);
});

test('combinedReport passes on a failed bazel info', async () => {
  const run = async () => {
    throw new Error('bazel exited with 2: ERROR: info: bad');
  };
  await assert.rejects(combinedReport({ run }), /^Error: bazel exited with 2/);
});

test('every config ARGS names is one .bazelrc defines, or the remote-cache action\'s', () => {
  const rc = readFileSync(fileURLToPath(new URL('../../../.bazelrc', import.meta.url)), 'utf8');
  for (const arg of ARGS.filter((a) => a.startsWith('--config='))) {
    const name = arg.slice('--config='.length);
    if (name === 'ci-remote') continue;
    assert.match(rc, new RegExp(`^(build|test|coverage|common):${name} `, 'm'), arg);
  }
  assert.ok(ARGS.includes('--config=ci-remote'), 'the remote cache');
});
