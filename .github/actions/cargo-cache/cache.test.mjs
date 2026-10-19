import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { chmodSync, existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, statSync, utimesSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import test from 'node:test';

import { command, HttpError } from '../lib/helpers.mjs';
import { capture, serve, withEnv } from '../lib/testing.mjs';
import { cacheKey, main, parsePointer, pointerBody, post, removeWorkspaceOutputs, restore, save } from './cache.mjs';

// --- Fixtures -----------------------------------------------------------------

// The archives are GNU tar's, with zstd where the runner has it: the action
// runs on the Linux pool only.
const LINUX = { skip: process.platform !== 'linux' && 'the action runs on the Linux pool only, with GNU tar' };

const AUTH = 'Basic Y2k6cHc=';
const SHA = 'a'.repeat(40);
const OLD = 1153704088;

function tempDir() {
  return mkdtempSync(join(process.env.TEST_TMPDIR ?? tmpdir(), 'cargo-cache-'));
}

const sha256Hex = (bytes) => createHash('sha256').update(bytes).digest('hex');

function put(file, data, { mtime, mode } = {}) {
  mkdirSync(dirname(file), { recursive: true });
  writeFileSync(file, data);
  if (mode !== undefined) chmodSync(file, mode);
  if (mtime !== undefined) utimesSync(file, mtime, mtime);
}

// A runner after a cold build: a workspace whose target/ holds a
// dependency's outputs and the workspace crate `member`'s, and a CARGO_HOME
// with a git dependency.
function runner() {
  const root = tempDir();
  const workspace = join(root, 'ws');
  const cargoHome = join(root, 'cargo-home');
  const debug = join(workspace, 'target', 'debug');
  put(join(debug, '.fingerprint', 'dep-0123456789abcdef', 'lib-dep'), 'fp', { mtime: OLD });
  put(join(debug, 'build', 'dep-0123456789abcdef', 'build-script-build'), '#!/bin/sh\n', { mode: 0o755 });
  put(join(debug, 'deps', 'libdep-0123456789abcdef.rmeta'), 'dep rmeta', { mtime: OLD });
  put(join(debug, '.fingerprint', 'member-fedcba9876543210', 'lib-member'), 'fp');
  put(join(debug, 'deps', 'libmember-fedcba9876543210.rmeta'), 'member rmeta');
  put(join(debug, 'incremental', 'member-1abc', 's-1', 'dep-graph.bin'), 'graph');
  put(join(cargoHome, 'git', 'checkouts', 'alpaca-1', 'abc', 'src', 'lib.rs'), 'pub fn a() {}', { mtime: OLD });
  return { workspace, cargoHome, runnerTemp: join(root, 'temp') };
}

function emptyRunner() {
  const r = { workspace: join(tempDir(), 'ws'), cargoHome: join(tempDir(), 'cargo-home'), runnerTemp: tempDir() };
  mkdirSync(r.workspace);
  mkdirSync(r.cargoHome);
  return r;
}

const NAMES = async () => ({ packages: ['member'], targets: ['member'] });

// A bazel-remote HTTP cache as the action uses one, as v2.6.2 behaves:
// anonymous reads, writes with AUTH only; a CAS upload whose SHA-256 is not
// its key refused; a pointer (AC entry, JSON) read back only while every
// archive it names is held at its size, and given the writer's address as its
// worker unless it names one. `tamper` serves other bytes for every archive.
async function store({ casPut = 200, tamper = false } = {}) {
  const cas = new Map();
  const ac = new Map();
  const server = await serve((req, res) => {
    const [, kind, key] = req.url.split('/');
    if (req.method === 'PUT') {
      if (req.headers.authorization !== AUTH) return res.writeHead(401).end();
      if (kind === 'cas') {
        if (casPut !== 200) return res.writeHead(casPut).end();
        if (sha256Hex(req.body) !== key) return res.writeHead(500).end('checksums do not match');
        cas.set(key, req.body);
        return res.writeHead(200).end();
      }
      if (req.headers['content-type'] !== 'application/json') return res.writeHead(400).end();
      const entry = JSON.parse(req.body.toString());
      entry.executionMetadata = { worker: entry.executionMetadata?.worker || req.socket.remoteAddress };
      ac.set(key, entry);
      return res.writeHead(200).end();
    }
    if (kind === 'cas') {
      if (!cas.has(key)) return res.writeHead(404).end();
      return res.writeHead(200).end(tamper ? Buffer.from('not the archive') : cas.get(key));
    }
    const entry = ac.get(key);
    const whole = entry?.outputFiles?.every((f) => cas.get(f.digest.hash)?.length === Number(f.digest.sizeBytes));
    if (!whole) return res.writeHead(404).end();
    if (req.headers.accept !== 'application/json') return res.writeHead(200).end(Buffer.from([0x0a, 0x01]));
    return res.writeHead(200, { 'content-type': 'application/json' }).end(JSON.stringify(entry));
  });
  return { ...server, cas, ac };
}

const KEY = 'b'.repeat(64);

function saveOptions(s, r, extra = {}) {
  return {
    base: s.url,
    key: KEY,
    writeAuth: AUTH,
    ...r,
    tempDir: join(r.runnerTemp, 'cargo-cache'),
    ref: 'refs/heads/main',
    sha: SHA,
    run: '42',
    defaultBranch: 'main',
    head: async () => SHA,
    names: NAMES,
    ...extra,
  };
}

function restoreOptions(s, r, extra = {}) {
  return { base: s.url, key: KEY, workspace: r.workspace, cargoHome: r.cargoHome, tempDir: join(r.runnerTemp, 'cargo-cache'), ...extra };
}

// --- The key ------------------------------------------------------------------

test('cacheKey is a SHA-256 in hex, the same for the same runner, and changes with each part', () => {
  const parts = { job: 'clippy', rustc: 'rustc 1.99.0\n', system: 'abc', workspace: '/w', cargoHome: '/c', runnerTemp: '/t' };
  const key = cacheKey(parts);
  assert.match(key, /^[0-9a-f]{64}$/);
  assert.equal(cacheKey({ ...parts }), key);
  for (const [name, value] of Object.entries({ job: 'other', rustc: 'rustc 1.100.0', system: 'abd', workspace: '/x', cargoHome: '/d', runnerTemp: '/u' })) {
    assert.notEqual(cacheKey({ ...parts, [name]: value }), key, name);
  }
});

// --- The pointer --------------------------------------------------------------

const ARCHIVES = [
  { name: 'target', file: 'target.tar.zst', hash: 'c'.repeat(64), size: 300 },
  { name: 'git', file: 'git.tar', hash: 'd'.repeat(64), size: 20 },
];

test('parsePointer reads back what pointerBody writes, the commit and run included', () => {
  const body = JSON.parse(pointerBody(ARCHIVES, { commit: SHA, run: '42' }));
  assert.equal(body.executionMetadata.worker, 'cargo-cache');
  assert.deepEqual(parsePointer(body), {
    archives: [
      { name: 'target', file: 'target.tar.zst', compressed: true, hash: 'c'.repeat(64), size: 300 },
      { name: 'git', file: 'git.tar', compressed: false, hash: 'd'.repeat(64), size: 20 },
    ],
    commit: SHA,
    run: '42',
  });
});

test('parsePointer leaves out a note it cannot read, and refuses a pointer this action did not write', () => {
  const good = JSON.parse(pointerBody(ARCHIVES.slice(0, 1), {}));
  assert.deepEqual(parsePointer({ ...good, stdoutRaw: 'bm90IGpzb24=' }).commit, undefined);
  const file = good.outputFiles[0];
  const refused = [
    [{}, /names no archives/],
    [{ outputFiles: [] }, /names no archives/],
    [{ outputFiles: [{ ...file, path: '../escape.tar' }] }, /is not an archive this action writes/],
    [{ outputFiles: [file, file] }, /names target twice/],
    [{ outputFiles: [{ ...file, digest: { hash: 'C'.repeat(64), sizeBytes: '1' } }] }, /has no SHA-256/],
    [{ outputFiles: [{ ...file, digest: { hash: 'c'.repeat(64) } }] }, /has size ""/],
    [{ outputFiles: [{ ...file, digest: { hash: 'c'.repeat(64), sizeBytes: '-1' } }] }, /has size "-1"/],
  ];
  for (const [value, message] of refused) assert.throws(() => parsePointer(value), message, JSON.stringify(value));
});

// --- The workspace's own outputs ----------------------------------------------

test("removeWorkspaceOutputs removes the workspace's units and keeps every dependency's, one named like it included", async () => {
  const target = join(tempDir(), 'target');
  const debug = join(target, 'debug');
  for (const p of [
    ['.fingerprint', 'member-0123456789abcdef', 'x'],
    ['.fingerprint', 'member-extra-0123456789abcdef', 'x'],
    ['build', 'member-0123456789abcdef', 'out', 'x'],
    ['deps', 'libmember-0123456789abcdef.rmeta'],
    ['deps', 'member_bin-0123456789abcdef.d'],
    ['deps', 'member_extra-0123456789abcdef.d'],
    ['deps', 'libmember_extra-0123456789abcdef.rmeta'],
    ['incremental', 'member-1', 'x'],
    ['examples', 'demo-0123456789abcdef.d'],
  ]) {
    put(join(debug, ...p), 'x');
  }
  await removeWorkspaceOutputs(target, { packages: ['member'], targets: ['member', 'member-bin'] });
  assert.deepEqual(readdirSync(join(debug, '.fingerprint')), ['member-extra-0123456789abcdef']);
  assert.deepEqual(readdirSync(join(debug, 'build')), []);
  assert.deepEqual(readdirSync(join(debug, 'deps')).sort(), ['libmember_extra-0123456789abcdef.rmeta', 'member_extra-0123456789abcdef.d']);
  assert.equal(existsSync(join(debug, 'incremental')), false);
  assert.equal(existsSync(join(debug, 'examples')), false);
});

// --- Save, then restore --------------------------------------------------------

test('a save and a restore carry the dependencies over, mtimes and modes included, and not the workspace', LINUX, async (t) => {
  const s = await store();
  t.after(() => s.close());
  const writer = runner();

  const saved = await capture(() => save(saveOptions(s, writer)));
  assert.match(saved.stdout, /^Saved the build cache in \d+\.\d s: target \d+ MiB, git \d+ MiB/m);
  const pointer = s.ac.get(KEY);
  assert.equal(pointer.executionMetadata.worker, 'cargo-cache');
  assert.deepEqual(pointer.outputFiles.map((f) => f.path.replace(/\.zst$/, '')), ['target.tar', 'git.tar']);
  assert.equal(existsSync(join(writer.runnerTemp, 'cargo-cache')), false, 'the archives are removed after the upload');

  const reader = emptyRunner();
  const { workspace, cargoHome } = reader;
  const restored = await capture(() => restore(restoreOptions(s, reader)));
  assert.match(restored.stdout, new RegExp(`^Restored the build cache in \\d+\\.\\d s: \\d+ MiB, saved by ${SHA} \\(run 42\\)\\.$`, 'm'));

  const debug = join(workspace, 'target', 'debug');
  assert.equal(readFileSync(join(debug, 'deps', 'libdep-0123456789abcdef.rmeta'), 'utf8'), 'dep rmeta');
  assert.equal(statSync(join(debug, 'deps', 'libdep-0123456789abcdef.rmeta')).mtimeMs / 1000, OLD);
  assert.equal(statSync(join(debug, 'build', 'dep-0123456789abcdef', 'build-script-build')).mode & 0o111, 0o111);
  assert.equal(statSync(join(cargoHome, 'git', 'checkouts', 'alpaca-1', 'abc', 'src', 'lib.rs')).mtimeMs / 1000, OLD);
  assert.deepEqual(readdirSync(join(debug, '.fingerprint')), ['dep-0123456789abcdef']);
  assert.deepEqual(readdirSync(join(debug, 'deps')), ['libdep-0123456789abcdef.rmeta']);
  assert.equal(existsSync(join(debug, 'incremental')), false);
  assert.equal(existsSync(join(reader.runnerTemp, 'cargo-cache')), false);
});

test('without zstd the archives go uncompressed, and restore the same', LINUX, async (t) => {
  const s = await store();
  t.after(() => s.close());
  const writer = runner();
  const saved = await capture(() => save(saveOptions(s, writer, { zstd: 'no-such-zstd' })));
  assert.match(saved.stdout, /\(uncompressed: no zstd on this runner\)\.$/m);
  assert.deepEqual(s.ac.get(KEY).outputFiles.map((f) => f.path), ['target.tar', 'git.tar']);

  const reader = emptyRunner();
  await capture(() => restore(restoreOptions(s, reader)));
  assert.equal(readFileSync(join(reader.workspace, 'target', 'debug', 'deps', 'libdep-0123456789abcdef.rmeta'), 'utf8'), 'dep rmeta');
});

// --- Restore --------------------------------------------------------------------

test('no pointer yet is a cold build, said in the log, not a warning', async (t) => {
  const s = await store();
  t.after(() => s.close());
  const { stdout } = await capture(() => restore(restoreOptions(s, emptyRunner())));
  assert.equal(stdout, 'The LAN cache holds no build cache for this toolchain and runner yet: the build runs cold.\n');
  assert.deepEqual(s.requests.map((r) => `${r.method} ${r.url}`), [`GET /ac/${KEY}`]);
});

test('a store that refuses the connection is a cold build, with a warning that leaves its address out', async () => {
  const closed = await serve(() => {});
  await closed.close();
  const { stdout } = await capture(() => restore({ ...restoreOptions(closed, emptyRunner()) }));
  assert.equal(stdout, '::warning::The LAN cache failed (ECONNREFUSED); the build runs cold.\n');
});

test('a store that does not answer in time is a cold build', async (t) => {
  const silent = await serve(() => {});
  t.after(() => silent.close());
  const started = Date.now();
  const { stdout } = await capture(() => restore(restoreOptions(silent, emptyRunner(), { pointerTimeoutMs: 200 })));
  // Between the 200 ms timeout given and the 10 s default, well clear of the
  // first for a loaded runner (docs/skills/testing.md, on ceilings).
  assert.ok(Date.now() - started < 8_000, 'it waited for the default timeout, not the one given');
  assert.equal(stdout, '::warning::The LAN cache failed (no answer in time); the build runs cold.\n');
});

test('an archive whose bytes are not the ones its pointer names is not unpacked', LINUX, async (t) => {
  const s = await store({ tamper: true });
  t.after(() => s.close());
  await capture(() => save(saveOptions(s, runner())));
  const reader = emptyRunner();
  const { stdout } = await capture(() => restore(restoreOptions(s, reader)));
  assert.match(stdout, /^::warning::The build cache was not restored \(target\.tar(\.zst)?: 15 bytes with SHA-256 [0-9a-f]{64}, but the pointer names \d+ bytes with [0-9a-f]{64}\); the build runs cold\.$/m);
  assert.equal(existsSync(join(reader.workspace, 'target')), false);
  assert.equal(existsSync(join(reader.runnerTemp, 'cargo-cache')), false);
});

test('a restore that fails part way takes back what it unpacked', LINUX, async (t) => {
  const s = await store();
  t.after(() => s.close());
  await capture(() => save(saveOptions(s, runner())));
  // The git archive, the second the pointer names, is replaced on the store.
  const git = s.ac.get(KEY).outputFiles[1].digest.hash;
  s.cas.set(git, Buffer.alloc(s.cas.get(git).length));
  const reader = emptyRunner();
  const { stdout } = await capture(() => restore(restoreOptions(s, reader)));
  assert.match(stdout, /^::warning::The build cache was not restored \(git\.tar(\.zst)?: \d+ bytes with SHA-256 /m);
  assert.equal(existsSync(join(reader.workspace, 'target')), false, 'target/ was unpacked before git failed, and is taken back');
  assert.equal(existsSync(join(reader.cargoHome, 'git')), false);
});

test('an archive is unpacked only under its own root, whatever else it holds', LINUX, async (t) => {
  const s = await store();
  t.after(() => s.close());
  const dir = tempDir();
  put(join(dir, 'target', 'debug', 'kept'), 'k');
  put(join(dir, 'elsewhere', 'planted'), 'p');
  const file = join(dir, 'target.tar');
  await command('tar', ['-c', '-f', file, '-C', dir, 'target', 'elsewhere']);
  const bytes = readFileSync(file);
  s.cas.set(sha256Hex(bytes), bytes);
  s.ac.set(KEY, JSON.parse(pointerBody([{ file: 'target.tar', hash: sha256Hex(bytes), size: bytes.length }], {})));
  const reader = emptyRunner();
  await capture(() => restore(restoreOptions(s, reader)));
  assert.equal(readFileSync(join(reader.workspace, 'target', 'debug', 'kept'), 'utf8'), 'k');
  assert.equal(existsSync(join(reader.workspace, 'elsewhere')), false);
});

test('a pointer this action did not write is a cold build, with a warning', async (t) => {
  const s = await store();
  t.after(() => s.close());
  s.ac.set(KEY, { outputFiles: [{ path: 'other.bin', digest: { hash: 'e'.repeat(64), sizeBytes: '1' } }] });
  s.cas.set('e'.repeat(64), Buffer.from('x'));
  const { stdout } = await capture(() => restore(restoreOptions(s, emptyRunner())));
  assert.equal(stdout, '::warning::The build cache\'s pointer is not one this action wrote ("other.bin" is not an archive this action writes); the build runs cold.\n');
  assert.equal(s.requests.length, 1);
});

test('a target/ already there is left alone, and nothing is downloaded', LINUX, async (t) => {
  const s = await store();
  t.after(() => s.close());
  await capture(() => save(saveOptions(s, runner())));
  const reader = emptyRunner();
  mkdirSync(join(reader.workspace, 'target'));
  const before = s.requests.length;
  const { stdout } = await capture(() => restore(restoreOptions(s, reader)));
  assert.equal(stdout, `${join(reader.workspace, 'target')} already exists: the build cache is not restored.\n`);
  assert.deepEqual(s.requests.slice(before).map((r) => r.method + ' ' + r.url.split('/')[1]), ['GET ac']);
});

// --- Save -----------------------------------------------------------------------

test('a push to another branch saves nothing', async (t) => {
  const s = await store();
  t.after(() => s.close());
  const { stdout } = await capture(() => save(saveOptions(s, runner(), { ref: 'refs/heads/feature' })));
  assert.equal(stdout, "refs/heads/feature is not the default branch's: the build cache is not saved.\n");
  assert.equal(s.requests.length, 0);
});

test('a push that main has moved on from saves nothing, so an older build never replaces a newer one', async (t) => {
  const s = await store();
  t.after(() => s.close());
  const { stdout } = await capture(() => save(saveOptions(s, runner(), { head: async () => 'f'.repeat(40) })));
  assert.equal(stdout, `main has moved on to ${'f'.repeat(40)}: the build cache is left to that push.\n`);
  assert.equal(s.requests.length, 0);
});

test('a head that cannot be read saves nothing, with a warning', async (t) => {
  const s = await store();
  t.after(() => s.close());
  const head = async () => {
    throw new HttpError('GET', 'https://api.github.com/repos/o/r/git/ref/heads/main', 502, 'Bad Gateway');
  };
  const { stdout } = await capture(() => save(saveOptions(s, runner(), { head })));
  assert.equal(stdout, '::warning::The build cache was not saved: the head of main could not be read (HTTP 502).\n');
  assert.equal(s.requests.length, 0);
});

test('a refused upload writes no pointer, and is a warning, not a failure', LINUX, async (t) => {
  const s = await store({ casPut: 507 });
  t.after(() => s.close());
  const r = runner();
  const { stdout } = await capture(() => save(saveOptions(s, r)));
  assert.equal(stdout, '::warning::The build cache was not saved: the LAN cache failed (HTTP 507).\n');
  assert.deepEqual(s.requests.map((q) => q.method + ' ' + q.url.split('/')[1]), ['PUT cas']);
  assert.equal(s.ac.size, 0);
  assert.equal(existsSync(join(r.runnerTemp, 'cargo-cache')), false);
});

// --- The two steps ------------------------------------------------------------

function stateFile() {
  const file = join(tempDir(), 'state');
  writeFileSync(file, '');
  return file;
}

const IDENTIFY = async () => ({ rustc: 'rustc 1.99.0 (abc 2026-09-01)\nhost: x86_64-unknown-linux-gnu\n', system: '0123456789abcdef' });

test('main without a store URL does nothing', async () => {
  const { stdout } = await capture(() => main({ storeUrl: '', ...emptyRunner(), job: 'clippy', identifyRunner: IDENTIFY }));
  assert.equal(stdout, 'No LAN cache on this runner: the build runs cold.\n');
});

test('main with the write credential masks it first, keeps the key for the post step, and never reads the store', async (t) => {
  const s = await store();
  t.after(() => s.close());
  const file = stateFile();
  const r = emptyRunner();
  const { stdout } = await withEnv({ GITHUB_STATE: file }, () =>
    capture(() => main({ storeUrl: s.url, writeAuth: AUTH, ...r, job: 'clippy', identifyRunner: IDENTIFY })),
  );
  const lines = stdout.split('\n');
  assert.equal(lines[0], `::add-mask::${AUTH}`);
  assert.equal(lines[1], `::add-mask::${s.url}`);
  const key = cacheKey({ job: 'clippy', ...(await IDENTIFY()), ...r });
  assert.ok(lines.includes(`Build cache key ${key}: rustc 1.99.0 (abc 2026-09-01), system packages 0123456789ab.`), stdout);
  assert.ok(lines.includes('With the write credential the build runs cold, and the post step saves it if the job succeeds.'), stdout);
  assert.match(readFileSync(file, 'utf8'), new RegExp(`^key<<ghadelimiter_.*\\r?\\n${key}\\r?\\n`));
  assert.equal(s.requests.length, 0);
});

test('main without the write credential restores', async (t) => {
  const s = await store();
  t.after(() => s.close());
  const { stdout } = await withEnv({ GITHUB_STATE: stateFile() }, () =>
    capture(() => main({ storeUrl: s.url, ...emptyRunner(), job: 'clippy', identifyRunner: IDENTIFY })),
  );
  assert.match(stdout, /The LAN cache holds no build cache for this toolchain and runner yet/);
  assert.equal(s.requests.length, 1);
});

test('main that cannot name the cache warns, and leaves no key for the post step', async () => {
  const file = stateFile();
  const failing = async () => {
    throw Object.assign(new Error('spawn dpkg-query ENOENT'), { code: 'ENOENT' });
  };
  const { stdout } = await withEnv({ GITHUB_STATE: file }, () =>
    capture(() => main({ storeUrl: 'http://127.0.0.1:9', ...emptyRunner(), job: 'clippy', identifyRunner: failing })),
  );
  assert.match(stdout, /^::warning::The build cache cannot be named \(spawn dpkg-query ENOENT\); the build runs cold\.$/m);
  assert.equal(readFileSync(file, 'utf8'), '');
});

test('post saves only after a main step that had the write credential, on the default branch the event names', LINUX, async (t) => {
  const s = await store();
  t.after(() => s.close());
  const r = runner();
  const eventPath = join(tempDir(), 'event.json');
  writeFileSync(eventPath, JSON.stringify({ repository: { default_branch: 'main' } }));
  const github = await serve((req, res) => res.writeHead(200, { 'content-type': 'application/json' }).end(JSON.stringify({ object: { sha: SHA } })));
  t.after(() => github.close());
  const options = { storeUrl: s.url, ...r, eventPath, ref: 'refs/heads/main', sha: SHA, run: '7', apiUrl: github.url, repository: 'o/r', token: 't', names: NAMES };

  const none = await withEnv({ STATE_key: undefined }, () => capture(() => post({ ...options, writeAuth: AUTH })));
  assert.equal(none.stdout, '');
  const reader = await withEnv({ STATE_key: KEY }, () => capture(() => post({ ...options, writeAuth: '' })));
  assert.equal(reader.stdout, '');
  assert.equal(s.requests.length + github.requests.length, 0);

  const { stdout } = await withEnv({ STATE_key: KEY }, () => capture(() => post({ ...options, writeAuth: AUTH })));
  assert.match(stdout, /^Saved the build cache in /m);
  assert.deepEqual(github.requests.map((q) => `${q.url} ${q.headers.authorization}`), ['/repos/o/r/git/ref/heads/main Bearer t']);
  assert.ok(s.ac.has(KEY));
});
