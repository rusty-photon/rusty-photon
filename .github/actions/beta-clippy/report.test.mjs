import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { mkdirSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { test } from 'node:test';

import { capture } from '../lib/testing.mjs';
import { CENSUS, PASSES, rendered, report } from './report.mjs';

function tempDir() {
  const base = process.env.TEST_TMPDIR ?? path.join(process.cwd(), '.tmp');
  mkdirSync(base, { recursive: true });
  return mkdtempSync(path.join(base, 'beta-clippy-'));
}

const VERSION = 'clippy 0.1.99 (0123456789 2026-10-14)';

// A program the fake spawn starts in each program's place: `node -e FAKE
// <program> <args>`. As `cargo clippy --version` it prints VERSION. As a
// clippy pass it writes a line to stderr and three JSON lines to stdout, one
// of them a diagnostic naming the pass, tells the test it is ready, then
// exits 101 if FAKE_FAIL names the pass, hangs if FAKE_HANG does, and passes
// otherwise. As the census it writes its table to --summary and, as its
// census, its toolchain and what it read; FAKE_CENSUS_FAIL fails it.
const FAKE = `
const fs = require('node:fs');
const [program, ...args] = process.argv.slice(1);
const end = (code) => {
  process.disconnect?.();
  process.exitCode = code;
};
if (program === 'cargo' && args[1] === '--version') {
  console.log(${JSON.stringify(VERSION)});
  end(0);
} else if (program === 'cargo') {
  const pass = args.includes('--all-targets') ? 'all-features' : 'default-features';
  console.error('    Checking the ' + pass + ' pass');
  console.log(JSON.stringify({ reason: 'compiler-artifact', target: { name: pass } }));
  console.log(JSON.stringify({ reason: 'compiler-message', message: { rendered: 'warning: from the ' + pass + ' pass\\n' } }));
  console.log(JSON.stringify({ reason: 'build-finished', success: true }));
  process.send('ready', () => {
    if (process.env.FAKE_HANG === pass) return setInterval(() => {}, 1000);
    end((process.env.FAKE_FAIL ?? '').split(',').includes(pass) ? 101 : 0);
  });
} else {
  const input = fs.readFileSync(0, 'utf8');
  fs.writeFileSync(args[args.indexOf('--summary') + 1], '## beta clippy census\\n');
  console.log(JSON.stringify({ toolchain: args[args.indexOf('--toolchain') + 1], lints: [], input }));
  end(process.env.FAKE_CENSUS_FAIL ? 1 : 0);
}
`;

function fakeSpawn({ ready } = {}) {
  const calls = [];
  const spawnFake = (program, args, options) => {
    calls.push({ program, args, options });
    const child = spawn(process.execPath, ['-e', FAKE, program, ...args], { ...options, stdio: [...options.stdio, 'ipc'] });
    child.on('message', () => ready?.(child));
    return child;
  };
  return { spawn: spawnFake, calls };
}

// Runs report in a fresh temp directory with `fake` and `env` over the
// runner's variables, and resolves to what it returned, printed and threw,
// the exit code it set, and its directories.
async function runReport(fake, { env = {}, ...options } = {}) {
  const dir = tempDir();
  const root = path.join(dir, 'checkout');
  const temp = path.join(dir, 'runner-temp');
  const summary = path.join(dir, 'summary.md');
  mkdirSync(root);
  writeFileSync(summary, 'an earlier step\n');
  const saved = process.exitCode;
  try {
    const { value, stdout, stderr } = await capture(() =>
      report({
        env: { ...process.env, GITHUB_WORKSPACE: root, RUNNER_TEMP: temp, GITHUB_STEP_SUMMARY: summary, ...env },
        spawn: fake.spawn,
        ...options,
      }).then(
        (census) => ({ census }),
        (error) => ({ error }),
      ),
    );
    return { ...value, stdout, stderr, exitCode: process.exitCode, root, temp, summary };
  } finally {
    process.exitCode = saved;
  }
}

// --- What clippy reported ----------------------------------------------------------

test('the diagnostics are shown as clippy renders them, and nothing else', () => {
  const lines = [
    JSON.stringify({ reason: 'compiler-artifact', target: { name: 'x' } }),
    JSON.stringify({ reason: 'compiler-message', message: { rendered: 'warning: one\n' } }),
    'not JSON',
    '{"cut short',
    JSON.stringify({ reason: 'compiler-message', message: { rendered: 'warning: two, with no line end' } }),
    JSON.stringify({ reason: 'compiler-message', message: { rendered: null } }),
    JSON.stringify({ reason: 'compiler-message', message: {} }),
    JSON.stringify({ reason: 'build-finished', success: true }),
    '',
  ];
  assert.equal(rendered(lines.join('\n')), 'warning: one\nwarning: two, with no line end\n');
  assert.equal(rendered(''), '');
});

// --- The run ----------------------------------------------------------------------------

test('runs both passes, shows what they reported, and adds their census to the summary', async () => {
  const fake = fakeSpawn();
  const run = await runReport(fake, { env: { ACTIONS_RUNTIME_TOKEN: 'cache-token', INPUT_TOKEN: 'input', KEPT: 'yes' } });
  assert.equal(run.error, undefined);
  assert.equal(run.exitCode, undefined);

  assert.deepEqual(
    fake.calls.map(({ program, args }) => [program, ...args]),
    [
      ['cargo', 'clippy', '--version'],
      ['cargo', ...PASSES[0]],
      ['cargo', ...PASSES[1]],
      ['python3', path.join(run.root, CENSUS), '--root', run.root, '--toolchain', VERSION, '--summary', path.join(run.temp, 'beta-clippy', 'census.md')],
    ],
  );
  assert.deepEqual(PASSES, [
    ['clippy', '--all-targets', '--all-features', '--message-format=json', '--', '--cap-lints', 'warn'],
    ['clippy', '--workspace', '--lib', '--bins', '--message-format=json', '--', '--cap-lints', 'warn'],
  ]);
  for (const { options } of fake.calls) {
    assert.equal(options.env.ACTIONS_RUNTIME_TOKEN, undefined);
    assert.equal(options.env.INPUT_TOKEN, undefined);
    assert.equal(options.env.KEPT, 'yes');
  }
  // stdin is the census's alone, the diagnostics' file.
  assert.equal(fake.calls[1].options.stdio[0], 'ignore');
  assert.equal(fake.calls[2].options.stdio[0], 'ignore');

  assert.equal(run.census, path.join(run.temp, 'beta-clippy', 'census.json'));
  const census = JSON.parse(readFileSync(run.census, 'utf8'));
  assert.equal(census.toolchain, VERSION);
  // The census read both passes' JSON, the first pass's first.
  assert.match(census.input, /"all-features"[^]*from the all-features pass[^]*"default-features"[^]*from the default-features pass/);
  assert.equal(readFileSync(run.summary, 'utf8'), 'an earlier step\n## beta clippy census\n');

  assert.match(run.stdout, new RegExp(`^${VERSION.replace(/[()]/g, '\\$&')}$`, 'm'));
  assert.match(run.stdout, /^warning: from the all-features pass\nwarning: from the default-features pass\n/m);
  assert.doesNotMatch(run.stdout, /compiler-artifact/);
  assert.match(run.stderr, /Checking the all-features pass[^]*Checking the default-features pass/);
});

test('a pass that fails fails the step and makes no census, after the other pass and what both reported', async () => {
  const fake = fakeSpawn();
  const run = await runReport(fake, { env: { FAKE_FAIL: 'all-features' } });
  assert.equal(run.error, undefined);
  assert.equal(run.census, undefined);
  assert.equal(run.exitCode, 1);
  assert.deepEqual(
    fake.calls.map(({ program, args }) => `${program} ${args[1]}`),
    ['cargo --version', 'cargo --all-targets', 'cargo --workspace'],
  );
  assert.match(run.stdout, /^::error::cargo clippy --all-targets --all-features --message-format=json -- --cap-lints warn exited with 101$/m);
  assert.doesNotMatch(run.stdout, /::error::cargo clippy --workspace/);
  assert.match(run.stdout, /from the all-features pass[^]*from the default-features pass/);
  assert.equal(readFileSync(run.summary, 'utf8'), 'an earlier step\n');
});

test('each failed pass is named', async () => {
  const run = await runReport(fakeSpawn(), { env: { FAKE_FAIL: 'all-features,default-features' } });
  assert.equal(run.exitCode, 1);
  assert.equal(run.stdout.match(/^::error::cargo clippy .* exited with 101$/gm).length, 2);
});

test('a census that fails throws, naming the script', async () => {
  const run = await runReport(fakeSpawn(), { env: { FAKE_CENSUS_FAIL: '1' } });
  assert.match(run.error?.message ?? '', /^tools\/ci\/beta_clippy_census\.py exited with 1$/);
  assert.equal(readFileSync(run.summary, 'utf8'), 'an earlier step\n');
});

test('a stop ends the pass running and runs nothing after it', async () => {
  const stop = new AbortController();
  const fake = fakeSpawn({ ready: () => stop.abort() });
  const run = await runReport(fake, { env: { FAKE_HANG: 'all-features' }, signal: stop.signal });
  assert.equal(run.census, undefined);
  assert.equal(run.exitCode, 1);
  assert.match(run.stdout, /^::error::beta clippy was stopped before it finished$/m);
  assert.deepEqual(
    fake.calls.map(({ args }) => args[1]),
    ['--version', '--all-targets'],
  );
});

test('a runner variable that is missing is named', async () => {
  for (const name of ['GITHUB_WORKSPACE', 'RUNNER_TEMP', 'GITHUB_STEP_SUMMARY']) {
    const fake = fakeSpawn();
    const run = await runReport(fake, { env: { [name]: '' } });
    assert.equal(run.error?.message, `${name} is not set; the runner sets it for every step`);
    assert.equal(fake.calls.length, 0);
  }
});
