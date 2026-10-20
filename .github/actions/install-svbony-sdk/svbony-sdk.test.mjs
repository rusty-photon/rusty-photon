import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { existsSync, lstatSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, readlinkSync, writeFileSync } from 'node:fs';
import { EOL, tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';

import { capture, serve, withEnv, zip } from '../lib/testing.mjs';
import {
  HEADER,
  INDI,
  install,
  installAt,
  MIRROR,
  NO_BUILD,
  PREFIX,
  REF,
  resolve,
  RUNNERS,
  WINDOWS_SDK_SHA256,
  WINDOWS_SDK_VERSION,
} from './svbony-sdk.mjs';

// --- Fixtures -----------------------------------------------------------------

function tempDir() {
  return mkdtempSync(join(process.env.TEST_TMPDIR ?? tmpdir(), 'svbony-sdk-'));
}

const sha = (bytes) => createHash('sha256').update(bytes).digest('hex');

const fast = { attempts: 1, delayMs: 1 };
const POSIX = { skip: process.platform === 'win32' && 'the path runs on Linux and macOS, with their install, cp and ln' };

// The files INDI's libsvbony/ folder serves at REF.
function indiFiles(arch) {
  return { [HEADER]: 'the header', [`libSVBCameraSDK_${arch}.bin`]: `${arch} camera` };
}

// A server holding `files` (a name under libsvbony/ to its text) where the
// INDI repository's raw links put them, and resolve's assets for `runner`
// there.
async function indiServer(runner, files = indiFiles(RUNNERS[runner].arch)) {
  const base = `/${INDI}/raw/${REF}/libsvbony/`;
  const server = await serve((req, res) => {
    const body = req.url.startsWith(base) ? files[req.url.slice(base.length)] : undefined;
    res.statusCode = body === undefined ? 404 : 200;
    res.end(body);
  });
  return { server, ...resolve({ runner, server: server.url }) };
}

// The vendor zip: the x64 import library and DLL, beside the x86 build and
// the header.
function vendorZip({ dll = true } = {}) {
  return zip([
    { path: 'SVBONY/include/SVBCameraSDK.h', data: 'header' },
    { path: 'SVBONY/lib/x86/SVBCameraSDK.dll', data: 'a 32-bit dll' },
    { path: 'SVBONY/lib/x86/SVBCameraSDK.lib', data: 'a 32-bit lib' },
    ...(dll ? [{ path: 'SVBONY/lib/x64/SVBCameraSDK.dll', data: 'camera dll' }] : []),
    { path: 'SVBONY/lib/x64/SVBCameraSDK.lib', data: 'camera lib' },
  ]);
}

const TOKEN = 'the-token';

// The private mirror serving `body` at the vendor zip's key to a request that
// carries TOKEN, or answering every request with `status`; and resolve's
// Windows assets pointed at it, pinned to `body` unless `pinned` keeps the
// real pin.
async function mirror(body = vendorZip(), { status, pinned = false } = {}) {
  const resolved = resolve({ runner: 'Windows-X64' });
  const [asset] = resolved.assets;
  const server = await serve((req, res) => {
    const allowed = req.headers.authorization === `Bearer ${TOKEN}`;
    res.statusCode = status ?? (!allowed ? 403 : req.url === `/svbony/${asset.name}` ? 200 : 404);
    res.end(res.statusCode === 200 ? body : undefined);
  });
  const url = asset.url.replace(MIRROR, server.url);
  const pin = pinned ? asset.pin : { sha256: sha(body) };
  return { server, ...resolved, assets: [{ ...asset, url, pin }] };
}

function writeAll(dir, files) {
  mkdirSync(dir, { recursive: true });
  for (const [name, data] of Object.entries(files)) writeFileSync(join(dir, name), data);
}

function readAll(dir) {
  return Object.fromEntries(readdirSync(dir).sort().map((name) => [name, readFileSync(join(dir, name), 'utf8')]));
}

// A `run` that records each command it is given and answers it from
// `answers` (a program to its output, or a function of its arguments to
// one), running none of them.
function recorder(answers = {}) {
  const calls = [];
  const run = async (program, args) => {
    calls.push([program, ...args]);
    const named = program === 'sudo' ? args[0] : program;
    const answer = answers[named] ?? '';
    return typeof answer === 'function' ? answer(program === 'sudo' ? args.slice(1) : args) : answer;
  };
  return { run, calls };
}

// The loader's cache (`ldconfig -p`) listing the library in `lib`, by both
// the names the install gives it.
function cacheListing(lib) {
  const files = ['libSVBCameraSDK.so.1', 'libSVBCameraSDK.so'];
  return `${files.length} libs found in cache\n${files.map((file) => `\t${file} (libc6,x86-64) => ${join(lib, file)}\n`).join('')}`;
}

// An ldconfig that lists the library in `lib` for -p, and does nothing else.
const listing = (lib) => (args) => (args[0] === '-p' ? cacheListing(lib) : '');

// The commands that copy the SDK from `from` into `prefix` on Linux, as root.
function copiesInto(prefix, from) {
  const lib = join(prefix, 'lib');
  const include = join(prefix, 'include');
  return [
    ['sudo', 'install', '-d', lib, include],
    ['sudo', 'cp', join(from, HEADER), join(include, HEADER)],
    ['sudo', 'cp', join(from, 'libSVBCameraSDK.so'), join(lib, 'libSVBCameraSDK.so.1')],
    ['sudo', 'ln', '-sf', 'libSVBCameraSDK.so.1', join(lib, 'libSVBCameraSDK.so')],
  ];
}

// Runs `fn` with command files to write to, and the environment it changes
// put back after, `env` set first; resolves to what it wrote to each.
async function inStep(fn, env = {}) {
  const dir = tempDir();
  const files = { GITHUB_PATH: join(dir, 'path'), GITHUB_ENV: join(dir, 'env') };
  for (const file of Object.values(files)) writeFileSync(file, '');
  const changed = { PATH: process.env.PATH, LD_LIBRARY_PATH: process.env.LD_LIBRARY_PATH, SVBONY_SDK_LIB_DIR: undefined };
  const { stdout, value } = await withEnv({ ...changed, ...files, ...env }, () => capture(fn));
  return { value, stdout, path: readFileSync(files.GITHUB_PATH, 'utf8'), env: readFileSync(files.GITHUB_ENV, 'utf8') };
}

// The value an entry in GITHUB_ENV's multi-line form sets.
function envValue(text, name) {
  const lines = text.split(EOL);
  const at = lines.findIndex((line) => line.startsWith(`${name}<<`));
  return at === -1 ? undefined : lines[at + 1];
}

// --- Choosing the files -------------------------------------------------------------

test('resolve takes the header and each runner\'s library from INDI\'s libsvbony folder at REF', () => {
  const base = `https://github.com/${INDI}/raw/${REF}/libsvbony`;
  assert.deepEqual(
    resolve({ runner: 'Linux-X64' }).assets.map(({ name, url, pin }) => [name, url, pin]),
    [
      ['SVBCameraSDK.h', `${base}/SVBCameraSDK.h`, { ref: REF }],
      ['libSVBCameraSDK.so', `${base}/libSVBCameraSDK_amd64.bin`, { ref: REF }],
    ],
  );
  assert.equal(resolve({ runner: 'Linux-ARM64' }).assets[1].url, `${base}/libSVBCameraSDK_armv8.bin`);
  const [, mac] = resolve({ runner: 'macOS-X64' }).assets;
  assert.deepEqual([mac.name, mac.url], ['libSVBCameraSDK.dylib', `${base}/libSVBCameraSDK_mac64.bin`]);
});

test('resolve keys the cache on the runner\'s build, the files and their one commit', () => {
  const { key } = resolve({ runner: 'Linux-X64' });
  assert.equal(key, `install-svbony-sdk-amd64-SVBCameraSDK.h-libSVBCameraSDK.so-${REF}`);
  assert.notEqual(resolve({ runner: 'Linux-ARM64' }).key, key);
});

test('resolve takes SVBony\'s SDK for Windows from the private mirror, against the original\'s SHA-256', () => {
  const { assets } = resolve({ runner: 'Windows-X64' });
  const name = `windows-SVBCameraSDK-v${WINDOWS_SDK_VERSION}.zip`;
  assert.deepEqual(assets, [{ name, url: `https://private.rustyphoton.space/svbony/${name}`, pin: { sha256: WINDOWS_SDK_SHA256 } }]);
});

test('resolve refuses Apple Silicon, saying what a job there does instead, and names the runners it has', () => {
  assert.throws(() => resolve({ runner: 'macOS-ARM64' }), { message: NO_BUILD['macOS-ARM64'] });
  assert.match(NO_BUILD['macOS-ARM64'], /mac64, for Intel, only\. .*SVBONY_SKIP_NATIVE_LINK=1$/);
  assert.throws(() => resolve({ runner: 'Windows-ARM64' }), {
    message: 'The SVBony SDK has no asset for Windows-ARM64; it has one for Linux-X64, Linux-ARM64, macOS-X64, Windows-X64',
  });
});

// --- Installing on Linux and macOS ----------------------------------------------------

test('install downloads into svbony-sdk/, installs the library as .so.1 with its link in /usr/local, and checks the loader finds it', async () => {
  const { server, runner, assets } = await indiServer('Linux-X64');
  const workspace = tempDir();
  const { run, calls } = recorder({ ldconfig: listing(join(PREFIX, 'lib')) });
  try {
    const out = await inStep(() => install({ runner, assets, workspace, sudo: true, cached: false, download: fast, run }));
    const base = `/${INDI}/raw/${REF}/libsvbony`;
    assert.deepEqual(server.requests.map(({ url }) => url), [`${base}/SVBCameraSDK.h`, `${base}/libSVBCameraSDK_amd64.bin`]);
    const downloads = join(workspace, 'svbony-sdk');
    assert.deepEqual(readAll(downloads), { 'SVBCameraSDK.h': 'the header', 'libSVBCameraSDK.so': 'amd64 camera' });
    assert.deepEqual(calls, [...copiesInto(PREFIX, downloads), ['sudo', 'ldconfig'], ['ldconfig', '-p']]);
    assert.equal(out.env, '');
    assert.equal(out.path, '');
  } finally {
    await server.close();
  }
});

test('install fails when the loader\'s cache does not list the library after ldconfig', async () => {
  const { server, runner, assets } = await indiServer('Linux-X64');
  const { run } = recorder({ ldconfig: (args) => (args[0] === '-p' ? '0 libs found in cache\n' : '') });
  const lib = join(PREFIX, 'lib');
  try {
    await assert.rejects(
      inStep(() => install({ runner, assets, workspace: tempDir(), sudo: true, cached: false, download: fast, run })),
      { message: `the loader's cache does not list libSVBCameraSDK.so in ${lib} after ldconfig: is ${lib} in /etc/ld.so.conf.d?` },
    );
  } finally {
    await server.close();
  }
});

test('install takes the files the cache restored, fetching nothing, and installs them', async () => {
  const { server, runner, assets } = await indiServer('Linux-ARM64');
  const workspace = tempDir();
  writeAll(join(workspace, 'svbony-sdk'), { 'SVBCameraSDK.h': 'cached', 'libSVBCameraSDK.so': 'cached' });
  const { run, calls } = recorder({ ldconfig: listing(join(PREFIX, 'lib')) });
  try {
    const out = await inStep(() => install({ runner, assets, workspace, sudo: true, cached: true, run }));
    assert.equal(server.requests.length, 0);
    assert.match(out.stdout, /restored from the cache/);
    assert.deepEqual(calls.slice(0, 4), copiesInto(PREFIX, join(workspace, 'svbony-sdk')));
  } finally {
    await server.close();
  }
});

test('install installs nothing when the library is missing upstream', async () => {
  const { server, runner, assets } = await indiServer('Linux-X64', { [HEADER]: 'the header' });
  const workspace = tempDir();
  const { run, calls } = recorder();
  try {
    await assert.rejects(inStep(() => install({ runner, assets, workspace, sudo: true, cached: false, download: fast, run })), /404/);
    assert.deepEqual(readdirSync(workspace), []);
    assert.deepEqual(calls, []);
  } finally {
    await server.close();
  }
});

test('install on macOS names the library by its path, and points it at a copy of libusb when it loads that by @rpath', async () => {
  const { server, runner, assets } = await indiServer('macOS-X64');
  const lib = join(PREFIX, 'lib');
  const target = join(lib, 'libSVBCameraSDK.dylib');
  const brew = join('/', 'usr', 'local', 'opt', 'libusb');
  const otool = `${target}:\n\t${target} (compatibility version 0.0.0)\n\t@rpath/libusb-1.0.0.dylib (compatibility version 4.0.0)\n`;
  const { run, calls } = recorder({ brew: `${brew}\n`, otool });
  try {
    await inStep(() => install({ runner, assets, workspace: tempDir(), sudo: false, cached: false, download: fast, run }));
    const libusb = join(lib, 'libusb-1.0.0.dylib');
    assert.deepEqual(calls.slice(2), [
      ['sudo', 'cp', calls[2][2], target],
      ['sudo', 'install_name_tool', '-id', target, target],
      ['otool', '-L', target],
      ['brew', '--prefix', 'libusb'],
      ['sudo', 'cp', join(brew, 'lib', 'libusb-1.0.0.dylib'), libusb],
      ['sudo', 'install_name_tool', '-change', '@rpath/libusb-1.0.0.dylib', libusb, target],
    ]);
  } finally {
    await server.close();
  }
});

test('install on macOS copies no libusb when the library does not load it by @rpath', async () => {
  const { server, runner, assets } = await indiServer('macOS-X64');
  const target = join(PREFIX, 'lib', 'libSVBCameraSDK.dylib');
  const { run, calls } = recorder({ otool: `${target}:\n\t/usr/lib/libc++.1.dylib (compatibility version 1.0.0)\n` });
  try {
    await inStep(() => install({ runner, assets, workspace: tempDir(), sudo: false, cached: false, download: fast, run }));
    assert.deepEqual(calls.at(-1), ['otool', '-L', target]);
    assert.ok(!calls.some(([program]) => program === 'brew'));
  } finally {
    await server.close();
  }
});

// --- Installing without sudo ------------------------------------------------------------

test('install without sudo stages the SDK under RUNNER_TEMP, links libusb\'s runtime, and points the build there', POSIX, async () => {
  const { server, runner, assets } = await indiServer('Linux-ARM64');
  const workspace = tempDir();
  const temp = tempDir();
  const system = tempDir();
  writeAll(system, { 'libusb-1.0.so.0': 'libusb' });
  const find = async (soname) => join(system, soname);
  try {
    const out = await inStep(
      () => install({ runner, assets, workspace, temp, sudo: false, cached: false, download: fast, find }),
      { LD_LIBRARY_PATH: '/opt/other' },
    );
    assert.deepEqual(readdirSync(workspace), []);
    assert.deepEqual(readdirSync(temp), ['svbony-sdk']);
    const lib = join(temp, 'svbony-sdk', 'lib');
    assert.deepEqual(readAll(join(temp, 'svbony-sdk', 'include')), { 'SVBCameraSDK.h': 'the header' });
    assert.deepEqual(readdirSync(lib).sort(), ['libSVBCameraSDK.so', 'libSVBCameraSDK.so.1', 'libusb-1.0.so']);
    assert.equal(readFileSync(join(lib, 'libSVBCameraSDK.so.1'), 'utf8'), 'armv8 camera');
    assert.equal(readlinkSync(join(lib, 'libSVBCameraSDK.so')), 'libSVBCameraSDK.so.1');
    assert.ok(lstatSync(join(lib, 'libusb-1.0.so')).isSymbolicLink());
    assert.equal(readlinkSync(join(lib, 'libusb-1.0.so')), join(system, 'libusb-1.0.so.0'));
    assert.equal(envValue(out.env, 'SVBONY_SDK_LIB_DIR'), lib);
    assert.equal(envValue(out.env, 'LD_LIBRARY_PATH'), `${lib}:/opt/other`);
  } finally {
    await server.close();
  }
});

test('install without sudo names the package to install when libusb\'s runtime is missing', POSIX, async () => {
  const { server, runner, assets } = await indiServer('Linux-ARM64');
  try {
    await assert.rejects(
      inStep(() => install({ runner, assets, workspace: tempDir(), temp: tempDir(), sudo: false, download: fast, find: async () => undefined })),
      /the system library libusb-1\.0\.so\.0 is not on this machine\. .*sudo apt-get install -y libusb-1\.0-0$/,
    );
  } finally {
    await server.close();
  }
});

// --- Installing on Windows ----------------------------------------------------------------

test('install on Windows fetches the vendor zip with the token, and stages its x64 import library and DLL', async () => {
  const { server, runner, assets } = await mirror();
  const workspace = tempDir();
  const temp = tempDir();
  try {
    const out = await inStep(() => install({ runner, assets, workspace, temp, token: TOKEN, download: fast }));
    assert.deepEqual(server.requests.map(({ url, headers }) => [url, headers.authorization]), [
      [`/svbony/windows-SVBCameraSDK-v${WINDOWS_SDK_VERSION}.zip`, `Bearer ${TOKEN}`],
    ]);
    const lib = join(temp, 'svbony-sdk', 'lib');
    assert.deepEqual(readAll(lib), { 'SVBCameraSDK.dll': 'camera dll', 'SVBCameraSDK.lib': 'camera lib' });
    assert.deepEqual(readdirSync(join(temp, 'svbony-sdk')), ['lib']);
    // Nothing for a cache to keep.
    assert.deepEqual(readdirSync(workspace), []);
    assert.equal(envValue(out.env, 'SVBONY_SDK_LIB_DIR'), lib);
    assert.equal(out.path, `${lib}${EOL}`);
    assert.match(out.stdout, new RegExp(`^::add-mask::${TOKEN}$`, 'm'));
  } finally {
    await server.close();
  }
});

test('install on Windows refuses to start without the token, and fetches nothing', async () => {
  const { server, runner, assets } = await mirror();
  try {
    await assert.rejects(
      inStep(() => install({ runner, assets, workspace: tempDir(), temp: tempDir(), token: '', download: fast })),
      /^Error: windows-mirror-token is empty\. .*SVBONY_SKIP_NATIVE_LINK=1/,
    );
    assert.equal(server.requests.length, 0);
  } finally {
    await server.close();
  }
});

test('install on Windows says to check the mirror and the token when the mirror refuses, and keeps an earlier install', async () => {
  const good = await mirror();
  const temp = tempDir();
  try {
    await inStep(() => install({ runner: good.runner, assets: good.assets, workspace: tempDir(), temp, token: TOKEN, download: fast }));
  } finally {
    await good.server.close();
  }
  const { server, runner, assets } = await mirror(undefined, { status: 403 });
  try {
    await assert.rejects(inStep(() => install({ runner, assets, workspace: tempDir(), temp, token: 'wrong', download: fast })), (err) => {
      assert.match(err.message, /^the private mirror did not serve svbony\/windows-SVBCameraSDK-v.*\.zip; check the mirror .* READ_TOKEN$/);
      assert.equal(err.cause.status, 403);
      return true;
    });
    assert.equal(readAll(join(temp, 'svbony-sdk', 'lib'))['SVBCameraSDK.dll'], 'camera dll');
  } finally {
    await server.close();
  }
});

test('install on Windows refuses a zip that is not the vendor\'s original', async () => {
  const { server, runner, assets } = await mirror(vendorZip(), { pinned: true });
  const temp = tempDir();
  try {
    await assert.rejects(inStep(() => install({ runner, assets, workspace: tempDir(), temp, token: TOKEN, download: fast })), (err) => {
      assert.match(err.message, /is not the vendor's original, whose SHA-256 this module pins; check the upload before trusting it$/);
      assert.equal(err.cause.name, 'ChecksumError');
      return true;
    });
    assert.equal(existsSync(join(temp, 'svbony-sdk', 'lib')), false);
  } finally {
    await server.close();
  }
});

test('install on Windows refuses a zip without the x64 DLL, and stages nothing', async () => {
  const { server, runner, assets } = await mirror(vendorZip({ dll: false }));
  const temp = tempDir();
  try {
    await assert.rejects(inStep(() => install({ runner, assets, workspace: tempDir(), temp, token: TOKEN, download: fast })), {
      message: `windows-SVBCameraSDK-v${WINDOWS_SDK_VERSION}.zip holds no SVBONY/lib/x64/SVBCameraSDK.dll: the vendor zip's layout changed`,
    });
    assert.deepEqual(readdirSync(join(temp, 'svbony-sdk')), []);
  } finally {
    await server.close();
  }
});

// --- A developer's install ----------------------------------------------------------------

test('installAt copies into a prefix the developer can write without sudo, and names its lib folder for the build and the loader', POSIX, async () => {
  const { server, runner, assets } = await indiServer('Linux-X64');
  const prefix = tempDir();
  writeAll(join(prefix, 'lib'), { 'libother.so': 'mine', 'libSVBCameraSDK.so.1': 'older' });
  try {
    const out = await inStep(() => installAt(prefix, { runner, assets }, { download: fast }));
    const lib = join(prefix, 'lib');
    assert.deepEqual(out.value, { SVBONY_SDK_LIB_DIR: lib, LD_LIBRARY_PATH: lib });
    assert.deepEqual(readdirSync(lib).sort(), ['libSVBCameraSDK.so', 'libSVBCameraSDK.so.1', 'libother.so']);
    assert.equal(readFileSync(join(lib, 'libSVBCameraSDK.so'), 'utf8'), 'amd64 camera');
    assert.equal(readFileSync(join(lib, 'libother.so'), 'utf8'), 'mine');
    assert.deepEqual(readAll(join(prefix, 'include')), { 'SVBCameraSDK.h': 'the header' });
    assert.doesNotMatch(out.stdout, /sudo/);
    assert.equal(out.env, '');
  } finally {
    await server.close();
  }
});

test('installAt writes /usr/local with sudo where the developer cannot, and needs no variable where the loader looks', async () => {
  const { server, runner, assets } = await indiServer('Linux-X64');
  const files = [];
  // The copies come from the work directory, which goes when the install ends.
  const { run, calls } = recorder({ cp: ([from]) => void files.push(readFileSync(from, 'utf8')), ldconfig: listing(join(PREFIX, 'lib')) });
  try {
    const out = await inStep(() => installAt(PREFIX, { runner, assets }, { download: fast, run, writable: async () => false }));
    assert.deepEqual(out.value, {});
    assert.match(out.stdout, /Writing \/usr\/local\/lib and \/usr\/local\/include as root, with sudo/);
    const from = calls[1][2].slice(0, -'/SVBCameraSDK.h'.length);
    assert.deepEqual(calls, [...copiesInto(PREFIX, from), ['sudo', 'ldconfig'], ['ldconfig', '-p']]);
    assert.equal(existsSync(from), false);
    assert.deepEqual(files, ['the header', 'amd64 camera']);
  } finally {
    await server.close();
  }
});

test('installAt names LD_LIBRARY_PATH where the loader\'s cache has no /usr/local/lib, as Fedora\'s has none', async () => {
  const { server, runner, assets } = await indiServer('Linux-X64');
  try {
    for (const ldconfig of [listing(join('/', 'opt', 'svbony', 'lib')), () => '0 libs found in cache\n']) {
      const { run } = recorder({ ldconfig });
      const out = await inStep(() => installAt(PREFIX, { runner, assets }, { download: fast, run, writable: async () => false }));
      assert.deepEqual(out.value, { LD_LIBRARY_PATH: join(PREFIX, 'lib') });
    }
  } finally {
    await server.close();
  }
});

test('installAt on macOS needs no loader variable, the library carrying its own path', async () => {
  const { server, runner, assets } = await indiServer('macOS-X64');
  const { run, calls } = recorder();
  try {
    const out = await inStep(() => installAt(PREFIX, { runner, assets }, { download: fast, run, writable: async () => false }));
    assert.deepEqual(out.value, {});
    assert.ok(calls.some((call) => call.join(' ').startsWith('sudo install_name_tool -id')));
    assert.ok(!calls.some(([program, ...args]) => program === 'ldconfig' || args[0] === 'ldconfig'));
  } finally {
    await server.close();
  }
});

test('installAt on Windows offers no SDK, which only the private mirror holds, and says where to get one', async () => {
  const { server, runner, assets } = await mirror();
  try {
    await assert.rejects(inStep(() => installAt(tempDir(), { runner, assets }, { download: fast })), (err) => {
      assert.match(err.message, /^the SVBony SDK for Windows comes from the project's private mirror, which developers are not given/);
      assert.match(err.message, /from svbony\.com, unpack it, and point SVBONY_SDK_LIB_DIR at its SVBONY\\lib\\x64 folder/);
      return true;
    });
    assert.equal(server.requests.length, 0);
  } finally {
    await server.close();
  }
});
