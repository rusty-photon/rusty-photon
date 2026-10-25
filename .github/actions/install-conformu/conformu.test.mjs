import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, statSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { basename, dirname, join, win32 } from 'node:path';
import test from 'node:test';
import { gzipSync } from 'node:zlib';

import { INSTALLED } from '../lib/install.mjs';
import { capture, serve, tar } from '../lib/testing.mjs';
import { ASSETS, binaryIn, checkVersion, installAt, REPO, resolve, windowsDir } from './conformu.mjs';

// --- Fixtures -----------------------------------------------------------------

function tempDir() {
  return mkdtempSync(join(process.env.TEST_TMPDIR ?? tmpdir(), 'conformu-'));
}

const sha = (bytes) => createHash('sha256').update(bytes).digest('hex');

// A Linux release archive laid out as ConformU's are, flat and gzipped:
// conformu ("the checker\n", mode 755), appsettings.json and
// wwwroot/favicon.ico.
const ARCHIVE = gzipSync(
  tar([
    { path: 'conformu', data: 'the checker\n', mode: 0o755 },
    { path: 'appsettings.json', data: '{}\n' },
    { path: 'wwwroot/', type: '5', mode: 0o755 },
    { path: 'wwwroot/favicon.ico', data: 'icon\n' },
  ]),
);

const INSTALLER = 'ConformU.4.5.3+56334.Setup.exe';

// Each asset of a release, as the API lists it, and the bytes served for it.
const FILES = {
  'ConformU-Installer.4.5.3.dmg': Buffer.from('the disk image'),
  [INSTALLER]: Buffer.from('the installer'),
  'conformu.4.5.3.linux-arm.tar.gz': Buffer.from('the arm32 archive'),
  'conformu.4.5.3.linux-arm64.tar.gz': Buffer.from('the arm64 archive'),
  'conformu.4.5.3.linux-x64.tar.gz': ARCHIVE,
};

// The API's answer for the latest release: `tag`, and `assets`, each with the
// digest GitHub gives the bytes FILES serves for it.
function release({ tag = 'v4.5.3', assets = Object.keys(FILES), digest = (name) => `sha256:${sha(FILES[name] ?? '')}` } = {}) {
  return { tag_name: tag, assets: assets.map((name) => ({ name, digest: digest(name), size: FILES[name]?.length ?? 0 })) };
}

// One server for the API and the downloads, as api.github.com and github.com
// are: the latest release answers `latest`, and a download its FILES bytes.
async function github(latest = release()) {
  return serve((req, res) => {
    if (req.url === `/repos/${REPO}/releases/latest`) {
      res.setHeader('content-type', 'application/json');
      res.end(JSON.stringify(latest));
      return;
    }
    const body = FILES[decodeURIComponent(basename(req.url))];
    res.statusCode = body ? 200 : 404;
    res.end(body);
  });
}

const at = (server) => ({ api: server.url, server: server.url });

// An `execute` that records each program it is given, with what the file
// held when it ran, and answers --version with `version`, running nothing.
function executor(version = '4.5.3') {
  const calls = [];
  const execute = async (program, args, options) => {
    calls.push({ program, args, options, held: existsSync(program) ? readFileSync(program, 'utf8') : undefined });
    return args[0] === '--version' ? `\nConform Universal ${version} (Build 56334.1a2b3c4)\n` : '';
  };
  return { execute, calls };
}

// A `run` that records each command, with what an attached image held, and
// for ditto makes the copy it names with one file in it, unless `fail` says
// that command fails.
function runner({ fail } = {}) {
  const calls = [];
  const run = async (program, args) => {
    calls.push([program, ...args]);
    if (program === 'hdiutil' && args[0] === 'attach') calls.attached = readFileSync(args[1], 'utf8');
    if (program === fail) throw new Error(`${program} exited with 1: it failed`);
    if (program === 'ditto') {
      mkdirSync(args[1], { recursive: true });
      writeFileSync(join(args[1], 'Info.plist'), 'the app');
    }
    return '';
  };
  return { run, calls };
}

const fast = { delayMs: 1 };

// --- Resolving the release ------------------------------------------------------

test('resolve takes each runner its asset of the latest release, pinned to the SHA-256 GitHub gives', async () => {
  const server = await github();
  try {
    for (const [runner, kind, name] of [
      ['Linux-X64', 'archive', 'conformu.4.5.3.linux-x64.tar.gz'],
      ['Linux-ARM64', 'archive', 'conformu.4.5.3.linux-arm64.tar.gz'],
      ['macOS-ARM64', 'image', 'ConformU-Installer.4.5.3.dmg'],
      ['macOS-X64', 'image', 'ConformU-Installer.4.5.3.dmg'],
      ['Windows-X64', 'installer', INSTALLER],
    ]) {
      assert.deepEqual(await resolve({ runner, ...at(server) }), {
        version: '4.5.3',
        kind,
        asset: {
          name,
          url: `${server.url}/${REPO}/releases/download/v4.5.3/${encodeURIComponent(name)}`,
          pin: { sha256: sha(FILES[name]) },
        },
      });
    }
    assert.deepEqual(Object.keys(ASSETS), ['Linux-X64', 'Linux-ARM64', 'macOS-ARM64', 'macOS-X64', 'Windows-X64']);
  } finally {
    await server.close();
  }
});

test('resolve sends the token when it is given one, and goes unauthenticated without', async () => {
  const server = await github();
  try {
    await resolve({ runner: 'Linux-X64', token: 't', ...at(server) });
    await resolve({ runner: 'Linux-X64', ...at(server) });
    assert.deepEqual(
      server.requests.map(({ headers }) => headers.authorization),
      ['Bearer t', undefined],
    );
  } finally {
    await server.close();
  }
});

test('resolve refuses an asset without a SHA-256 from GitHub to check it against', async () => {
  for (const [digest, shown] of [
    [null, 'null'],
    ['md5:0123456789abcdef0123456789abcdef', '"md5:0123456789abcdef0123456789abcdef"'],
    [`sha256:${'A'.repeat(64)}`, `"sha256:${'A'.repeat(64)}"`],
  ]) {
    const server = await github(release({ digest: () => digest }));
    try {
      await assert.rejects(resolve({ runner: 'Linux-X64', ...at(server) }), {
        message:
          `ConformU v4.5.3's conformu.4.5.3.linux-x64.tar.gz has no SHA-256 from GitHub to check it against (its digest is ${shown}). ` +
          'GitHub gives one only for assets uploaded since mid-2025, and an unchecked install is not supported',
      });
    } finally {
      await server.close();
    }
  }
});

test('resolve fails when the release has no asset for the runner, or two installers', async () => {
  const server = await github(release({ assets: ['ConformU-Installer.4.5.3.dmg', INSTALLER, 'ConformU.4.5.3+1.Setup.exe'] }));
  try {
    await assert.rejects(resolve({ runner: 'Linux-ARM64', ...at(server) }), {
      message: `ConformU v4.5.3 should have one asset ${ASSETS['Linux-ARM64']} for Linux-ARM64; it has none`,
    });
    await assert.rejects(resolve({ runner: 'Windows-X64', ...at(server) }), {
      message: `ConformU v4.5.3 should have one asset ${ASSETS['Windows-X64']} for Windows-X64; it has ${INSTALLER}, ConformU.4.5.3+1.Setup.exe`,
    });
  } finally {
    await server.close();
  }
});

test('resolve refuses a runner ConformU has no asset for, without asking the API', async () => {
  const server = await github();
  try {
    await assert.rejects(resolve({ runner: 'Windows-ARM64', ...at(server) }), {
      message: 'ConformU has no asset for Windows-ARM64; it has one for Linux-X64, Linux-ARM64, macOS-ARM64, macOS-X64, Windows-X64',
    });
    assert.equal(server.requests.length, 0);
  } finally {
    await server.close();
  }
});

test('resolve reads the version from a tag with a build number, and refuses one that names none', async () => {
  let server = await github(release({ tag: 'v3.0.0+29642' }));
  try {
    const { version, asset } = await resolve({ runner: 'Linux-X64', ...at(server) });
    assert.equal(version, '3.0.0');
    assert.equal(asset.url, `${server.url}/${REPO}/releases/download/v3.0.0%2B29642/conformu.4.5.3.linux-x64.tar.gz`);
  } finally {
    await server.close();
  }
  for (const tag of ['latest', '4.5.3', 'v4.5', undefined]) {
    server = await github({ ...release(), tag_name: tag });
    try {
      await assert.rejects(resolve({ runner: 'Linux-X64', ...at(server) }), {
        message: `ConformU's latest release has the tag ${JSON.stringify(tag)}, which names no version`,
      });
    } finally {
      await server.close();
    }
  }
});

test('resolve says that reading the release failed, with what GitHub answered', async () => {
  const server = await serve((req, res) => {
    res.statusCode = 404;
    res.end('{"message":"Not Found"}');
  });
  try {
    await assert.rejects(resolve({ runner: 'Linux-X64', ...at(server) }), {
      message: 'Reading ConformU\'s latest release failed (GitHub answered: {"message":"Not Found"})',
    });
  } finally {
    await server.close();
  }
});

// --- Installing -----------------------------------------------------------------

test('installAt unpacks the archive into the directory, marks it, and checks its version', async () => {
  const server = await github();
  const parent = tempDir();
  const dir = join(parent, 'conformu');
  const { execute, calls } = executor();
  try {
    const resolved = await resolve({ runner: 'Linux-X64', ...at(server) });
    const { value } = await capture(() => installAt(dir, resolved, { download: fast, execute }));
    assert.deepEqual(value, { CONFORMU_PATH: join(dir, 'conformu') });
    assert.equal(readFileSync(join(dir, 'conformu'), 'utf8'), 'the checker\n');
    if (process.platform !== 'win32') assert.equal(statSync(join(dir, 'conformu')).mode & 0o777, 0o755);
    assert.equal(readFileSync(join(dir, 'wwwroot', 'favicon.ico'), 'utf8'), 'icon\n');
    assert.deepEqual(readdirSync(dir).sort(), [INSTALLED, 'appsettings.json', 'conformu', 'wwwroot'].sort());
    assert.equal(readFileSync(join(dir, INSTALLED), 'utf8'), 'ConformU 4.5.3, conformu.4.5.3.linux-x64.tar.gz\n');
    assert.deepEqual(readdirSync(parent), ['conformu']);
    assert.deepEqual(calls.map(({ program, args }) => [program, ...args]), [[join(dir, 'conformu'), '--version']]);
  } finally {
    await server.close();
  }
});

test('installAt replaces an earlier install, and leaves a directory of other files alone', async () => {
  const server = await github();
  const parent = tempDir();
  const dir = join(parent, 'conformu');
  const { execute } = executor();
  try {
    const resolved = await resolve({ runner: 'Linux-X64', ...at(server) });
    mkdirSync(dir);
    writeFileSync(join(dir, INSTALLED), 'ConformU 4.5.0\n');
    writeFileSync(join(dir, 'stale'), '');
    await capture(() => installAt(dir, resolved, { download: fast, execute }));
    assert.equal(existsSync(join(dir, 'stale')), false);
    assert.equal(existsSync(join(dir, 'conformu')), true);

    const other = join(parent, 'other');
    mkdirSync(other);
    writeFileSync(join(other, 'notes'), '');
    await assert.rejects(installAt(other, resolved, { download: fast, execute }), /holds files and no \.installed-by-rusty-photon/);
    assert.deepEqual(readdirSync(other), ['notes']);
  } finally {
    await server.close();
  }
});

test('installAt fails when the binary does not answer with the release version', async () => {
  const server = await github();
  const dir = join(tempDir(), 'conformu');
  try {
    const resolved = await resolve({ runner: 'Linux-X64', ...at(server) });
    await assert.rejects(capture(() => installAt(dir, resolved, { download: fast, execute: executor('4.5.0').execute })), {
      message: `${join(dir, 'conformu')} is ConformU 4.5.0, where 4.5.3 was installed`,
    });
  } finally {
    await server.close();
  }
});

test('installAt copies ConformU.app out of the mounted disk image, and detaches it', async () => {
  const server = await github();
  const parent = tempDir();
  const dir = join(parent, 'conformu');
  const { run, calls } = runner();
  const { execute, calls: executed } = executor();
  try {
    const resolved = await resolve({ runner: 'macOS-ARM64', ...at(server) });
    const { value } = await capture(() => installAt(dir, resolved, { download: fast, run, execute }));
    const binary = join(dir, 'ConformU.app', 'Contents', 'Resources', 'conformu');
    assert.deepEqual(value, { CONFORMU_PATH: binary });
    assert.equal(calls.length, 3);
    const [attach, copy, detach] = calls;
    const mount = attach.at(-1);
    assert.deepEqual(attach, ['hdiutil', 'attach', attach[2], '-readonly', '-nobrowse', '-noautoopen', '-mountpoint', mount]);
    assert.equal(basename(attach[2]), 'ConformU-Installer.4.5.3.dmg');
    assert.equal(calls.attached, 'the disk image');
    assert.deepEqual(copy, ['ditto', join(mount, 'ConformU.app'), join(dirname(mount), 'conformu', 'ConformU.app')]);
    assert.deepEqual(detach, ['hdiutil', 'detach', mount, '-force']);
    assert.equal(readFileSync(join(dir, 'ConformU.app', 'Info.plist'), 'utf8'), 'the app');
    assert.deepEqual(readdirSync(dir).sort(), [INSTALLED, 'ConformU.app'].sort());
    assert.deepEqual(readdirSync(parent), ['conformu']);
    assert.deepEqual(executed.map(({ program, args }) => [program, ...args]), [[binary, '--version']]);
  } finally {
    await server.close();
  }
});

test('installAt detaches the disk image when the copy fails, and leaves the directory as it was', async () => {
  const server = await github();
  const parent = tempDir();
  const dir = join(parent, 'conformu');
  const { run, calls } = runner({ fail: 'ditto' });
  try {
    const resolved = await resolve({ runner: 'macOS-X64', ...at(server) });
    await assert.rejects(capture(() => installAt(dir, resolved, { download: fast, run, execute: executor().execute })), {
      message: 'ditto exited with 1: it failed',
    });
    assert.deepEqual(calls.map(([program, verb]) => (program === 'hdiutil' ? `${program} ${verb}` : program)), ['hdiutil attach', 'ditto', 'hdiutil detach']);
    assert.deepEqual(readdirSync(parent), []);
  } finally {
    await server.close();
  }
});

test('installAt runs the Windows installer silently from a scratch directory, then checks the installed version', async () => {
  const server = await github();
  const { execute, calls } = executor();
  try {
    const resolved = await resolve({ runner: 'Windows-X64', ...at(server) });
    const { value } = await capture(() => installAt(windowsDir(), resolved, { download: fast, execute }));
    const binary = win32.join(windowsDir(), 'ConformU.exe');
    assert.deepEqual(value, { CONFORMU_PATH: binary });
    assert.equal(calls.length, 2);
    const [setup, version] = calls;
    assert.equal(basename(setup.program), INSTALLER);
    assert.equal(setup.held, 'the installer');
    assert.deepEqual(setup.args, ['/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/SP-']);
    assert.equal(existsSync(dirname(setup.program)), false);
    assert.deepEqual([version.program, ...version.args], [binary, '--version']);
  } finally {
    await server.close();
  }
});

test('installAt refuses to put the Windows install anywhere but where its installer installs', async () => {
  const server = await github();
  try {
    const resolved = await resolve({ runner: 'Windows-X64', ...at(server) });
    const requests = server.requests.length;
    await assert.rejects(installAt(join(tempDir(), 'conformu'), resolved, { download: fast, execute: executor().execute }), {
      message: new RegExp(`^ConformU's Windows installer installs into ${windowsDir().replaceAll('\\', '\\\\')} and nowhere else; it cannot install into `),
    });
    assert.equal(server.requests.length, requests);
    assert.equal(windowsDir({ ProgramFiles: 'D:\\Programs' }), 'D:\\Programs\\ASCOM\\ConformU');
    assert.equal(windowsDir({}), 'C:\\Program Files\\ASCOM\\ConformU');
  } finally {
    await server.close();
  }
});

// --- The binary -------------------------------------------------------------------

test('binaryIn names where each kind of install puts the binary', () => {
  assert.equal(binaryIn(join('/', 'opt', 'c'), 'archive'), join('/', 'opt', 'c', 'conformu'));
  assert.equal(binaryIn(join('/', 'opt', 'c'), 'image'), join('/', 'opt', 'c', 'ConformU.app', 'Contents', 'Resources', 'conformu'));
  assert.equal(binaryIn('C:\\Program Files\\ASCOM\\ConformU', 'installer'), 'C:\\Program Files\\ASCOM\\ConformU\\ConformU.exe');
});

test('checkVersion passes on the version it expects, with a time limit, and names what it got otherwise', async () => {
  const { execute, calls } = executor('4.5.3');
  await checkVersion('conformu', '4.5.3', { execute });
  assert.ok(calls[0].options.signal instanceof AbortSignal);
  await assert.rejects(checkVersion('conformu', '4.6.0', { execute }), { message: 'conformu is ConformU 4.5.3, where 4.6.0 was installed' });
  await assert.rejects(checkVersion('conformu', '4.5.3', { execute: async () => 'Usage: conformu [command]\n' }), {
    message: 'conformu --version printed no "Conform Universal <version>"',
  });
});
