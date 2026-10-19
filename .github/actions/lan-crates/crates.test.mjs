import assert from 'node:assert/strict';
import { existsSync, mkdtempSync, readFileSync, statSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import { gzipSync } from 'node:zlib';

import { capture, serve, tar } from '../lib/testing.mjs';
import { CRATES_IO, lanCrates, parseLock, replaceCratesIo, unpack } from './crates.mjs';
import { crateFile, lockfile, sha256Hex } from './fixtures.mjs';

// --- Fixtures -----------------------------------------------------------------

function tempDir() {
  return mkdtempSync(join(process.env.TEST_TMPDIR ?? tmpdir(), 'lan-crates-'));
}

// A bazel-remote HTTP CAS holding `blobs` (checksum → bytes); a PUT answers
// `putStatus` and, on 200, stores the blob.
function store(blobs, { putStatus = 200 } = {}) {
  return serve((req, res) => {
    const checksum = req.url.replace(/^\/cas\//, '');
    if (req.method === 'PUT') {
      if (putStatus === 200) blobs.set(checksum, req.body);
      res.writeHead(putStatus).end();
    } else if (blobs.has(checksum)) {
      res.writeHead(200).end(blobs.get(checksum));
    } else {
      res.writeHead(404).end();
    }
  });
}

// crates.io's download endpoint for `crates`.
function cratesIo(crates) {
  return serve((req, res) => {
    const crate = crates.find((c) => req.url === `/${c.name}/${c.version}/download`);
    if (crate) res.writeHead(200).end(crate.bytes);
    else res.writeHead(404).end();
  });
}

// A runner's view: Cargo.lock pinning `crates`, an empty CARGO_HOME, and
// where the action unpacks.
function workspace(crates) {
  const root = tempDir();
  const lockPath = join(root, 'Cargo.lock');
  writeFileSync(lockPath, lockfile(crates));
  return { lockPath, dir: join(root, 'vendor'), cargoHome: join(root, 'cargo-home') };
}

const ALPHA = crateFile('alpha', '1.0.0', { 'Cargo.toml': '[package]\nname = "alpha"\n', 'src/lib.rs': 'pub fn a() {}\n' });
const BETA = crateFile('beta', '0.2.1');

const OPTIONS = { delayMs: 1, concurrency: 4 };

function cargoConfig(cargoHome) {
  const file = join(cargoHome, 'config.toml');
  return existsSync(file) ? readFileSync(file, 'utf8') : undefined;
}

// --- Cargo.lock ---------------------------------------------------------------

test('parseLock returns the crates.io packages with their checksums, and leaves the rest to Cargo', () => {
  const text = lockfile(
    [ALPHA],
    '[[package]]\nname = "ascom-alpaca"\nversion = "1.0.0"\n' +
      'source = "git+https://github.com/example/ascom-alpaca-rs.git?branch=x#6f9fa597"\n' +
      'dependencies = [\n "alpha",\n]\n\n' +
      '[[package]]\nname = "rp"\nversion = "0.1.0"\ndependencies = [\n "alpha",\n "ascom-alpaca",\n]\n\n' +
      '[[patch.unused]]\nname = "unused"\nversion = "9.9.9"\n',
  );
  assert.deepEqual(parseLock(text), [{ name: 'alpha', version: '1.0.0', checksum: ALPHA.checksum }]);
});

test("parseLock reads this repository's Cargo.lock", () => {
  const text = readFileSync(fileURLToPath(new URL('../../../Cargo.lock', import.meta.url)), 'utf8');
  const crates = parseLock(text);
  const pinned = text.split('\n').filter((line) => line === `source = "${CRATES_IO}"`).length;
  assert.ok(pinned > 0, 'no crates.io package in Cargo.lock');
  assert.equal(crates.length, pinned);
});

test('parseLock refuses a lockfile version it does not read', () => {
  assert.throws(() => parseLock(lockfile([ALPHA]).replace('version = 4', 'version = 5')), /Cargo\.lock version 5: this action reads versions 3 and 4/);
  assert.throws(() => parseLock(lockfile([ALPHA]).replace('version = 4\n', '')), /Cargo\.lock version \(none\)/);
});

test('parseLock refuses a crates.io package without a checksum', () => {
  const text = lockfile([ALPHA]).replace(/^checksum = .*\n/m, '');
  assert.throws(() => parseLock(text), /alpha 1\.0\.0 has no SHA-256 checksum/);
});

test('parseLock refuses a name or version that could reach outside the directory', () => {
  assert.throws(() => parseLock(lockfile([{ ...ALPHA, name: '../alpha' }])), /"\.\.\/alpha" is not a crates\.io name/);
  assert.throws(() => parseLock(lockfile([{ ...ALPHA, version: '1.0/../..' }])), /alpha has version "1\.0\/\.\.\/\.\."/);
  assert.throws(() => parseLock(lockfile([{ ...ALPHA, version: '..' }])), /alpha has version "\.\."/);
});

test('parseLock refuses what Cargo does not write, rather than guess', () => {
  assert.throws(() => parseLock(lockfile([{ ...ALPHA, name: 'al\\"pha' }])), /line 6: name is not a plain string/);
  assert.throws(() => parseLock(`${lockfile([ALPHA])}\nname.dotted = "x"\n`), /line \d+ is not one this action reads/);
});

// --- Unpacking ------------------------------------------------------------------

test('unpack writes the crate under <name>-<version>/ with the checksum file a directory source needs', async () => {
  const dir = tempDir();
  await unpack(ALPHA.bytes, ALPHA, dir);
  assert.equal(readFileSync(join(dir, 'alpha-1.0.0', 'src', 'lib.rs'), 'utf8'), 'pub fn a() {}\n');
  assert.deepEqual(JSON.parse(readFileSync(join(dir, 'alpha-1.0.0', '.cargo-checksum.json'), 'utf8')), {
    files: {},
    package: ALPHA.checksum,
  });
});

test('unpack keeps an executable file executable', { skip: process.platform === 'win32' && 'Windows has no mode bits' }, async () => {
  const bytes = gzipSync(tar([{ path: 'gamma-1.0.0/configure', data: '#!/bin/sh\n', mode: 0o755 }, { path: 'gamma-1.0.0/README', data: 'r' }]));
  const dir = tempDir();
  await unpack(bytes, { name: 'gamma', version: '1.0.0', checksum: sha256Hex(bytes) }, dir);
  assert.equal(statSync(join(dir, 'gamma-1.0.0', 'configure')).mode & 0o111, 0o111);
  assert.equal(statSync(join(dir, 'gamma-1.0.0', 'README')).mode & 0o111, 0);
});

// `cargo package`'s fixed mtime (2006-07-23), and a later one.
const PACKAGED = 1153704088;
const LATER = 1700000000;

test('unpack gives each file its tar mtime, as Cargo does, and the directories and checksum file the newest', async () => {
  const bytes = gzipSync(
    tar([
      { path: 'gamma-1.0.0/build.rs', data: 'fn main() {}', mtime: PACKAGED },
      { path: 'gamma-1.0.0/src/deep/lib.rs', data: '', mtime: LATER },
      { path: 'gamma-1.0.0/empty/', type: '5', mtime: PACKAGED },
    ]),
  );
  const dir = tempDir();
  await unpack(bytes, { name: 'gamma', version: '1.0.0', checksum: sha256Hex(bytes) }, dir);
  const mtime = (...p) => statSync(join(dir, 'gamma-1.0.0', ...p)).mtimeMs / 1000;
  assert.equal(mtime('build.rs'), PACKAGED);
  assert.equal(mtime('src', 'deep', 'lib.rs'), LATER);
  for (const p of [[], ['src'], ['src', 'deep'], ['empty'], ['.cargo-checksum.json']]) {
    assert.equal(mtime(...p), LATER, p.join('/') || 'the crate directory');
  }
});

test('unpack refuses an entry outside <name>-<version>/, or one that climbs out of it', async () => {
  for (const path of ['other-1.0.0/x', 'gamma-1.0.0/../escaped', '/etc/x', 'gamma-1.0.0//x']) {
    const bytes = gzipSync(tar([{ path, data: 'x' }]));
    const dir = tempDir();
    await assert.rejects(unpack(bytes, { name: 'gamma', version: '1.0.0', checksum: sha256Hex(bytes) }, dir), /is not (under gamma-1\.0\.0\/|a plain relative path)/, path);
    assert.equal(existsSync(join(dir, 'escaped')), false);
  }
});

// --- The action -----------------------------------------------------------------

test('every crate from the store: crates.io is not asked, and Cargo reads the directory', async (t) => {
  const cas = await store(new Map([[ALPHA.checksum, ALPHA.bytes], [BETA.checksum, BETA.bytes]]));
  const registry = await cratesIo([ALPHA, BETA]);
  t.after(() => Promise.all([cas.close(), registry.close()]));
  const ws = workspace([ALPHA, BETA]);

  const { stdout } = await capture(() => lanCrates({ storeUrl: cas.url, ...ws, downloads: registry.url, ...OPTIONS }));

  assert.equal(registry.requests.length, 0);
  assert.deepEqual(cas.requests.map((r) => `${r.method} ${r.url}`).sort(), [`GET /cas/${ALPHA.checksum}`, `GET /cas/${BETA.checksum}`].sort());
  assert.match(stdout, /^2 crates from Cargo\.lock in [\d.]+ s: 2 from the LAN cache, 0 from crates\.io\.$/m);
  assert.doesNotMatch(stdout, /::warning::/);
  assert.equal(readFileSync(join(ws.dir, 'beta-0.2.1', 'Cargo.toml'), 'utf8'), '[package]\nname = "beta"\nversion = "0.2.1"\n');
  assert.equal(
    cargoConfig(ws.cargoHome),
    '# Written by the lan-crates action: Cargo reads the crates.io packages\n' +
      '# from a directory it filled and checked against Cargo.lock.\n' +
      '[source.crates-io]\nreplace-with = "lan-crates"\n\n' +
      `[source.lan-crates]\ndirectory = '${ws.dir}'\n`,
  );
});

test('a crate the store lacks comes from crates.io and, with the write credential, goes to the store', async (t) => {
  const blobs = new Map([[ALPHA.checksum, ALPHA.bytes]]);
  const cas = await store(blobs);
  const registry = await cratesIo([ALPHA, BETA]);
  t.after(() => Promise.all([cas.close(), registry.close()]));
  const ws = workspace([ALPHA, BETA]);

  const { stdout } = await capture(() =>
    lanCrates({ storeUrl: cas.url, writeAuth: 'Basic Y2k6cHc=', ...ws, downloads: registry.url, ...OPTIONS }),
  );

  assert.deepEqual(registry.requests.map((r) => r.url), ['/beta/0.2.1/download']);
  const puts = cas.requests.filter((r) => r.method === 'PUT');
  assert.equal(puts.length, 1);
  assert.equal(puts[0].url, `/cas/${BETA.checksum}`);
  assert.equal(puts[0].headers.authorization, 'Basic Y2k6cHc=');
  assert.deepEqual(puts[0].body, BETA.bytes);
  assert.match(stdout, /: 1 from the LAN cache, 1 from crates\.io, 1 of them uploaded to the LAN cache\.$/m);
  assert.match(stdout, /::group::Crates from crates\.io\nbeta 0\.2\.1\n::endgroup::/);
  assert.ok(existsSync(join(ws.dir, 'beta-0.2.1', '.cargo-checksum.json')));
  assert.match(cargoConfig(ws.cargoHome), /replace-with = "lan-crates"/);
});

test('without the write credential, nothing is uploaded', async (t) => {
  const cas = await store(new Map());
  const registry = await cratesIo([ALPHA]);
  t.after(() => Promise.all([cas.close(), registry.close()]));
  const ws = workspace([ALPHA]);

  const { stdout } = await capture(() => lanCrates({ storeUrl: cas.url, ...ws, downloads: registry.url, ...OPTIONS }));

  assert.deepEqual(cas.requests.map((r) => r.method), ['GET']);
  assert.match(stdout, /: 0 from the LAN cache, 1 from crates\.io\.$/m);
  assert.match(cargoConfig(ws.cargoHome), /replace-with = "lan-crates"/);
});

test('a store that fails is asked no more, and nothing is uploaded to it', async (t) => {
  const cas = await serve((req, res) => res.writeHead(500).end());
  const registry = await cratesIo([ALPHA, BETA]);
  t.after(() => Promise.all([cas.close(), registry.close()]));
  const ws = workspace([ALPHA, BETA]);

  const { stdout } = await capture(() =>
    lanCrates({ storeUrl: cas.url, writeAuth: 'Basic Y2k6cHc=', ...ws, downloads: registry.url, ...OPTIONS, concurrency: 1 }),
  );

  assert.deepEqual(cas.requests.map((r) => `${r.method} ${r.url}`), [`GET /cas/${ALPHA.checksum}`]);
  assert.equal(registry.requests.length, 2);
  assert.match(stdout, /::warning::The LAN cache failed \(HTTP 500\); the crates it had not served came from crates\.io\./);
  assert.match(stdout, /: 0 from the LAN cache, 2 from crates\.io, 0 of them uploaded to the LAN cache\.$/m);
  assert.match(cargoConfig(ws.cargoHome), /replace-with = "lan-crates"/);
});

test('a store that refuses the connection is a miss, and its address is masked first', async (t) => {
  const closed = await serve(() => {});
  await closed.close();
  const registry = await cratesIo([ALPHA]);
  t.after(() => registry.close());
  const ws = workspace([ALPHA]);
  const { host, hostname } = new URL(closed.url);

  const { stdout } = await capture(() => lanCrates({ storeUrl: `${closed.url}/`, ...ws, downloads: registry.url, ...OPTIONS }));

  const masks = [`::add-mask::${closed.url}`, `::add-mask::${host}`, `::add-mask::${hostname}`];
  const lines = stdout.split('\n');
  assert.deepEqual(lines.slice(0, 3), masks);
  assert.ok(stdout.includes('::warning::The LAN cache failed (ECONNREFUSED); '), stdout);
  // The address stays out of what is printed, masked or not: a connect
  // error names the address a host name resolved to, which no mask covers.
  assert.deepEqual(lines.slice(3).filter((l) => l.includes(hostname)), []);
  assert.match(cargoConfig(ws.cargoHome), /replace-with = "lan-crates"/);
});

test('a store that does not answer in time is a miss', async (t) => {
  const cas = await serve(() => {});
  const registry = await cratesIo([ALPHA]);
  t.after(() => Promise.all([cas.close(), registry.close()]));
  const ws = workspace([ALPHA]);

  const started = Date.now();
  const { stdout } = await capture(() =>
    lanCrates({ storeUrl: cas.url, ...ws, downloads: registry.url, ...OPTIONS, storeTimeoutMs: 200 }),
  );

  // Between the 200 ms timeout this test passes (about 0.25 s in all) and
  // the 10 s default, which nothing else here tells apart; well clear of
  // the first, for a loaded runner (docs/skills/testing.md, on ceilings).
  assert.ok(Date.now() - started < 8_000, 'it waited for the default timeout, not the one given');
  assert.match(stdout, /::warning::The LAN cache failed \(no answer in time\)/);
  assert.match(stdout, /: 0 from the LAN cache, 1 from crates\.io\.$/m);
});

test('bytes from the store that do not match Cargo.lock are a miss, and the store is still asked for the rest', async (t) => {
  const cas = await store(new Map([[ALPHA.checksum, Buffer.from('not alpha')], [BETA.checksum, BETA.bytes]]));
  const registry = await cratesIo([ALPHA, BETA]);
  t.after(() => Promise.all([cas.close(), registry.close()]));
  const ws = workspace([ALPHA, BETA]);

  const { stdout } = await capture(() => lanCrates({ storeUrl: cas.url, ...ws, downloads: registry.url, ...OPTIONS, concurrency: 1 }));

  assert.deepEqual(registry.requests.map((r) => r.url), ['/alpha/1.0.0/download']);
  assert.match(stdout, /::warning::The LAN cache served bytes that do not match Cargo\.lock for alpha 1\.0\.0; those came from crates\.io\./);
  assert.match(stdout, /: 1 from the LAN cache, 1 from crates\.io\.$/m);
  assert.equal(readFileSync(join(ws.dir, 'alpha-1.0.0', 'src', 'lib.rs'), 'utf8'), 'pub fn a() {}\n');
});

test('a crate crates.io serves wrong is not retried, and Cargo is left to download its crates itself', async (t) => {
  const cas = await store(new Map([[ALPHA.checksum, ALPHA.bytes]]));
  const registry = await cratesIo([ALPHA, { ...BETA, bytes: Buffer.from('tampered') }]);
  t.after(() => Promise.all([cas.close(), registry.close()]));
  const ws = workspace([ALPHA, BETA]);

  const { stdout } = await capture(() =>
    lanCrates({ storeUrl: cas.url, writeAuth: 'Basic Y2k6cHc=', ...ws, downloads: registry.url, ...OPTIONS }),
  );

  assert.equal(registry.requests.length, 1);
  assert.equal(cas.requests.filter((r) => r.method === 'PUT').length, 0);
  assert.match(stdout, /::warning::1 crates could not be had \(beta 0\.2\.1: beta 0\.2\.1: SHA-256 [0-9a-f]{64}, but Cargo\.lock pins [0-9a-f]{64}\); Cargo downloads its crates itself\./);
  assert.equal(cargoConfig(ws.cargoHome), undefined);
});

test('a crate that does not unpack leaves Cargo to download its crates itself', async (t) => {
  const bad = gzipSync(tar([{ path: 'gamma-1.0.0/link', type: '2' }]));
  const gamma = { name: 'gamma', version: '1.0.0', checksum: sha256Hex(bad), bytes: bad };
  const cas = await store(new Map([[gamma.checksum, bad]]));
  t.after(() => cas.close());
  const ws = workspace([gamma]);

  const { stdout } = await capture(() => lanCrates({ storeUrl: cas.url, ...ws, ...OPTIONS }));

  assert.match(stdout, /::warning::1 crates could not be had \(gamma 1\.0\.0: tar: "gamma-1\.0\.0\/link" is of type "2"/);
  assert.equal(cargoConfig(ws.cargoHome), undefined);
});

test('a transient failure at crates.io is retried', async (t) => {
  const cas = await store(new Map());
  const registry = await serve((req, res, n) => (n === 1 ? res.writeHead(503).end() : res.writeHead(200).end(ALPHA.bytes)));
  t.after(() => Promise.all([cas.close(), registry.close()]));
  const ws = workspace([ALPHA]);

  const { stdout } = await capture(() => lanCrates({ storeUrl: cas.url, ...ws, downloads: registry.url, ...OPTIONS }));

  assert.equal(registry.requests.length, 2);
  assert.match(stdout, /: 0 from the LAN cache, 1 from crates\.io\.$/m);
});

test('the first failed upload stops the uploads, and Cargo still reads the directory', async (t) => {
  const cas = await store(new Map(), { putStatus: 503 });
  const registry = await cratesIo([ALPHA, BETA]);
  t.after(() => Promise.all([cas.close(), registry.close()]));
  const ws = workspace([ALPHA, BETA]);

  const { stdout } = await capture(() =>
    lanCrates({ storeUrl: cas.url, writeAuth: 'Basic d3Jvbmc=', ...ws, downloads: registry.url, ...OPTIONS, concurrency: 1 }),
  );

  assert.equal(cas.requests.filter((r) => r.method === 'PUT').length, 1, 'uploads went on after one failed, or one was retried');
  assert.equal(cas.requests.filter((r) => r.method === 'GET').length, 2, 'a failed upload stopped the reads');
  assert.match(stdout, /::warning::An upload to the LAN cache failed \(HTTP 503\), so the uploads stopped: 0 of the 2 crates from crates\.io were uploaded\./);
  assert.match(cargoConfig(ws.cargoHome), /replace-with = "lan-crates"/);
});

test('an upload that gets no answer in time stops the uploads without holding up the rest', async (t) => {
  const cas = await serve((req, res) => {
    if (req.method === 'GET') res.writeHead(404).end();
  });
  const registry = await cratesIo([ALPHA, BETA]);
  t.after(() => Promise.all([cas.close(), registry.close()]));
  const ws = workspace([ALPHA, BETA]);

  const started = Date.now();
  const { stdout } = await capture(() =>
    lanCrates({ storeUrl: cas.url, writeAuth: 'Basic d3Jvbmc=', ...ws, downloads: registry.url, ...OPTIONS, concurrency: 1, storeTimeoutMs: 200 }),
  );

  // Between the 200 ms timeout this test passes and the 10 s default; the
  // PUT count below catches an upload retried or the uploads going on.
  assert.ok(Date.now() - started < 8_000, 'it waited for the default timeout, not the one given');
  assert.equal(cas.requests.filter((r) => r.method === 'PUT').length, 1);
  assert.match(stdout, /An upload to the LAN cache failed \(no answer in time\), so the uploads stopped/);
});

test('the write credential is masked before anything else is printed', async (t) => {
  const cas = await store(new Map());
  const registry = await cratesIo([ALPHA]);
  t.after(() => Promise.all([cas.close(), registry.close()]));
  const ws = workspace([ALPHA]);

  const { stdout } = await capture(() =>
    lanCrates({ storeUrl: cas.url, writeAuth: 'Basic c2VjcmV0', ...ws, downloads: registry.url, ...OPTIONS }),
  );

  const lines = stdout.split('\n');
  assert.equal(lines[0], '::add-mask::Basic c2VjcmV0');
  assert.equal(lines.filter((l) => l.includes('c2VjcmV0')).length, 1);
});

test('a Cargo.lock the action cannot read is a warning, and Cargo is left alone', async (t) => {
  const cas = await store(new Map([[ALPHA.checksum, ALPHA.bytes]]));
  t.after(() => cas.close());
  const ws = workspace([ALPHA]);
  writeFileSync(ws.lockPath, lockfile([ALPHA]).replace('version = 4', 'version = 5'));

  const { stdout } = await capture(() => lanCrates({ storeUrl: cas.url, ...ws, ...OPTIONS }));

  assert.match(stdout, /::warning::Cargo\.lock version 5: this action reads versions 3 and 4; Cargo downloads its crates itself\./);
  assert.equal(cas.requests.length, 0);
  assert.equal(cargoConfig(ws.cargoHome), undefined);
});

test('no store URL: nothing is fetched and Cargo is left alone', async () => {
  const ws = workspace([ALPHA]);
  const { stdout } = await capture(() => lanCrates({ storeUrl: '', ...ws, downloads: 'http://127.0.0.1:9', ...OPTIONS }));
  assert.equal(stdout, 'No LAN cache on this runner: Cargo downloads its crates itself.\n');
  assert.equal(existsSync(ws.dir), false);
  assert.equal(cargoConfig(ws.cargoHome), undefined);
});

test('a store URL that does not parse fails the step without showing it', async () => {
  const ws = workspace([ALPHA]);
  await assert.rejects(
    capture(() => lanCrates({ storeUrl: 'not a url', ...ws, ...OPTIONS })),
    (err) => /does not parse as a URL/.test(err.message) && !err.message.includes('not a url'),
  );
});

// --- Cargo's configuration ----------------------------------------------------------

test('replaceCratesIo keeps what config.toml already holds and adds the source after it', async () => {
  const cargoHome = tempDir();
  writeFileSync(join(cargoHome, 'config.toml'), '[net]\nretry = 5');
  assert.equal(await replaceCratesIo(cargoHome, '/runner/temp/lan-crates'), true);
  assert.match(cargoConfig(cargoHome), /^\[net\]\nretry = 5\n# Written by the lan-crates action/);
  assert.match(cargoConfig(cargoHome), /\[source\.lan-crates\]\ndirectory = '\/runner\/temp\/lan-crates'\n$/);
});

test('replaceCratesIo leaves a configuration that already names a source alone', async () => {
  const cargoHome = tempDir();
  const own = '[source.crates-io]\nreplace-with = "mirror"\n';
  writeFileSync(join(cargoHome, 'config.toml'), own);
  assert.equal(await replaceCratesIo(cargoHome, '/runner/temp/lan-crates'), false);
  assert.equal(cargoConfig(cargoHome), own);
});

test('replaceCratesIo leaves a source set as a dotted key, a quoted table or an inline table alone', async () => {
  for (const own of [
    'source.crates-io.replace-with = "mirror"\n',
    '[ "source".crates-io ]\nreplace-with = "mirror"\n',
    'source = { crates-io = { replace-with = "mirror" } }\n',
  ]) {
    const cargoHome = tempDir();
    writeFileSync(join(cargoHome, 'config.toml'), own);
    assert.equal(await replaceCratesIo(cargoHome, '/runner/temp/lan-crates'), false, own);
    assert.equal(cargoConfig(cargoHome), own);
  }
});

test('replaceCratesIo leaves CARGO_HOME alone when it has the older file `config`', async () => {
  const cargoHome = tempDir();
  writeFileSync(join(cargoHome, 'config'), '');
  assert.equal(await replaceCratesIo(cargoHome, '/runner/temp/lan-crates'), false);
  assert.equal(cargoConfig(cargoHome), undefined);
});

test('replaceCratesIo refuses a directory a TOML literal string cannot hold', async () => {
  await assert.rejects(replaceCratesIo(tempDir(), "/tmp/it's"), /cannot be written as a TOML literal string/);
});
