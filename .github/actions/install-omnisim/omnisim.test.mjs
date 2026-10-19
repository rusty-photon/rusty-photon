import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, statSync, writeFileSync } from 'node:fs';
import { EOL, tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import { gzipSync } from 'node:zlib';

import { INSTALLED } from '../lib/install.mjs';
import { capture, serve, tar, withEnv } from '../lib/testing.mjs';
import { ASSETS, install, installAt, REPO, resolve, VERSION } from './omnisim.mjs';

// --- Fixtures -----------------------------------------------------------------

function tempDir() {
  return mkdtempSync(join(process.env.TEST_TMPDIR ?? tmpdir(), 'omnisim-'));
}

const sha = (bytes) => createHash('sha256').update(bytes).digest('hex');

// The binary each OS runs, under the name install() is given.
const BINARY = process.platform === 'win32' ? 'ascom.alpaca.simulators.exe' : 'ascom.alpaca.simulators';

// A release archive as OmniSim's are laid out: one folder holding the binary
// and what it loads. A .tar.gz, which every OS's tar reads.
function release(entries = [
  { path: 'ascom.alpaca.simulators.test/', type: '5', mode: 0o755 },
  { path: `ascom.alpaca.simulators.test/${BINARY}`, data: 'the simulators', mode: 0o644 },
  { path: 'ascom.alpaca.simulators.test/appsettings.json', data: '{}' },
]) {
  return gzipSync(tar(entries));
}

// Runs `fn` with command files to write to, and the environment it changes
// put back after; resolves to what it wrote to each.
async function inStep(fn) {
  const dir = tempDir();
  const files = { GITHUB_PATH: join(dir, 'path'), GITHUB_ENV: join(dir, 'env') };
  for (const file of Object.values(files)) writeFileSync(file, '');
  const { stdout } = await withEnv({ ...files, PATH: process.env.PATH, OMNISIM_PATH: undefined }, () => capture(fn));
  return {
    stdout,
    path: readFileSync(files.GITHUB_PATH, 'utf8'),
    env: readFileSync(files.GITHUB_ENV, 'utf8'),
  };
}

// The value an entry in GITHUB_ENV's multi-line form sets.
function envValue(text, name) {
  const lines = text.split(EOL);
  const at = lines.findIndex((line) => line.startsWith(`${name}<<`));
  return at === -1 ? undefined : lines[at + 1];
}

async function releaseServer(archive) {
  return serve((req, res) => res.end(archive));
}

function testAsset(server, archive) {
  return { name: 'omnisim.tar.gz', url: `${server.url}/omnisim.tar.gz`, pin: { sha256: sha(archive) } };
}

// --- Choosing the asset ---------------------------------------------------------

test('resolve gives each runner its own release asset, URL and pin', () => {
  const expected = {
    'Linux-X64': 'ascom.alpaca.simulators.linux-x64.tar.xz',
    'Linux-ARM64': 'ascom.alpaca.simulators.linux-aarch64.tar.xz',
    'macOS-ARM64': 'ascom.alpaca.simulators.macos-arm64.zip',
    'macOS-X64': 'ascom.alpaca.simulators.macos-x64.zip',
    'Windows-X64': 'ascom.alpaca.simulators.windows-x64.zip',
  };
  assert.deepEqual(Object.keys(ASSETS).sort(), Object.keys(expected).sort());
  for (const [runner, name] of Object.entries(expected)) {
    const { asset } = resolve({ runner });
    assert.equal(asset.name, name, runner);
    assert.equal(asset.url, `https://github.com/${REPO}/releases/download/${VERSION}/${name}`, runner);
    assert.deepEqual(asset.pin, { sha256: ASSETS[runner].sha256 }, runner);
  }
});

test('resolve names the binary each OS runs', () => {
  assert.equal(resolve({ runner: 'Windows-X64' }).binary, 'ascom.alpaca.simulators.exe');
  assert.equal(resolve({ runner: 'Linux-ARM64' }).binary, 'ascom.alpaca.simulators');
  assert.equal(resolve({ runner: 'macOS-ARM64' }).binary, 'ascom.alpaca.simulators');
});

test('resolve keys the cache on the version, the asset and its SHA-256', () => {
  const { key } = resolve({ runner: 'Linux-X64' });
  assert.equal(key, `install-omnisim-${VERSION}-ascom.alpaca.simulators.linux-x64.tar.xz-${ASSETS['Linux-X64'].sha256}`);
});

test('resolve refuses a runner OmniSim has no build for', () => {
  assert.throws(() => resolve({ runner: 'Windows-ARM64' }), {
    message: 'OmniSim has no asset for Windows-ARM64; it has one for Linux-X64, Linux-ARM64, macOS-ARM64, macOS-X64, Windows-X64',
  });
});

// --- Installing -------------------------------------------------------------------

test('install downloads, checks and unpacks the release into omnisim/, and exports its paths', async () => {
  const archive = release();
  const server = await releaseServer(archive);
  const workspace = tempDir();
  try {
    const out = await inStep(() =>
      install({ asset: testAsset(server, archive), binary: BINARY, workspace, cached: false, download: { delayMs: 1 } }),
    );
    assert.equal(server.requests.length, 1);
    assert.equal(readFileSync(join(workspace, 'omnisim', BINARY), 'utf8'), 'the simulators');
    assert.equal(existsSync(join(workspace, 'omnisim', 'appsettings.json')), true);
    assert.deepEqual(readdirSync(workspace), ['omnisim']);
    if (process.platform !== 'win32') {
      assert.equal(statSync(join(workspace, 'omnisim', BINARY)).mode & 0o777, 0o755);
    }
    assert.equal(out.path, `${workspace}/omnisim${EOL}`);
    assert.equal(envValue(out.env, 'OMNISIM_PATH'), `${workspace}/omnisim/${BINARY}`);
  } finally {
    await server.close();
  }
});

test('install replaces what an older install left in omnisim/', async () => {
  const archive = release();
  const server = await releaseServer(archive);
  const workspace = tempDir();
  mkdirSync(join(workspace, 'omnisim'));
  writeFileSync(join(workspace, 'omnisim', 'stale'), '');
  try {
    await inStep(() =>
      install({ asset: testAsset(server, archive), binary: BINARY, workspace, cached: false, download: { delayMs: 1 } }),
    );
    assert.equal(existsSync(join(workspace, 'omnisim', 'stale')), false);
    assert.equal(existsSync(join(workspace, 'omnisim', BINARY)), true);
  } finally {
    await server.close();
  }
});

test('install takes what the cache restored, and fetches nothing', async () => {
  const server = await releaseServer(release());
  const workspace = tempDir();
  mkdirSync(join(workspace, 'omnisim'));
  writeFileSync(join(workspace, 'omnisim', BINARY), 'cached');
  try {
    const out = await inStep(() =>
      install({ asset: testAsset(server, release()), binary: BINARY, workspace, cached: true }),
    );
    assert.equal(server.requests.length, 0);
    assert.equal(readFileSync(join(workspace, 'omnisim', BINARY), 'utf8'), 'cached');
    assert.match(out.stdout, /OmniSim restored from the cache/);
    assert.equal(envValue(out.env, 'OMNISIM_PATH'), `${workspace}/omnisim/${BINARY}`);
  } finally {
    await server.close();
  }
});

test('install downloads again when the cache restored no binary', async () => {
  const archive = release();
  const server = await releaseServer(archive);
  const workspace = tempDir();
  mkdirSync(join(workspace, 'omnisim'));
  try {
    const out = await inStep(() =>
      install({ asset: testAsset(server, archive), binary: BINARY, workspace, cached: true, download: { delayMs: 1 } }),
    );
    assert.equal(server.requests.length, 1);
    assert.match(out.stdout, /::warning::The cache restored .* without ascom\.alpaca\.simulators/);
    assert.equal(readFileSync(join(workspace, 'omnisim', BINARY), 'utf8'), 'the simulators');
  } finally {
    await server.close();
  }
});

test('install refuses a release that does not match its pin, and installs nothing', async () => {
  const server = await releaseServer(release());
  const workspace = tempDir();
  try {
    const asset = { ...testAsset(server, release()), pin: { sha256: '0'.repeat(64) } };
    await assert.rejects(
      inStep(() => install({ asset, binary: BINARY, workspace, cached: false, download: { delayMs: 1 } })),
      /SHA-256 mismatch/,
    );
    assert.equal(existsSync(join(workspace, 'omnisim')), false);
  } finally {
    await server.close();
  }
});

test('install refuses a release without the binary, and installs nothing', async () => {
  const archive = release([
    { path: 'ascom.alpaca.simulators.test/', type: '5', mode: 0o755 },
    { path: 'ascom.alpaca.simulators.test/appsettings.json', data: '{}' },
  ]);
  const server = await releaseServer(archive);
  const workspace = tempDir();
  try {
    await assert.rejects(
      inStep(() =>
        install({ asset: testAsset(server, archive), binary: BINARY, workspace, cached: false, download: { delayMs: 1 } }),
      ),
      { message: `omnisim.tar.gz holds no ${BINARY} in ascom.alpaca.simulators.test` },
    );
    assert.equal(existsSync(join(workspace, 'omnisim')), false);
  } finally {
    await server.close();
  }
});

test('install refuses a release that does not unpack into one folder', async () => {
  const archive = release([
    { path: BINARY, data: 'loose' },
    { path: 'appsettings.json', data: '{}' },
  ]);
  const server = await releaseServer(archive);
  const workspace = tempDir();
  try {
    await assert.rejects(
      inStep(() =>
        install({ asset: testAsset(server, archive), binary: BINARY, workspace, cached: false, download: { delayMs: 1 } }),
      ),
      /expected the archive to hold one directory at its top/,
    );
    assert.equal(existsSync(join(workspace, 'omnisim')), false);
  } finally {
    await server.close();
  }
});

// --- A developer's install --------------------------------------------------------

test('installAt installs into a new directory and gives OMNISIM_PATH, exporting nothing', async () => {
  const archive = release();
  const server = await releaseServer(archive);
  const parent = tempDir();
  const dir = join(parent, 'opt', 'omnisim');
  try {
    const out = await inStep(async () => {
      const variables = await installAt(dir, { asset: testAsset(server, archive), binary: BINARY }, { download: { delayMs: 1 } });
      assert.deepEqual(variables, { OMNISIM_PATH: join(dir, BINARY) });
    });
    assert.equal(readFileSync(join(dir, BINARY), 'utf8'), 'the simulators');
    if (process.platform !== 'win32') assert.equal(statSync(join(dir, BINARY)).mode & 0o777, 0o755);
    assert.equal(readFileSync(join(dir, INSTALLED), 'utf8'), 'OmniSim v0.5.0-467.2, omnisim.tar.gz\n');
    assert.deepEqual(readdirSync(join(parent, 'opt')), ['omnisim']);
    assert.equal(out.env, '');
    assert.equal(out.path, '');
  } finally {
    await server.close();
  }
});

test('installAt replaces an earlier install, which it marked', async () => {
  const archive = release();
  const server = await releaseServer(archive);
  const dir = tempDir();
  writeFileSync(join(dir, BINARY), 'older');
  writeFileSync(join(dir, 'stale.dll'), '');
  writeFileSync(join(dir, INSTALLED), 'OmniSim v0.5.0-467.1\n');
  try {
    await inStep(() => installAt(dir, { asset: testAsset(server, archive), binary: BINARY }, { download: { delayMs: 1 } }));
    assert.equal(readFileSync(join(dir, BINARY), 'utf8'), 'the simulators');
    assert.equal(existsSync(join(dir, 'stale.dll')), false);
  } finally {
    await server.close();
  }
});

// A directory that holds OmniSim among other things, as ~/bin might, is not
// one an install of ours made, whatever it holds.
for (const [what, files] of [
  ['other files', ['notes.txt']],
  ['an OmniSim it did not install', [BINARY, 'notes.txt']],
]) {
  test(`installAt leaves a directory holding ${what} alone, and fetches nothing`, async () => {
    const archive = release();
    const server = await releaseServer(archive);
    const dir = tempDir();
    for (const file of files) writeFileSync(join(dir, file), 'mine');
    try {
      await assert.rejects(
        inStep(() => installAt(dir, { asset: testAsset(server, archive), binary: BINARY }, { download: { delayMs: 1 } })),
        /holds files and no \.installed-by-rusty-photon/,
      );
      assert.equal(server.requests.length, 0);
      assert.deepEqual(readdirSync(dir).sort(), [...files].sort());
    } finally {
      await server.close();
    }
  });
}

test('installAt keeps an earlier install when the new one fails its check', async () => {
  const server = await releaseServer(release());
  const dir = tempDir();
  writeFileSync(join(dir, BINARY), 'older');
  writeFileSync(join(dir, INSTALLED), 'OmniSim v0.5.0-467.1\n');
  try {
    const asset = { ...testAsset(server, release()), pin: { sha256: '0'.repeat(64) } };
    await assert.rejects(
      inStep(() => installAt(dir, { asset, binary: BINARY }, { download: { delayMs: 1 } })),
      /SHA-256 mismatch/,
    );
    assert.equal(readFileSync(join(dir, BINARY), 'utf8'), 'older');
  } finally {
    await server.close();
  }
});
