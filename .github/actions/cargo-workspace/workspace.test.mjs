import assert from 'node:assert/strict';
import { test } from 'node:test';

import { capture } from '../lib/testing.mjs';
import { discover, tagged, verifyMsrv, workspacePackages } from './workspace.mjs';

// --- Fixtures -----------------------------------------------------------------

// Packages in the shape `cargo metadata --no-deps` gives them, trimmed to the
// fields the module reads: a package with no [package.metadata] has null.
const PACKAGES = [
  {
    name: 'bdd-infra',
    manifest_path: '/w/crates/bdd-infra/Cargo.toml',
    rust_version: '1.99',
    metadata: null,
  },
  {
    name: 'filemonitor',
    manifest_path: '/w/services/filemonitor/Cargo.toml',
    rust_version: '1.99',
    metadata: {
      conformu: { command: 'cargo test -p filemonitor --test conformu_integration -- --ignored' },
      miri: { command: 'cargo miri test -p filemonitor' },
    },
  },
  {
    name: 'rp-auth',
    manifest_path: '/w/crates/rp-auth/Cargo.toml',
    rust_version: null,
    metadata: { miri: { command: 'cargo miri test -p rp-auth' } },
  },
  {
    name: 'ppba-driver',
    manifest_path: '/w/services/ppba-driver/Cargo.toml',
    rust_version: '1.99',
    metadata: { conformu: { command: 'cargo test -p ppba-driver --test conformu_integration -- --ignored' }, deb: { name: 'x' } },
  },
];

// A `run` that answers each call from `answers` in turn (a string it
// resolves to, or an Error it rejects with), recording what it was asked.
function fakeRun(answers = []) {
  const calls = [];
  const run = async (program, args, options) => {
    calls.push({ program, args, options });
    const answer = answers.shift() ?? '';
    if (answer instanceof Error) throw answer;
    return answer;
  };
  return { run, calls };
}

// --- workspacePackages ---------------------------------------------------------

test("workspacePackages reads the workspace's own packages from cargo metadata, keeping its JSON out of the log", async () => {
  const signal = new AbortController().signal;
  const fake = fakeRun([JSON.stringify({ packages: PACKAGES, workspace_root: '/w' })]);
  assert.deepEqual(await workspacePackages({ run: fake.run, signal }), PACKAGES);
  assert.deepEqual(fake.calls, [
    { program: 'cargo', args: ['metadata', '--format-version', '1', '--no-deps'], options: { echo: false, signal } },
  ]);
});

// --- tagged -------------------------------------------------------------------

test("tagged finds the packages a table gives a command, in the workspace's order", () => {
  assert.deepEqual(tagged(PACKAGES, 'conformu'), [
    { name: 'filemonitor', command: 'cargo test -p filemonitor --test conformu_integration -- --ignored' },
    { name: 'ppba-driver', command: 'cargo test -p ppba-driver --test conformu_integration -- --ignored' },
  ]);
  assert.deepEqual(tagged(PACKAGES, 'miri'), [
    { name: 'filemonitor', command: 'cargo miri test -p filemonitor' },
    { name: 'rp-auth', command: 'cargo miri test -p rp-auth' },
  ]);
});

test('tagged finds nothing for a table no package has, or one with no command', () => {
  assert.deepEqual(tagged(PACKAGES, 'loom'), []);
  assert.deepEqual(tagged(PACKAGES, 'deb'), []);
});

test('tagged refuses a command that is not a command line, naming the package', () => {
  for (const command of [['cargo', 'test'], '', '  ', false, 7]) {
    const packages = [{ name: 'broken', metadata: { conformu: { command } } }];
    assert.throws(() => tagged(packages, 'conformu'), {
      message: `broken's [package.metadata.conformu] command must be a command line, not ${JSON.stringify(command)}`,
    });
  }
});

test('discover logs what it found, and that it found nothing', async () => {
  const found = await capture(() => discover('miri', { run: fakeRun([JSON.stringify({ packages: PACKAGES })]).run }));
  assert.deepEqual(
    found.value.map((p) => p.name),
    ['filemonitor', 'rp-auth'],
  );
  assert.equal(
    found.stdout,
    '2 package(s) with a [package.metadata.miri] command:\n' +
      '  - filemonitor: cargo miri test -p filemonitor\n' +
      '  - rp-auth: cargo miri test -p rp-auth\n',
  );
  const none = await capture(() => discover('loom', { run: fakeRun([JSON.stringify({ packages: PACKAGES })]).run }));
  assert.deepEqual(none.value, []);
  assert.equal(none.stdout, '0 package(s) with a [package.metadata.loom] command:\n');
});

// --- verifyMsrv ---------------------------------------------------------------

test('verifyMsrv verifies each package that declares a rust-version by its own manifest, each in a log group', async () => {
  const signal = new AbortController().signal;
  const fake = fakeRun();
  const { value, stdout } = await capture(() => verifyMsrv(PACKAGES, { run: fake.run, signal }));
  assert.equal(value, 3);
  assert.deepEqual(fake.calls, [
    { program: 'cargo', args: ['msrv', 'verify', '--manifest-path', '/w/crates/bdd-infra/Cargo.toml'], options: { signal } },
    { program: 'cargo', args: ['msrv', 'verify', '--manifest-path', '/w/services/filemonitor/Cargo.toml'], options: { signal } },
    { program: 'cargo', args: ['msrv', 'verify', '--manifest-path', '/w/services/ppba-driver/Cargo.toml'], options: { signal } },
  ]);
  assert.equal(stdout, '::group::bdd-infra\n::endgroup::\n::group::filemonitor\n::endgroup::\n::group::ppba-driver\n::endgroup::\n');
});

test('verifyMsrv stops at the first package that fails, naming it, with cargo-msrv\'s error as the cause', async () => {
  const cause = new Error('cargo exited with 1: Check for toolchain 1.99 failed');
  const fake = fakeRun(['', cause]);
  await capture(() =>
    assert.rejects(verifyMsrv(PACKAGES, { run: fake.run }), (err) => {
      assert.equal(err.message, 'cargo msrv verify failed for filemonitor');
      assert.equal(err.cause, cause);
      return true;
    }),
  );
  assert.deepEqual(
    fake.calls.map((c) => c.args.at(-1)),
    ['/w/crates/bdd-infra/Cargo.toml', '/w/services/filemonitor/Cargo.toml'],
  );
});
