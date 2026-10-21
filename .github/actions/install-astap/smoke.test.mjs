import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { chmodSync, mkdirSync, mkdtempSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { test } from 'node:test';

import { capture } from '../lib/testing.mjs';
import { smoke } from './smoke.mjs';

// --- Fixtures -----------------------------------------------------------------

const onWindows = process.platform === 'win32';
const BANNER = 'ASTAP astrometric solver version CLI-2024.03.01\n';

function tempDir() {
  const base = process.env.TEST_TMPDIR ?? path.join(process.cwd(), '.tmp');
  mkdirSync(base, { recursive: true });
  return mkdtempSync(path.join(base, 'astap-smoke-'));
}

// A file where the installed CLI would be, with the mode `mode`.
function binary(mode = 0o755) {
  const file = path.join(tempDir(), onWindows ? 'astap_cli.exe' : 'astap_cli');
  writeFileSync(file, '');
  chmodSync(file, mode);
  return file;
}

// What the fake spawn starts in ASTAP's place: `node -e FAKE <answer>`, which
// prints the answer's stdout and stderr and exits with its code, or with
// `hang` runs until it is killed. The answer goes as base64 JSON, which no
// platform's quoting alters.
const FAKE = `
const { stdout, stderr, code, hang } = JSON.parse(Buffer.from(process.argv[1], 'base64').toString());
process.stdout.write(stdout);
process.stderr.write(stderr);
process.exitCode = code;
if (hang) setInterval(() => {}, 1000);
`;

// A spawn that starts the fake with `answer` ({ stdout, stderr, code, hang },
// all optional), recording what it was asked to start and the child.
function fakeSpawn(answer = {}) {
  const calls = [];
  const start = (cmd, args, options) => {
    const { stdout = '', stderr = '', code = 0, hang = false } = answer;
    const encoded = Buffer.from(JSON.stringify({ stdout, stderr, code, hang })).toString('base64');
    const child = spawn(process.execPath, ['-e', FAKE, encoded], options);
    calls.push({ cmd, args, options, child });
    return child;
  };
  return { spawn: start, calls };
}

const ENV = { PATH: process.env.PATH, SystemRoot: process.env.SystemRoot, KEPT: 'yes' };

// --- The checks ---------------------------------------------------------------

test('smoke runs the binary with no arguments and no stdin, logs what it printed, and passes on the banner', async () => {
  const file = binary();
  const fake = fakeSpawn({ stdout: BANNER, code: 1 });
  const { stdout } = await capture(() => smoke(file, { spawn: fake.spawn, env: ENV }));
  assert.equal(fake.calls.length, 1);
  const [{ cmd, args, options }] = fake.calls;
  assert.equal(cmd, file);
  assert.deepEqual(args, []);
  assert.deepEqual(options.stdio, ['ignore', 'pipe', 'pipe']);
  assert.equal(options.env, ENV);
  assert.equal(stdout, `Running ${file} (no arguments: ASTAP prints its banner and exits)\n${BANNER}`);
});

test('smoke finds the banner on stderr too, in any case, whatever the exit status', async () => {
  for (const answer of [{ stderr: 'astap_cli: usage\n', code: 0 }, { stdout: 'Astap\n', code: 255 }]) {
    await capture(() => smoke(binary(), { spawn: fakeSpawn(answer).spawn, env: ENV }));
  }
});

test('smoke fails when the output names no ASTAP', async () => {
  const fake = fakeSpawn({ stdout: 'Segmentation fault\n', stderr: 'core dumped\n' });
  const { stdout } = await capture(() =>
    assert.rejects(smoke(binary(), { spawn: fake.spawn, env: ENV }), { message: 'ASTAP banner not detected in output' }),
  );
  assert.match(stdout, /Segmentation fault\ncore dumped\n$/);
});

test('smoke fails when the action exported no ASTAP_BINARY, starting nothing', async () => {
  for (const unset of [undefined, '']) {
    const fake = fakeSpawn();
    await assert.rejects(smoke(unset, { spawn: fake.spawn }), { message: 'ASTAP_BINARY is not set: the install-astap action sets it' });
    assert.equal(fake.calls.length, 0);
  }
});

test('smoke fails on a binary that is not there, starting nothing', async () => {
  const missing = path.join(tempDir(), 'astap_cli');
  const fake = fakeSpawn();
  await assert.rejects(smoke(missing, { spawn: fake.spawn }), { message: `${missing} is not an executable file` });
  assert.equal(fake.calls.length, 0);
});

test('smoke fails on a binary without its execute bit, off Windows', { skip: onWindows && 'Windows has no execute bit' }, async () => {
  const file = binary(0o644);
  const fake = fakeSpawn();
  await assert.rejects(smoke(file, { spawn: fake.spawn }), { message: `${file} is not an executable file` });
  assert.equal(fake.calls.length, 0);
});

test('smoke fails with the spawn error when the binary cannot start', async () => {
  const start = (cmd, args, options) => spawn(path.join(tempDir(), 'rp-no-such-program'), args, options);
  await capture(() => assert.rejects(smoke(binary(), { spawn: start, env: ENV }), { code: 'ENOENT' }));
});

test('smoke stops the binary when its signal aborts', async () => {
  const stop = new AbortController();
  const fake = fakeSpawn({ hang: true });
  // Aborted once the child has started, not inside spawn, where Node would
  // emit the error before smoke listens for it.
  const start = (...args) => {
    const child = fake.spawn(...args);
    child.once('spawn', () => stop.abort());
    return child;
  };
  await capture(() => assert.rejects(smoke(binary(), { spawn: start, env: ENV, signal: stop.signal }), { name: 'AbortError' }));
  const [{ child }] = fake.calls;
  await new Promise((resolve) => (child.exitCode !== null || child.signalCode !== null ? resolve() : child.once('exit', resolve)));
  assert.ok(child.killed);
});
