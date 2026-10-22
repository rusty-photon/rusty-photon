import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, statSync, symlinkSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { test } from 'node:test';

import { capture, serve, tar } from '../lib/testing.mjs';
import { ASSETS, LINTED, REPO, VERSION, install, lint, resolve, scripts } from './shellcheck.mjs';

function tempDir() {
  return mkdtempSync(path.join(process.env.TEST_TMPDIR ?? tmpdir(), 'shellcheck-'));
}

const sha = (bytes) => createHash('sha256').update(bytes).digest('hex');

// A release archive laid out as shellcheck's are: shellcheck-v0.11.0/ holding
// shellcheck ("the checker\n", mode 755), README.txt and LICENSE.txt, made by
// GNU tar and xz.
const ARCHIVE = Buffer.from(
  '/Td6WFoAAATm1rRGBMDLAYBQIQEWAAAAAAAAAKqA2/ngJ/8Aw10AOZoI1BF9jFwtPu5LSDQxGbTBHEcCbDQm5328set89QeKpOp9' +
    'F/p+3apIagZI++47xbcpXlaV5fqob3Z/b68xGQm2Vzgg3rDgFapq2F7yy4kqs3xoF3SWPYN+KFGU8+/te6n5z6zyFHzbceV88Czf' +
    'XSoLY4hhoYZEAZm3llAKiB1EyToUtPAhxVScke709C0kqVuBvruAENLJvvZSohAr1aeZuTRzSeVmdqcnZ/o5wFDe0efhmFc3q3Tm' +
    'CcS3fNixFISAAAD09u07qtSdLwAB5wGAUAAA3E6RdLHEZ/sCAAAAAARZWg==',
  'base64',
);

// What shellcheck 0.11.0 prints for --version.
const BANNER = 'ShellCheck - shell script analysis tool\nversion: 0.11.0\nlicense: GNU General Public License, version 3\nwebsite: https://www.shellcheck.net\n';

// The release served from loopback, as `name` in its asset, pinned to `pin`.
async function release(t, archive = ARCHIVE, { name = 'shellcheck-v0.11.0.linux.x86_64.tar.xz', pin = sha(archive) } = {}) {
  const server = await serve((req, res) => res.end(archive));
  t.after(server.close);
  return { server, asset: { name, url: `${server.url}/${name}`, pin: { sha256: pin } } };
}

// A `run` that answers --version with `banner` and records each call.
function fakeRun(banner = BANNER) {
  const calls = [];
  const run = async (program, args) => {
    calls.push([program, args]);
    return banner;
  };
  return { run, calls };
}

// --- The asset --------------------------------------------------------------------------

test('a Linux x64 runner takes the pinned release’s archive for its platform', () => {
  assert.deepEqual(resolve({ runner: 'Linux-X64' }), {
    name: 'shellcheck-v0.11.0.linux.x86_64.tar.xz',
    url: `https://github.com/${REPO}/releases/download/v0.11.0/shellcheck-v0.11.0.linux.x86_64.tar.xz`,
    pin: { sha256: ASSETS['Linux-X64'].sha256 },
  });
  assert.equal(VERSION, 'v0.11.0');
  assert.equal(ASSETS['Linux-X64'].sha256, '8c3be12b05d5c177a04c29e3c78ce89ac86f1595681cab149b65b97c4e227198');
});

test('a runner the table lacks is named', () => {
  assert.throws(() => resolve({ runner: 'macOS-ARM64' }), /^Error: shellcheck has no asset for macOS-ARM64; it has one for Linux-X64$/);
});

// --- The install ------------------------------------------------------------------------

test('install downloads, checks and unpacks the binary into its directory, and checks its version by its path', async (t) => {
  const { asset } = await release(t);
  const dir = path.join(tempDir(), 'shellcheck');
  const fake = fakeRun();
  const { value: file, stdout } = await capture(() => install(asset, dir, { run: fake.run, download: { delayMs: 1 } }));
  assert.equal(file, path.join(dir, 'shellcheck'));
  assert.equal(readFileSync(file, 'utf8'), 'the checker\n');
  if (process.platform !== 'win32') assert.equal(statSync(file).mode & 0o777, 0o755);
  assert.deepEqual(readdirSync(dir), ['shellcheck']);
  // Nothing of the download is left beside it.
  assert.deepEqual(readdirSync(path.dirname(dir)), ['shellcheck']);
  assert.deepEqual(fake.calls, [[file, ['--version']]]);
  assert.match(stdout, /^version: 0\.11\.0$/m);
});

test('a binary that reports another version, or none, is refused', async (t) => {
  for (const [banner, says] of [
    [BANNER.replace('0.11.0', '0.9.0'), 'version 0.9.0'],
    ['ShellCheck\n', 'no version'],
  ]) {
    const { asset } = await release(t);
    const dir = path.join(tempDir(), 'shellcheck');
    await assert.rejects(
      capture(() => install(asset, dir, { run: fakeRun(banner).run, download: { delayMs: 1 } })),
      new RegExp(`^Error: .*shellcheck reports ${says}, not v0\\.11\\.0$`),
    );
  }
});

test('an archive that fails its SHA-256 installs nothing', async (t) => {
  const { asset } = await release(t, ARCHIVE, { pin: '0'.repeat(64) });
  const dir = path.join(tempDir(), 'shellcheck');
  const fake = fakeRun();
  await assert.rejects(capture(() => install(asset, dir, { run: fake.run, download: { delayMs: 1 } })), /SHA-256/);
  assert.equal(existsSync(dir), false);
  assert.deepEqual(readdirSync(path.dirname(dir)), []);
  assert.equal(fake.calls.length, 0);
});

test('an archive without the binary where the release has it is refused', async (t) => {
  const archive = tar([{ path: 'shellcheck/shellcheck', data: 'elsewhere\n' }]);
  const { asset } = await release(t, archive, { name: 'shellcheck.tar' });
  const dir = path.join(tempDir(), 'shellcheck');
  await assert.rejects(
    capture(() => install(asset, dir, { run: fakeRun().run, download: { delayMs: 1 } })),
    /^Error: shellcheck\.tar holds no shellcheck-v0\.11\.0\/shellcheck$/,
  );
  assert.equal(existsSync(dir), false);
});

// --- The scripts ------------------------------------------------------------------------

// A checkout holding `files` (path → content); `links` are symbolic links to
// the first file.
function checkout(files, links = []) {
  const root = tempDir();
  for (const file of files) {
    mkdirSync(path.dirname(path.join(root, file)), { recursive: true });
    writeFileSync(path.join(root, file), '#!/bin/sh\n');
  }
  for (const link of links) symlinkSync(path.join(root, files[0]), path.join(root, link));
  return root;
}

test('it lints every *.sh under tools/ci and rp-set-hostname, by their paths from the checkout, sorted', async () => {
  const root = checkout([
    'tools/ci/rp-runner-pool.sh',
    'tools/ci/tests/test-free.sh',
    'tools/ci/runner-guest/rp-set-hostname',
    'tools/ci/rp-edac-check.sh',
    'tools/ci/beta_clippy_census.py',
    'tools/ci/rp-runner-pool.service',
    'tools/ci/runner-guest/rp-set-hostname.service',
    'scripts/elsewhere.sh',
  ]);
  mkdirSync(path.join(root, 'tools/ci/a-directory.sh'));
  assert.deepEqual(await scripts(root), [
    'tools/ci/rp-edac-check.sh',
    'tools/ci/rp-runner-pool.sh',
    'tools/ci/runner-guest/rp-set-hostname',
    'tools/ci/tests/test-free.sh',
  ]);
  assert.equal(LINTED, 'tools/ci');
});

test('a symbolic link is not linted', { skip: process.platform === 'win32' && 'a symbolic link needs a privilege on Windows' }, async () => {
  const root = checkout(['tools/ci/real.sh'], ['tools/ci/link.sh']);
  assert.deepEqual(await scripts(root), ['tools/ci/real.sh']);
});

test('lint runs shellcheck by its path over the scripts, and lists them first', async () => {
  const root = checkout(['tools/ci/b.sh', 'tools/ci/a.sh']);
  const calls = [];
  const { stdout } = await capture(() => lint({ shellcheck: '/opt/sc/shellcheck', root, run: async (...args) => calls.push(args) }));
  assert.deepEqual(calls, [['/opt/sc/shellcheck', ['tools/ci/a.sh', 'tools/ci/b.sh']]]);
  assert.equal(stdout, 'linting 2 script(s):\n  tools/ci/a.sh\n  tools/ci/b.sh\n');
});

test('a finding fails the lint', async () => {
  const root = checkout(['tools/ci/a.sh']);
  const run = async () => {
    throw new Error('shellcheck exited with 1');
  };
  await assert.rejects(capture(() => lint({ shellcheck: 'shellcheck', root, run })), /^Error: shellcheck exited with 1$/);
});

test('no script to lint is an error, not a pass', async () => {
  const root = checkout(['tools/ci/notes.md']);
  const calls = [];
  await assert.rejects(
    capture(() => lint({ shellcheck: 'shellcheck', root, run: async (...args) => calls.push(args) })),
    /^Error: found no script to lint under tools\/ci$/,
  );
  assert.equal(calls.length, 0);
});
