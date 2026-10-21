import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { mkdirSync, mkdtempSync, readFileSync } from 'node:fs';
import path from 'node:path';
import { test } from 'node:test';

import { replay, runStreams } from './processes.mjs';
import { capture } from './testing.mjs';

function tempDir() {
  const base = process.env.TEST_TMPDIR ?? path.join(process.cwd(), '.tmp');
  mkdirSync(base, { recursive: true });
  return mkdtempSync(path.join(base, 'processes-'));
}

// A program the fake spawn starts in a run's place: `node -e FAKE <what>
// <label>`. It prints its label and FAKE_SAY, tells the test it is ready,
// then does `what`: pass, fail, hang, stubborn (hang through SIGTERM), or
// parent (start a child that holds the pipe on fd 4 and writes to it only if
// it is still running 10 s later, then hang).
const FAKE = `
const [what, label] = process.argv.slice(1);
console.log(label + (process.env.FAKE_SAY ? ' ' + process.env.FAKE_SAY : ''));
if (what === 'stubborn') process.on('SIGTERM', () => {});
if (what === 'parent') {
  const child = require('node:child_process').spawn(process.execPath, ['-e', ${JSON.stringify(
    "setTimeout(() => { require('node:fs').writeSync(4, 'outlived the stop'); process.exit(0); }, 10000);",
  )}], { stdio: ['ignore', 'ignore', 'ignore', 'ignore', 4] });
  child.on('spawn', () => process.send('ready'));
} else {
  process.send('ready', () => {
    if (what === 'pass') process.exit(0);
    if (what === 'fail') process.exit(1);
  });
}
setInterval(() => {}, 1000);
`;

// A spawn that runs FAKE in place of each program, `fake:<what>:<label>`,
// recording what it was asked to start, and what was written to each child's
// fd 4 pipe. `ready` is called with each child once it has started and set
// itself up. The fake spawn throws for `fake:throw:<label>`, as spawn does on
// arguments it refuses.
function fakeSpawn({ ready } = {}) {
  const calls = [];
  const children = [];
  const written = [];
  const spawnFake = (program, args, options) => {
    calls.push({ program, args, options });
    const [, what, label] = program.split(':');
    if (what === 'throw') throw new TypeError(`cannot spawn ${program}`);
    const child = spawn(process.execPath, ['-e', FAKE, what, label, ...args], { ...options, stdio: [...options.stdio, 'ipc', 'pipe'] });
    children.push(child);
    // Read from the start: Node discards what an unread pipe gets once the
    // child exits.
    const i = written.push('') - 1;
    child.stdio[4].on('data', (chunk) => {
      written[i] += chunk;
    });
    child.on('message', () => ready?.(child));
    return child;
  };
  return { spawn: spawnFake, calls, children, written };
}

function fake(what, label, env = {}) {
  return { program: `fake:${what}:${label}`, args: [], env: { ...process.env, ...env } };
}

test('every stream runs its programs one after another, each with its own arguments and environment, and fails if one did', async () => {
  const logs = tempDir();
  const spawned = fakeSpawn();
  const planned = [
    { name: 'a', runs: [fake('fail', 'a1', { FAKE_SAY: 'first' }), { ...fake('pass', 'a2'), args: ['--x', 'y z'] }] },
    { name: 'b', runs: [fake('pass', 'b1')] },
  ];
  const result = await runStreams(planned, { logs, spawn: spawned.spawn });
  assert.deepEqual(result, { failed: ['a'], stopped: false });
  assert.equal(readFileSync(path.join(logs, 'a.log'), 'utf8'), 'a1 first\na2\n');
  assert.equal(readFileSync(path.join(logs, 'b.log'), 'utf8'), 'b1\n');
  assert.deepEqual(spawned.calls.map(({ program, args }) => [program, args]).toSorted(), [
    ['fake:fail:a1', []],
    ['fake:pass:a2', ['--x', 'y z']],
    ['fake:pass:b1', []],
  ]);
  for (const { program, options } of spawned.calls) {
    assert.equal(options.stdio[0], 'ignore', program);
    assert.equal(options.detached, process.platform !== 'win32', program);
  }
  assert.equal(spawned.calls.find(({ program }) => program === 'fake:fail:a1').options.env.FAKE_SAY, 'first');
});

test('a program that cannot start fails its stream, and its log says why', async () => {
  const logs = tempDir();
  const missing = path.join(logs, 'no-such-program');
  const planned = [{ name: 'rest', runs: [{ program: missing, args: [], env: { PATH: process.env.PATH } }] }];
  const result = await runStreams(planned, { logs });
  assert.deepEqual(result, { failed: ['rest'], stopped: false });
  assert.match(readFileSync(path.join(logs, 'rest.log'), 'utf8'), /no-such-program did not start: spawn .*ENOENT/);
});

test('a stopped run stops every program, starts no other, and keeps what each printed', async () => {
  const logs = tempDir();
  const stop = new AbortController();
  // Each stream's first program hangs, so both are running when the second is
  // ready: the streams run at once.
  let ready = 0;
  const spawned = fakeSpawn({ ready: () => ++ready === 2 && stop.abort() });
  const planned = [
    { name: 'a', runs: [fake('hang', 'a1'), fake('pass', 'a2')] },
    { name: 'b', runs: [fake('hang', 'b1')] },
  ];
  const result = await runStreams(planned, { logs, spawn: spawned.spawn, signal: stop.signal, graceMs: 600_000 });
  assert.deepEqual(result, { failed: ['a', 'b'], stopped: true });
  assert.deepEqual(spawned.calls.map(({ program }) => program).toSorted(), ['fake:hang:a1', 'fake:hang:b1']);
  for (const child of spawned.children) assert.equal(child.signalCode, 'SIGTERM', `pid ${child.pid}`);
  assert.equal(readFileSync(path.join(logs, 'a.log'), 'utf8'), 'a1\n');
});

test('a stop reaches the processes a program started', { skip: process.platform === 'win32' && 'Windows has no process group a signal reaches' }, async () => {
  const logs = tempDir();
  const stop = new AbortController();
  const spawned = fakeSpawn({ ready: () => stop.abort() });
  const result = await runStreams([{ name: 'a', runs: [fake('parent', 'a1')] }], { logs, spawn: spawned.spawn, signal: stop.signal, graceMs: 600_000 });
  // A child's 'close', which the run waits for, comes once its pipes have
  // closed: fd 4's once every process holding it has exited, the program and
  // the one it started, which writes to it if it outlives the stop.
  assert.deepEqual(result, { failed: ['a'], stopped: true });
  assert.equal(spawned.written[0], '');
});

test('streams stopped before they start run nothing', async () => {
  const stop = new AbortController();
  stop.abort();
  const spawned = fakeSpawn();
  const result = await runStreams([{ name: 'rest', runs: [fake('hang', 'r1')] }], { logs: tempDir(), spawn: spawned.spawn, signal: stop.signal });
  assert.deepEqual(result, { failed: ['rest'], stopped: true });
  assert.deepEqual(spawned.calls, []);
});

test('a program still running after the grace is killed', { skip: process.platform === 'win32' && 'a kill on Windows is never ignored' }, async () => {
  const stop = new AbortController();
  const spawned = fakeSpawn({ ready: () => stop.abort() });
  const result = await runStreams([{ name: 'rest', runs: [fake('stubborn', 'stuck')] }], { logs: tempDir(), spawn: spawned.spawn, signal: stop.signal, graceMs: 100 });
  assert.deepEqual(result, { failed: ['rest'], stopped: true });
  assert.equal(spawned.children[0].signalCode, 'SIGKILL');
});

test('a stream that throws stops the others, starts no other program, and throws', async () => {
  const spawned = fakeSpawn();
  const planned = [
    { name: 'a', runs: [fake('hang', 'a1'), fake('pass', 'a2')] },
    { name: 'b', runs: [fake('throw', 'b1')] },
  ];
  await assert.rejects(runStreams(planned, { logs: tempDir(), spawn: spawned.spawn, graceMs: 600_000 }), { message: 'cannot spawn fake:throw:b1' });
  assert.deepEqual(spawned.calls.map(({ program }) => program), ['fake:hang:a1', 'fake:throw:b1']);
  assert.equal(spawned.children[0].signalCode, 'SIGTERM');
});

test('each stream’s log is printed as a group under its title, an empty one for a stream that printed nothing or whose log never opened', async () => {
  const logs = tempDir();
  const planned = [{ name: 'a', runs: [fake('pass', 'a1')] }, { name: 'quiet', runs: [] }, { name: 'never-opened', runs: [] }];
  await runStreams(planned.slice(0, 2), { logs, spawn: fakeSpawn().spawn });
  const { stdout } = await capture(() => replay(planned, logs, (name) => `suite ${name}`));
  assert.equal(stdout, '::group::suite a\na1\n::endgroup::\n::group::suite quiet\n::endgroup::\n::group::suite never-opened\n::endgroup::\n');
});
