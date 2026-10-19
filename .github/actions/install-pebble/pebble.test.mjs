import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, statSync, writeFileSync } from 'node:fs';
import { EOL, tmpdir } from 'node:os';
import { basename, join } from 'node:path';
import test from 'node:test';
import { gzipSync } from 'node:zlib';

import { capture, serve, tar, withEnv } from '../lib/testing.mjs';
import { ASSETS, BINARIES, install, installAt, REPO, resolve, VERSION } from './pebble.mjs';

// --- Fixtures -----------------------------------------------------------------

function tempDir() {
  return mkdtempSync(join(process.env.TEST_TMPDIR ?? tmpdir(), 'pebble-'));
}

const sha = (bytes) => createHash('sha256').update(bytes).digest('hex');

// The runner like this machine, so the binaries carry the names this OS runs.
const RUNNER = process.platform === 'win32' ? 'Windows-X64' : 'Linux-X64';
const EXE = process.platform === 'win32' ? '.exe' : '';
const PLATFORM = ASSETS[RUNNER].platform;

// A release archive as Pebble's are laid out:
// <binary>-<platform>/<os>/<arch>/<binary>[.exe], and nothing else.
function release(binary, { data = `the ${binary}`, entries } = {}) {
  const [os, arch] = PLATFORM.split('-');
  const top = `${binary}-${PLATFORM}`;
  return gzipSync(
    tar(
      entries ?? [
        { path: `${top}/`, type: '5', mode: 0o755 },
        { path: `${top}/${os}/`, type: '5', mode: 0o755 },
        { path: `${top}/${os}/${arch}/`, type: '5', mode: 0o755 },
        { path: `${top}/${os}/${arch}/${binary}${EXE}`, data, mode: 0o644 },
      ],
    ),
  );
}

// A release server holding `archives` (file name to bytes), and resolve's
// assets for it, pinned to those bytes unless `pins` says otherwise.
async function releaseServer(archives = {
  [`pebble-${PLATFORM}.tar.gz`]: release('pebble'),
  [`pebble-challtestsrv-${PLATFORM}.tar.gz`]: release('pebble-challtestsrv'),
}, pins = {}) {
  const server = await serve((req, res) => {
    const body = archives[basename(req.url)];
    res.statusCode = body ? 200 : 404;
    res.end(body);
  });
  const assets = resolve({ runner: RUNNER, server: server.url }).assets.map((asset) => ({
    ...asset,
    pin: { sha256: pins[asset.name] ?? sha(archives[asset.name] ?? '') },
  }));
  return { server, assets };
}

// Runs `fn` with command files to write to, and the environment it changes
// put back after; resolves to what it wrote to each.
async function inStep(fn) {
  const dir = tempDir();
  const files = { GITHUB_PATH: join(dir, 'path'), GITHUB_ENV: join(dir, 'env') };
  for (const file of Object.values(files)) writeFileSync(file, '');
  const { stdout } = await withEnv({ ...files, PEBBLE_PATH: undefined, PEBBLE_CHALLTESTSRV_PATH: undefined }, () => capture(fn));
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

const fast = { delayMs: 1 };

// --- Choosing the assets --------------------------------------------------------

test('resolve gives each runner both binaries of its own platform, with their URLs and pins', () => {
  const platforms = {
    'Linux-X64': 'linux-amd64',
    'Linux-ARM64': 'linux-arm64',
    'macOS-ARM64': 'darwin-arm64',
    'macOS-X64': 'darwin-amd64',
    'Windows-X64': 'windows-amd64',
  };
  assert.deepEqual(Object.keys(ASSETS).sort(), Object.keys(platforms).sort());
  for (const [runner, platform] of Object.entries(platforms)) {
    const { assets } = resolve({ runner });
    assert.deepEqual(
      assets.map(({ name, url, pin }) => ({ name, url, pin })),
      ['pebble', 'pebble-challtestsrv'].map((binary) => ({
        name: `${binary}-${platform}.tar.gz`,
        url: `https://github.com/${REPO}/releases/download/${VERSION}/${binary}-${platform}.tar.gz`,
        pin: { sha256: ASSETS[runner].sha256[binary] },
      })),
      runner,
    );
  }
});

test('every runner pins both binaries, and nothing else', () => {
  for (const [runner, { sha256 }] of Object.entries(ASSETS)) {
    assert.deepEqual(Object.keys(sha256).sort(), BINARIES.map(({ name }) => name).sort(), runner);
    // The cache key checks each pin's form.
    resolve({ runner });
  }
});

test('resolve says where each archive holds its binary, and what locates it', () => {
  assert.deepEqual(
    resolve({ runner: 'Linux-ARM64' }).assets.map(({ file, inside, variable }) => ({ file, inside, variable })),
    [
      { file: 'pebble', inside: ['pebble-linux-arm64', 'linux', 'arm64', 'pebble'], variable: 'PEBBLE_PATH' },
      {
        file: 'pebble-challtestsrv',
        inside: ['pebble-challtestsrv-linux-arm64', 'linux', 'arm64', 'pebble-challtestsrv'],
        variable: 'PEBBLE_CHALLTESTSRV_PATH',
      },
    ],
  );
  assert.deepEqual(
    resolve({ runner: 'Windows-X64' }).assets.map(({ file, inside }) => [file, inside.at(-1)]),
    [
      ['pebble.exe', 'pebble.exe'],
      ['pebble-challtestsrv.exe', 'pebble-challtestsrv.exe'],
    ],
  );
});

test('resolve keys the cache on the version, each asset and its SHA-256', () => {
  const { sha256 } = ASSETS['Linux-X64'];
  assert.equal(
    resolve({ runner: 'Linux-X64' }).key,
    `install-pebble-${VERSION}-pebble-linux-amd64.tar.gz-${sha256.pebble}-pebble-challtestsrv-linux-amd64.tar.gz-${sha256['pebble-challtestsrv']}`,
  );
});

test('resolve refuses a runner Pebble has no build for', () => {
  assert.throws(() => resolve({ runner: 'Windows-ARM64' }), {
    message: 'Pebble has no asset for Windows-ARM64; it has one for Linux-X64, Linux-ARM64, macOS-ARM64, macOS-X64, Windows-X64',
  });
});

// --- Installing -------------------------------------------------------------------

test('install downloads, checks and unpacks both binaries into pebble/, and exports their paths', async () => {
  const { server, assets } = await releaseServer();
  const workspace = tempDir();
  try {
    const out = await inStep(() => install({ assets, workspace, cached: false, download: fast }));
    assert.equal(server.requests.length, 2);
    assert.deepEqual(readdirSync(workspace), ['pebble']);
    assert.deepEqual(readdirSync(join(workspace, 'pebble')).sort(), [`pebble${EXE}`, `pebble-challtestsrv${EXE}`].sort());
    assert.equal(readFileSync(join(workspace, 'pebble', `pebble${EXE}`), 'utf8'), 'the pebble');
    assert.equal(readFileSync(join(workspace, 'pebble', `pebble-challtestsrv${EXE}`), 'utf8'), 'the pebble-challtestsrv');
    if (process.platform !== 'win32') {
      assert.equal(statSync(join(workspace, 'pebble', 'pebble')).mode & 0o777, 0o755);
      assert.equal(statSync(join(workspace, 'pebble', 'pebble-challtestsrv')).mode & 0o777, 0o755);
    }
    assert.equal(envValue(out.env, 'PEBBLE_PATH'), `${workspace}/pebble/pebble${EXE}`);
    assert.equal(envValue(out.env, 'PEBBLE_CHALLTESTSRV_PATH'), `${workspace}/pebble/pebble-challtestsrv${EXE}`);
    assert.equal(out.path, '');
  } finally {
    await server.close();
  }
});

test('install takes what the cache restored, and fetches nothing', async () => {
  const { server, assets } = await releaseServer();
  const workspace = tempDir();
  mkdirSync(join(workspace, 'pebble'));
  writeFileSync(join(workspace, 'pebble', `pebble${EXE}`), 'cached');
  writeFileSync(join(workspace, 'pebble', `pebble-challtestsrv${EXE}`), 'cached');
  try {
    const out = await inStep(() => install({ assets, workspace, cached: true }));
    assert.equal(server.requests.length, 0);
    assert.match(out.stdout, /Pebble restored from the cache/);
    assert.equal(readFileSync(join(workspace, 'pebble', `pebble${EXE}`), 'utf8'), 'cached');
    assert.equal(envValue(out.env, 'PEBBLE_CHALLTESTSRV_PATH'), `${workspace}/pebble/pebble-challtestsrv${EXE}`);
  } finally {
    await server.close();
  }
});

test('install downloads again when the cache restored only one binary', async () => {
  const { server, assets } = await releaseServer();
  const workspace = tempDir();
  mkdirSync(join(workspace, 'pebble'));
  writeFileSync(join(workspace, 'pebble', `pebble${EXE}`), 'cached');
  try {
    const out = await inStep(() => install({ assets, workspace, cached: true, download: fast }));
    assert.equal(server.requests.length, 2);
    assert.match(out.stdout, new RegExp(`::warning::The cache restored .* without pebble-challtestsrv${EXE.replace('.', '\\.')}; downloading Pebble again`));
    assert.equal(readFileSync(join(workspace, 'pebble', `pebble${EXE}`), 'utf8'), 'the pebble');
  } finally {
    await server.close();
  }
});

test('install installs neither binary when the second fails its check', async () => {
  const { server, assets } = await releaseServer(undefined, { [`pebble-challtestsrv-${PLATFORM}.tar.gz`]: '0'.repeat(64) });
  const workspace = tempDir();
  try {
    await assert.rejects(
      inStep(() => install({ assets, workspace, cached: false, download: fast })),
      /pebble-challtestsrv-.*: SHA-256 mismatch/,
    );
    assert.deepEqual(readdirSync(workspace), []);
  } finally {
    await server.close();
  }
});

test('install refuses an archive without the binary where Pebble puts it', async () => {
  const misplaced = release('pebble', { entries: [{ path: `pebble${EXE}`, data: 'loose' }] });
  const { server, assets } = await releaseServer({
    [`pebble-${PLATFORM}.tar.gz`]: misplaced,
    [`pebble-challtestsrv-${PLATFORM}.tar.gz`]: release('pebble-challtestsrv'),
  });
  const workspace = tempDir();
  const [os, arch] = PLATFORM.split('-');
  try {
    await assert.rejects(inStep(() => install({ assets, workspace, cached: false, download: fast })), {
      message: `pebble-${PLATFORM}.tar.gz holds no pebble-${PLATFORM}/${os}/${arch}/pebble${EXE}`,
    });
    assert.deepEqual(readdirSync(workspace), []);
  } finally {
    await server.close();
  }
});

// --- A developer's install --------------------------------------------------------

test('installAt replaces the two binaries in a directory, keeps what else is there, and exports nothing', async () => {
  const { server, assets } = await releaseServer();
  const dir = tempDir();
  writeFileSync(join(dir, `pebble${EXE}`), 'older');
  writeFileSync(join(dir, 'other-tool'), 'mine');
  try {
    let variables;
    const out = await inStep(async () => {
      variables = await installAt(dir, { assets }, { download: fast });
    });
    assert.deepEqual(variables, {
      PEBBLE_PATH: join(dir, `pebble${EXE}`),
      PEBBLE_CHALLTESTSRV_PATH: join(dir, `pebble-challtestsrv${EXE}`),
    });
    assert.equal(readFileSync(join(dir, `pebble${EXE}`), 'utf8'), 'the pebble');
    assert.equal(readFileSync(join(dir, 'other-tool'), 'utf8'), 'mine');
    if (process.platform !== 'win32') assert.equal(statSync(join(dir, 'pebble-challtestsrv')).mode & 0o777, 0o755);
    assert.equal(out.env, '');
  } finally {
    await server.close();
  }
});

test('installAt creates the directory, and leaves no work directory beside it', async () => {
  const { server, assets } = await releaseServer();
  const parent = tempDir();
  try {
    await inStep(() => installAt(join(parent, 'opt', 'pebble'), { assets }, { download: fast }));
    assert.deepEqual(readdirSync(join(parent, 'opt')), ['pebble']);
    assert.equal(existsSync(join(parent, 'opt', 'pebble', `pebble-challtestsrv${EXE}`)), true);
  } finally {
    await server.close();
  }
});

test('installAt keeps the earlier binaries when the new ones fail their check', async () => {
  const { server, assets } = await releaseServer(undefined, { [`pebble-${PLATFORM}.tar.gz`]: '0'.repeat(64) });
  const dir = tempDir();
  writeFileSync(join(dir, `pebble${EXE}`), 'older');
  try {
    await assert.rejects(inStep(() => installAt(dir, { assets }, { download: fast })), /SHA-256 mismatch/);
    assert.deepEqual(readdirSync(dir), [`pebble${EXE}`]);
    assert.equal(readFileSync(join(dir, `pebble${EXE}`), 'utf8'), 'older');
  } finally {
    await server.close();
  }
});
