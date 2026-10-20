import assert from 'node:assert/strict';
import { existsSync, lstatSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, readlinkSync, writeFileSync } from 'node:fs';
import { EOL, tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';

import { INSTALLED } from '../lib/install.mjs';
import { capture, serve, withEnv, zip } from '../lib/testing.mjs';
import {
  CAMERA_SDK_URL,
  EAF_SDK_URL,
  EFW_SDK_URL,
  findRuntime,
  HEADERS,
  INDI,
  install,
  installAt,
  powershellProgram,
  PREFIX,
  REF,
  resolve,
  RUNNERS,
  unpackZip,
} from './zwo-sdk.mjs';

// --- Fixtures -----------------------------------------------------------------

function tempDir() {
  return mkdtempSync(join(process.env.TEST_TMPDIR ?? tmpdir(), 'zwo-sdk-'));
}

const fast = { attempts: 1, delayMs: 1 };
const POSIX = { skip: process.platform === 'win32' && 'the path runs on Linux and macOS, with their install and cp' };

// The files INDI's libasi/ folder serves at REF, by their path under it.
function indiFiles(arch) {
  return {
    ...Object.fromEntries(HEADERS.map((name) => [name, `the ${name}`])),
    [`${arch}/libASICamera2.bin`]: `${arch} camera`,
    [`${arch}/libEFWFilter.bin`]: `${arch} filter wheel`,
    [`${arch}/libEAFFocuser.bin`]: `${arch} focuser`,
  };
}

// A server holding `files` (a path under libasi/ to its text) where the INDI
// repository's raw links put them, and resolve's assets for `runner` there.
async function indiServer(runner, files = indiFiles(RUNNERS[runner].arch)) {
  const base = `/${INDI}/raw/${REF}/libasi/`;
  const server = await serve((req, res) => {
    const body = req.url.startsWith(base) ? files[req.url.slice(base.length)] : undefined;
    res.statusCode = body === undefined ? 404 : 200;
    res.end(body);
  });
  return { server, ...resolve({ runner, server: server.url }) };
}

// The x64 import library and DLL of an SDK, and the builds a search must pass
// over: 32-bit ones, a demo's, a Windows 7 one, a static library. With
// `unicode`, the EFW SDK also holds the guide ZWO names in Chinese, which
// Windows' code page cannot hold.
function windowsSdks({ eafX64 = true, unicode = false } = {}) {
  const inner = {
    asi: zip([
      { path: 'ASI SDK/demo/bin/x64/ASICamera2.dll', data: 'a demo camera dll' },
      { path: 'ASI SDK/include/ASICamera2.h', data: 'header' },
      { path: 'ASI SDK/lib/ASICamera2.dll', data: 'a camera dll of no named architecture' },
      { path: 'ASI SDK/lib/x64/ASICamera2.dll', data: 'camera dll' },
      { path: 'ASI SDK/lib/x64/ASICamera2.lib', data: 'camera lib' },
      { path: 'ASI SDK/lib/x86/ASICamera2.dll', data: 'a 32-bit camera dll' },
      { path: 'ASI SDK/lib/x86/ASICamera2.lib', data: 'a 32-bit camera lib' },
    ]),
    efw: zip([
      { path: 'EFW_Windows_SDK_V1.8.4/lib/Win32/Release/EFW_filter.dll', data: 'a 32-bit filter wheel dll' },
      { path: 'EFW_Windows_SDK_V1.8.4/lib/x64/Release/EFW_filter-static.lib', data: 'a static library' },
      { path: 'EFW_Windows_SDK_V1.8.4/lib/x64/Release/EFW_filter.dll', data: 'filter wheel dll' },
      { path: 'EFW_Windows_SDK_V1.8.4/lib/x64/Release/EFW_filter.lib', data: 'filter wheel lib' },
      ...(unicode ? [{ path: 'EFW_Windows_SDK_V1.8.4/doc/EFW SDK快速入门.pdf', data: 'a guide' }] : []),
    ]),
    eaf: zip([
      { path: 'EAF_Windows_SDK_V1.8.1/lib/Windows/Win32/Release/EAF_focuser.dll', data: 'a 32-bit focuser dll' },
      { path: 'EAF_Windows_SDK_V1.8.1/lib/Windows/Win32/Release/EAF_focuser.lib', data: 'a 32-bit focuser lib' },
      { path: 'EAF_Windows_SDK_V1.8.1/lib/x86/EAF_focuser.lib', data: 'a 32-bit focuser lib' },
      ...(eafX64
        ? [
            { path: 'EAF_Windows_SDK_V1.8.1/lib/Windows/x64/Release/EAF_focuser.dll', data: 'focuser dll' },
            { path: 'EAF_Windows_SDK_V1.8.1/lib/Windows/x64/Release/EAF_focuser.lib', data: 'focuser lib' },
            { path: 'EAF_Windows_SDK_V1.8.1/lib/Windows7/x64/Release/EAF_focuser.dll', data: 'a Windows 7 focuser dll' },
            { path: 'EAF_Windows_SDK_V1.8.1/lib/Windows7/x64/Release/EAF_focuser.lib', data: 'a Windows 7 focuser lib' },
          ]
        : []),
    ]),
  };
  // Each download holds its SDK for Windows beside others, as ZWO's do; the
  // Android one holds a DLL that must not be taken, since it is not unpacked.
  return {
    'asi.zip': zip([
      { path: 'ASI_Camera_SDK/ASISDK_ANDROID.zip', data: zip([{ path: 'x64/ASICamera2.dll', data: 'an Android decoy' }]) },
      { path: 'ASI_Camera_SDK/ASI_Windows_SDK_V1.41.zip', data: inner.asi },
      { path: 'ASI_Camera_SDK/Readme.txt', data: 'read me' },
    ]),
    'efw.zip': zip([
      { path: 'EFW_SDK/EFW_Linux_macOS_SDK_V1.8.4.tar.bz2', data: 'not for Windows' },
      { path: 'EFW_SDK/EFW_Windows_SDK_V1.8.4.zip', data: inner.efw },
    ]),
    'eaf.zip': zip([{ path: 'EAF_SDK_V1.8.1/EAF_Windows_SDK_V1.8.1.zip', data: inner.eaf }]),
  };
}

// What the Windows install stages, by file name.
const STAGED = {
  'ASICamera2.dll': 'camera dll',
  'ASICamera2.lib': 'camera lib',
  'EAFFocuser.dll': 'focuser dll',
  'EAFFocuser.lib': 'focuser lib',
  'EFWFilter.dll': 'filter wheel dll',
  'EFWFilter.lib': 'filter wheel lib',
};

// A CDN serving `zips` (a download's name to its bytes), or failing every
// request with `status`, and resolve's Windows assets pointed at it.
async function cdn(zips = windowsSdks(), { status } = {}) {
  const byApp = { DeveloperCameraSdk: 'asi.zip', DeveloperEfwSdk: 'efw.zip', DeveloperEafSdk: 'eaf.zip' };
  const server = await serve((req, res) => {
    const body = zips[byApp[new URL(req.url, 'http://cdn').searchParams.get('app')]];
    res.statusCode = status ?? (body === undefined ? 404 : 200);
    res.end(status ? undefined : body);
  });
  const resolved = resolve({ runner: 'Windows-X64' });
  const assets = resolved.assets.map((asset) => ({ ...asset, url: `${server.url}/software${new URL(asset.url).search}` }));
  return { server, ...resolved, assets };
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

// The commands that copy what resolve chose for `suffix` from `from` into
// PREFIX, as root.
function copiesInto(prefix, from, suffix) {
  const lib = join(prefix, 'lib');
  const include = join(prefix, 'include');
  return [
    ['sudo', 'install', '-d', lib, include],
    ...HEADERS.map((name) => ['sudo', 'cp', join(from, name), join(include, name)]),
    ...['ASICamera2', 'EFWFilter', 'EAFFocuser'].map((name) => ['sudo', 'cp', join(from, `lib${name}${suffix}`), join(lib, `lib${name}${suffix}`)]),
  ];
}

// Runs `fn` with command files to write to, and the environment it changes
// put back after, `env` set first; resolves to what it wrote to each.
async function inStep(fn, env = {}) {
  const dir = tempDir();
  const files = { GITHUB_PATH: join(dir, 'path'), GITHUB_ENV: join(dir, 'env') };
  for (const file of Object.values(files)) writeFileSync(file, '');
  const changed = { PATH: process.env.PATH, LD_LIBRARY_PATH: process.env.LD_LIBRARY_PATH, LIBCLANG_PATH: undefined, ZWO_SDK_LIB_DIR: undefined };
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

test('resolve takes the headers and each runner\'s libraries from INDI\'s libasi folder at REF', () => {
  const base = `https://github.com/${INDI}/raw/${REF}/libasi`;
  const linux = resolve({ runner: 'Linux-X64' });
  assert.deepEqual(
    linux.assets.map(({ name, url, pin }) => [name, url, pin]),
    [
      ['ASICamera2.h', `${base}/ASICamera2.h`, { ref: REF }],
      ['EFW_filter.h', `${base}/EFW_filter.h`, { ref: REF }],
      ['EAF_focuser.h', `${base}/EAF_focuser.h`, { ref: REF }],
      ['license.txt', `${base}/license.txt`, { ref: REF }],
      ['libASICamera2.so', `${base}/x64/libASICamera2.bin`, { ref: REF }],
      ['libEFWFilter.so', `${base}/x64/libEFWFilter.bin`, { ref: REF }],
      ['libEAFFocuser.so', `${base}/x64/libEAFFocuser.bin`, { ref: REF }],
    ],
  );
  assert.equal(resolve({ runner: 'Linux-ARM64' }).assets[4].url, `${base}/armv8/libASICamera2.bin`);
  const mac = resolve({ runner: 'macOS-ARM64' }).assets.slice(4);
  assert.deepEqual(
    mac.map(({ name, url }) => [name, url]),
    [
      ['libASICamera2.dylib', `${base}/mac_arm64/libASICamera2.bin`],
      ['libEFWFilter.dylib', `${base}/mac_arm64/libEFWFilter.bin`],
      ['libEAFFocuser.dylib', `${base}/mac_arm64/libEAFFocuser.bin`],
    ],
  );
  assert.equal(resolve({ runner: 'macOS-X64' }).assets[6].url, `${base}/mac_x64/libEAFFocuser.bin`);
});

test('resolve keys the cache on the runner\'s folder, the files and their one commit, with nothing to restore instead', () => {
  const { key, restoreKey } = resolve({ runner: 'Linux-X64' });
  assert.equal(
    key,
    'install-zwo-sdk-x64-ASICamera2.h-EFW_filter.h-EAF_focuser.h-license.txt-' +
      `libASICamera2.so-libEFWFilter.so-libEAFFocuser.so-${REF}`,
  );
  assert.equal(restoreKey, '');
  assert.notEqual(resolve({ runner: 'Linux-ARM64' }).key, key);
});

test('resolve takes ZWO\'s three SDKs on Windows, rolling, keyed on the week, with the earlier weeks\' to restore', () => {
  const now = new Date('2026-10-19T12:00:00Z');
  const { assets, key, restoreKey } = resolve({ runner: 'Windows-X64', now });
  assert.deepEqual(
    assets.map(({ name, url, pin, library, as }) => [name, url, pin, library, as]),
    [
      ['asi.zip', CAMERA_SDK_URL, { rolling: true }, 'ASICamera2', 'ASICamera2'],
      ['efw.zip', EFW_SDK_URL, { rolling: true }, 'EFW_filter', 'EFWFilter'],
      ['eaf.zip', EAF_SDK_URL, { rolling: true }, 'EAF_focuser', 'EAFFocuser'],
    ],
  );
  assert.equal(key, 'install-zwo-sdk-windows-asi.zip-efw.zip-eaf.zip-week-2026-43');
  assert.equal(restoreKey, 'install-zwo-sdk-windows-asi.zip-efw.zip-eaf.zip-week-');
  assert.ok(resolve({ runner: 'Windows-X64', now: new Date('2026-01-01T00:00:00Z') }).key.startsWith(restoreKey));
});

test('resolve refuses a runner the SDK has no build for', () => {
  assert.throws(() => resolve({ runner: 'Windows-ARM64' }), {
    message: 'The ZWO SDK has no asset for Windows-ARM64; it has one for Linux-X64, Linux-ARM64, macOS-ARM64, macOS-X64, Windows-X64',
  });
});

// --- Installing on Linux and macOS ----------------------------------------------------

test('install downloads the files into zwo-sdk/, copies them into /usr/local as root, and refreshes the loader\'s cache', async () => {
  const { server, runner, assets } = await indiServer('Linux-X64');
  const workspace = tempDir();
  const { run, calls } = recorder();
  try {
    const out = await inStep(() => install({ runner, assets, workspace, sudo: true, cached: false, download: fast, run }));
    assert.equal(out.value, true);
    const base = `/${INDI}/raw/${REF}/libasi`;
    assert.deepEqual(server.requests.map(({ url }) => url), [
      ...HEADERS.map((name) => `${base}/${name}`),
      `${base}/x64/libASICamera2.bin`,
      `${base}/x64/libEFWFilter.bin`,
      `${base}/x64/libEAFFocuser.bin`,
    ]);
    const downloads = join(workspace, 'zwo-sdk');
    assert.deepEqual(readdirSync(workspace), ['zwo-sdk']);
    assert.equal(readFileSync(join(downloads, 'libEFWFilter.so'), 'utf8'), 'x64 filter wheel');
    assert.equal(readFileSync(join(downloads, 'license.txt'), 'utf8'), 'the license.txt');
    assert.deepEqual(calls, [...copiesInto(PREFIX, downloads, '.so'), ['sudo', 'ldconfig']]);
    assert.equal(out.env, '');
    assert.equal(out.path, '');
  } finally {
    await server.close();
  }
});

test('install takes the files the cache restored, fetching nothing, and installs them', async () => {
  const { server, runner, assets } = await indiServer('Linux-ARM64');
  const workspace = tempDir();
  writeAll(join(workspace, 'zwo-sdk'), Object.fromEntries(assets.map(({ name }) => [name, 'cached'])));
  const { run, calls } = recorder();
  try {
    const out = await inStep(() => install({ runner, assets, workspace, sudo: true, cached: true, run }));
    assert.equal(out.value, false);
    assert.equal(server.requests.length, 0);
    assert.match(out.stdout, /restored from the cache/);
    assert.deepEqual(calls, [...copiesInto(PREFIX, join(workspace, 'zwo-sdk'), '.so'), ['sudo', 'ldconfig']]);
  } finally {
    await server.close();
  }
});

test('install downloads again when the cache restored only some of the files', async () => {
  const { server, runner, assets } = await indiServer('Linux-X64');
  const workspace = tempDir();
  writeAll(join(workspace, 'zwo-sdk'), { 'ASICamera2.h': 'cached' });
  try {
    const out = await inStep(() => install({ runner, assets, workspace, sudo: true, cached: true, download: fast, run: recorder().run }));
    assert.equal(out.value, true);
    assert.equal(server.requests.length, 7);
    assert.match(out.stdout, /::warning::The cache restored .* without EFW_filter\.h and EAF_focuser\.h and license\.txt and libASICamera2\.so/);
    assert.equal(readFileSync(join(workspace, 'zwo-sdk', 'ASICamera2.h'), 'utf8'), 'the ASICamera2.h');
  } finally {
    await server.close();
  }
});

test('install installs nothing when a file is missing upstream', async () => {
  const files = indiFiles('x64');
  delete files['x64/libEAFFocuser.bin'];
  const { server, runner, assets } = await indiServer('Linux-X64', files);
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

test('install on macOS names each library by its path, and points those that load libusb by @rpath at a copy beside them', async () => {
  const { server, runner, assets } = await indiServer('macOS-ARM64');
  const workspace = tempDir();
  const lib = join(PREFIX, 'lib');
  const brew = join('/', 'opt', 'homebrew', 'opt', 'libusb');
  // The camera and filter wheel load libusb by @rpath; the focuser does not.
  const otool = ([, target]) =>
    `${target}:\n\t${target} (compatibility version 0.0.0)\n` +
    (target.endsWith('libEAFFocuser.dylib') ? '' : '\t@rpath/libusb-1.0.0.dylib (compatibility version 4.0.0)\n') +
    '\t/usr/lib/libc++.1.dylib (compatibility version 1.0.0)\n';
  const { run, calls } = recorder({ brew: `${brew}\n`, otool });
  try {
    const out = await inStep(() => install({ runner, assets, workspace, sudo: false, cached: false, download: fast, run }));
    assert.equal(out.value, true);
    const libusb = join(lib, 'libusb-1.0.0.dylib');
    const named = (name) => [
      ['sudo', 'install_name_tool', '-id', join(lib, name), join(lib, name)],
      ['otool', '-L', join(lib, name)],
    ];
    const pointed = (name) => [['sudo', 'install_name_tool', '-change', '@rpath/libusb-1.0.0.dylib', libusb, join(lib, name)]];
    assert.deepEqual(calls, [
      ...copiesInto(PREFIX, join(workspace, 'zwo-sdk'), '.dylib'),
      ['brew', '--prefix', 'libusb'],
      ['sudo', 'cp', join(brew, 'lib', 'libusb-1.0.0.dylib'), libusb],
      ...named('libASICamera2.dylib'),
      ...pointed('libASICamera2.dylib'),
      ...named('libEFWFilter.dylib'),
      ...pointed('libEFWFilter.dylib'),
      ...named('libEAFFocuser.dylib'),
    ]);
  } finally {
    await server.close();
  }
});

// --- Installing without sudo ------------------------------------------------------------

test('install without sudo stages the SDK under RUNNER_TEMP, links the runtime libraries, and points the build there', POSIX, async () => {
  const { server, runner, assets } = await indiServer('Linux-ARM64');
  const workspace = tempDir();
  const temp = tempDir();
  const system = tempDir();
  writeAll(system, { 'libusb-1.0.so.0': 'libusb', 'libudev.so.1': 'libudev' });
  const find = async (soname) => join(system, soname);
  try {
    const out = await inStep(
      () => install({ runner, assets, workspace, temp, sudo: false, cached: false, download: fast, find }),
      { LD_LIBRARY_PATH: '/opt/other' },
    );
    assert.equal(out.value, false);
    assert.deepEqual(readdirSync(workspace), []);
    const lib = join(temp, 'zwo-sdk', 'lib');
    assert.deepEqual(readdirSync(temp), ['zwo-sdk']);
    assert.deepEqual(readAll(join(temp, 'zwo-sdk', 'include')), Object.fromEntries(HEADERS.map((name) => [name, `the ${name}`])));
    assert.deepEqual(readdirSync(lib).sort(), ['libASICamera2.so', 'libEAFFocuser.so', 'libEFWFilter.so', 'libudev.so', 'libusb-1.0.so']);
    assert.equal(readFileSync(join(lib, 'libASICamera2.so'), 'utf8'), 'armv8 camera');
    assert.ok(lstatSync(join(lib, 'libusb-1.0.so')).isSymbolicLink());
    assert.equal(readlinkSync(join(lib, 'libusb-1.0.so')), join(system, 'libusb-1.0.so.0'));
    assert.equal(readlinkSync(join(lib, 'libudev.so')), join(system, 'libudev.so.1'));
    assert.equal(envValue(out.env, 'ZWO_SDK_LIB_DIR'), lib);
    assert.equal(envValue(out.env, 'LD_LIBRARY_PATH'), `${lib}:/opt/other`);
  } finally {
    await server.close();
  }
});

test('install without sudo names the package to install when a runtime library is missing', POSIX, async () => {
  const { server, runner, assets } = await indiServer('Linux-ARM64');
  const system = tempDir();
  writeAll(system, { 'libusb-1.0.so.0': 'libusb' });
  const find = async (soname) => (soname === 'libudev.so.1' ? undefined : join(system, soname));
  try {
    await assert.rejects(
      inStep(() => install({ runner, assets, workspace: tempDir(), temp: tempDir(), sudo: false, download: fast, find })),
      /the system library libudev\.so\.1 is not on this machine\. .*sudo apt-get install -y libudev1$/,
    );
  } finally {
    await server.close();
  }
});

test('findRuntime reads the loader\'s cache, and the usual directories when that fails', async () => {
  const listing =
    '1234 libs found in cache `/etc/ld.so.cache\'\n' +
    '\tlibusb-1.0.so.0 (libc6,AArch64) => /lib/aarch64-linux-gnu/libusb-1.0.so.0\n' +
    '\tlibudev.so.1 (libc6,AArch64) => /lib/aarch64-linux-gnu/libudev.so.1\n';
  const cache = recorder({ ldconfig: listing });
  assert.equal(await findRuntime('libudev.so.1', { run: cache.run, dirs: [] }), '/lib/aarch64-linux-gnu/libudev.so.1');
  assert.deepEqual(cache.calls, [['ldconfig', '-p']]);

  const dirs = [tempDir(), tempDir()];
  writeFileSync(join(dirs[1], 'libudev.so.1'), 'libudev');
  const none = async () => {
    throw new Error('spawn ldconfig ENOENT');
  };
  assert.equal(await findRuntime('libudev.so.1', { run: none, dirs }), join(dirs[1], 'libudev.so.1'));
  assert.equal(await findRuntime('libudev.so.1', { run: recorder({ ldconfig: listing.slice(0, 40) }).run, dirs }), join(dirs[1], 'libudev.so.1'));
  assert.equal(await findRuntime('libusb-1.0.so.0', { run: none, dirs }), undefined);
});

// --- Installing on Windows ----------------------------------------------------------------

test('install on Windows stages each SDK\'s x64 import library and DLL under the names libzwo-sys links', async () => {
  const { server, runner, assets } = await cdn();
  const workspace = tempDir();
  const temp = tempDir();
  const llvm = tempDir();
  const { run, calls } = recorder();
  try {
    const out = await inStep(() => install({ runner, assets, workspace, temp, cached: false, download: fast, run, llvm }));
    assert.equal(out.value, true);
    assert.equal(server.requests.length, 3);
    assert.deepEqual(readdirSync(join(workspace, 'zwo-sdk')).sort(), ['asi.zip', 'eaf.zip', 'efw.zip']);
    const lib = join(temp, 'zwo-sdk', 'lib');
    assert.deepEqual(readAll(lib), STAGED);
    assert.deepEqual(readdirSync(join(temp, 'zwo-sdk')), ['lib']);
    assert.deepEqual(calls, [['choco', 'install', 'llvm', '--no-progress', '-y']]);
    assert.equal(envValue(out.env, 'LIBCLANG_PATH'), llvm);
    assert.equal(envValue(out.env, 'ZWO_SDK_LIB_DIR'), lib);
    assert.equal(out.path, `${lib}${EOL}`);
  } finally {
    await server.close();
  }
});

test('install on Windows unpacks an SDK holding a name Windows\' code page cannot hold', {
  skip: process.platform !== 'win32' && 'the Windows runner unpacks these with its own PowerShell',
}, async () => {
  const { server, runner, assets } = await cdn(windowsSdks({ unicode: true }));
  try {
    const out = await inStep(() =>
      install({ runner, assets, workspace: tempDir(), temp: tempDir(), cached: false, download: fast, run: recorder().run, llvm: tempDir() }),
    );
    assert.deepEqual(readAll(envValue(out.env, 'ZWO_SDK_LIB_DIR')), STAGED);
  } finally {
    await server.close();
  }
});

test('unpackZip on Windows runs Expand-Archive by its full path, with each path quoted whole', async () => {
  const { run, calls } = recorder();
  await withEnv({ SystemRoot: 'C:\\Windows' }, () =>
    unpackZip("C:\\Users\\o'brien\\a\u2019b.zip", 'C:\\Temp\\out', { platform: 'win32', run }),
  );
  assert.equal(powershellProgram(), 'C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe');
  assert.equal(calls.length, 1);
  const [program, ...args] = calls[0];
  assert.equal(program, powershellProgram());
  assert.deepEqual(args.slice(0, 3), ['-NoProfile', '-NonInteractive', '-EncodedCommand']);
  const script = Buffer.from(args[3], 'base64').toString('utf16le');
  assert.match(script, /^\$ErrorActionPreference = 'Stop';/);
  assert.ok(script.includes("Expand-Archive -LiteralPath 'C:\\Users\\o''brien\\a\u2019\u2019b.zip' -DestinationPath 'C:\\Temp\\out' -Force"), script);
  assert.match(script, /catch \{ \[Console\]::Error\.WriteLine\(\$_\.Exception\.Message\); exit 1 \}$/);
});

test('install on Windows takes the LLVM the runner has, and this week\'s cached downloads', async () => {
  const { server, runner, assets } = await cdn();
  const workspace = tempDir();
  writeAll(join(workspace, 'zwo-sdk'), windowsSdks());
  const llvm = tempDir();
  writeFileSync(join(llvm, 'libclang.dll'), 'libclang');
  const { run, calls } = recorder();
  try {
    const out = await inStep(() => install({ runner, assets, workspace, temp: tempDir(), cached: true, run, llvm }));
    assert.equal(out.value, false);
    assert.equal(server.requests.length, 0);
    assert.deepEqual(calls, []);
    assert.deepEqual(readAll(envValue(out.env, 'ZWO_SDK_LIB_DIR')), STAGED);
  } finally {
    await server.close();
  }
});

test('install on Windows falls back on an earlier week\'s downloads when ZWO\'s CDN fails', async () => {
  const { server, runner, assets } = await cdn(undefined, { status: 503 });
  const workspace = tempDir();
  writeAll(join(workspace, 'zwo-sdk'), windowsSdks());
  const restored = 'install-zwo-sdk-windows-asi.zip-efw.zip-eaf.zip-week-2026-41';
  try {
    const out = await inStep(() =>
      install({ runner, assets, workspace, temp: tempDir(), cached: false, restored, download: fast, run: recorder().run, llvm: tempDir() }),
    );
    assert.equal(out.value, false);
    assert.equal(server.requests.length, 1);
    assert.match(out.stdout, new RegExp(`::warning::.*503.*Installing the earlier download the cache restored instead, ${restored}`));
    assert.deepEqual(readAll(envValue(out.env, 'ZWO_SDK_LIB_DIR')), STAGED);
  } finally {
    await server.close();
  }
});

test('install on Windows fails when ZWO\'s CDN fails and the cache restored nothing', async () => {
  const { server, runner, assets } = await cdn(undefined, { status: 503 });
  const workspace = tempDir();
  try {
    await assert.rejects(
      inStep(() => install({ runner, assets, workspace, temp: tempDir(), cached: false, download: fast, run: recorder().run, llvm: tempDir() })),
      /503/,
    );
    assert.deepEqual(readdirSync(workspace), []);
  } finally {
    await server.close();
  }
});

test('install on Windows refuses an SDK without its x64 library, and stages nothing', async () => {
  const { server, runner, assets } = await cdn(windowsSdks({ eafX64: false }));
  const temp = tempDir();
  try {
    await assert.rejects(
      inStep(() => install({ runner, assets, workspace: tempDir(), temp, cached: false, download: fast, run: recorder().run, llvm: tempDir() })),
      { message: 'eaf.zip holds no x64 EAF_focuser.lib' },
    );
    assert.deepEqual(readdirSync(join(temp, 'zwo-sdk')), []);
  } finally {
    await server.close();
  }
});

// --- A developer's install ----------------------------------------------------------------

test('installAt copies into a prefix the developer can write without sudo, and names its lib folder', POSIX, async () => {
  const { server, runner, assets } = await indiServer('Linux-X64');
  const prefix = tempDir();
  writeAll(join(prefix, 'lib'), { 'libother.so': 'mine', 'libASICamera2.so': 'older' });
  try {
    const out = await inStep(() => installAt(prefix, { runner, assets }, { download: fast }));
    assert.deepEqual(out.value, { ZWO_SDK_LIB_DIR: join(prefix, 'lib') });
    assert.deepEqual(readAll(join(prefix, 'lib')), {
      'libASICamera2.so': 'x64 camera',
      'libEAFFocuser.so': 'x64 focuser',
      'libEFWFilter.so': 'x64 filter wheel',
      'libother.so': 'mine',
    });
    assert.deepEqual(readdirSync(join(prefix, 'include')).sort(), [...HEADERS].sort());
    assert.doesNotMatch(out.stdout, /sudo/);
    assert.equal(out.env, '');
  } finally {
    await server.close();
  }
});

test('installAt writes /usr/local with sudo where the developer cannot, and needs no variable there', async () => {
  const { server, runner, assets } = await indiServer('Linux-X64');
  const files = [];
  // The copies come from the work directory, which goes when the install ends.
  const { run, calls } = recorder({ cp: ([from]) => void files.push(readFileSync(from, 'utf8')) });
  try {
    const out = await inStep(() => installAt(PREFIX, { runner, assets }, { download: fast, run, writable: async () => false }));
    assert.deepEqual(out.value, {});
    assert.match(out.stdout, /Writing \/usr\/local\/lib and \/usr\/local\/include as root, with sudo/);
    const from = calls[1][2].slice(0, -'/ASICamera2.h'.length);
    assert.deepEqual(calls, [...copiesInto(PREFIX, from, '.so'), ['sudo', 'ldconfig']]);
    assert.equal(existsSync(from), false);
    assert.deepEqual(files, [...HEADERS.map((name) => `the ${name}`), 'x64 camera', 'x64 filter wheel', 'x64 focuser']);
  } finally {
    await server.close();
  }
});

test('installAt on Windows installs a directory whole, marked, and replaces only an earlier install', async () => {
  const { server, runner, assets } = await cdn();
  const parent = tempDir();
  const dir = join(parent, 'zwo-sdk');
  const now = new Date('2026-10-19T12:00:00Z');
  try {
    const first = await inStep(() => installAt(dir, { runner, assets }, { download: fast, now }));
    assert.deepEqual(first.value, { ZWO_SDK_LIB_DIR: dir });
    assert.deepEqual(readAll(dir), {
      ...STAGED,
      [INSTALLED]: "ZWO's SDKs for Windows, asi.zip, efw.zip, eaf.zip, as its CDN served them on 2026-10-19\n",
    });
    assert.deepEqual(readdirSync(parent), ['zwo-sdk']);
    await inStep(() => installAt(dir, { runner, assets }, { download: fast, now }));
    assert.equal(readdirSync(dir).length, 7);

    const other = tempDir();
    writeFileSync(join(other, 'notes.txt'), 'mine');
    await assert.rejects(inStep(() => installAt(other, { runner, assets }, { download: fast })), /not ours to replace/);
    assert.deepEqual(readdirSync(other), ['notes.txt']);
  } finally {
    await server.close();
  }
});

test('installAt on Windows keeps an earlier install when a download fails', async () => {
  const good = await cdn();
  const dir = join(tempDir(), 'zwo-sdk');
  try {
    await inStep(() => installAt(dir, { runner: good.runner, assets: good.assets }, { download: fast }));
  } finally {
    await good.server.close();
  }
  const { server, runner, assets } = await cdn(undefined, { status: 503 });
  try {
    await assert.rejects(inStep(() => installAt(dir, { runner, assets }, { download: fast })), /503/);
    assert.equal(readAll(dir)['ASICamera2.dll'], 'camera dll');
  } finally {
    await server.close();
  }
});
