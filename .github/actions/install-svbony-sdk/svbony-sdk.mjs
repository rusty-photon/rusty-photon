// The SVBony camera SDK, libSVBCameraSDK, which libsvbony-sys links: which
// files each runner takes, and their install where libsvbony-sys's build
// script and the loader find them. libsvbony-sys links the SDK even into a
// `simulation` build, which removes the camera, not the link, so every Cargo
// job that builds svbony-camera installs it, or sets SVBONY_SKIP_NATIVE_LINK
// to leave the link out (docs/services/svbony-camera.md "Native dependency &
// build gating"). Bazel's builds always leave it out.
//
// The SDK carries no license grant, so nothing we ship bundles it (ADR-018).
// Its FFI is written by hand, with no bindgen, so it needs no libclang, and
// it needs libusb-1.0 and no libudev: readelf -d of the vendored blobs shows
// undefined libusb_* symbols and no NEEDED entry for libusb at all, which the
// consuming binary's own -lusb-1.0 resolves at run time, besides libc,
// libstdc++, libgcc_s and libm.
//
// Linux and macOS take SVBony's prebuilt library as the INDI repository
// vendors it, libsvbony/libSVBCameraSDK_<arch>.bin at the commit REF: it has
// no hash of its own, and a commit's content cannot move. Bump REF to adopt a
// newer SDK. The tools crate's Linux builder, `cargo ci build-packages`,
// links the packages against the blob at REF, which it reads from its line
// below, and its packaging guard (tools/rusty-photon-ci/src/pkg_assets.rs)
// checks that the operator's rusty-photon-svbony-sdk-install takes the same
// commit.
//
// The blob has no DT_SONAME, despite the SOVERSION 1 INDI's CMakeLists.txt
// gives its install, so a binary linked against libSVBCameraSDK.so asks the
// loader for that file name. glibc's ldconfig keys its cache by the file name
// when there is no SONAME, so the library installed as libSVBCameraSDK.so.1
// with a libSVBCameraSDK.so link to it, in a directory ldconfig scans, is
// found by both names, at link time and at run time.
//
// INDI vendors no build for Apple Silicon, only mac64 (Intel), so a macOS-ARM64
// runner has none. A job there skips the action and builds with
// SVBONY_SKIP_NATIVE_LINK=1, as native.yml's and conformu.yml's do.
//
// Windows takes SVBony's own SDK, whose download from svbony.com sits behind
// a CAPTCHA, and INDI's packaging has no Windows build. CI fetches the
// operator's copy of the vendor zip from the project's private mirror,
// token-gated on every request and never republished (ADR-018 §7,
// tools/private-mirror-worker/), and checks it against the SHA-256 of the
// vendor's original: the mirror is trusted for availability, not integrity.
// So the zip goes into no cache, which a fork's pull request could restore,
// and the developer's install offers no Windows SDK.

import { copyFile, mkdir, rename, rm } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';

import { addPath, command, exportVariable, io, setSecret } from '../lib/helpers.mjs';
import {
  cacheKey,
  canWrite,
  ChecksumError,
  download,
  downloadAll,
  findRuntime,
  forRunner,
  linkRuntime,
  loaderFinds,
  unpackZips,
  withWorkDir,
} from '../lib/install.mjs';

/** The repository that vendors the library for Linux and macOS, and its commit it comes from. */
export const INDI = 'indilib/indi-3rdparty';
export const REF = 'cd50a3b95032d850cca28d8162513276bc1349ba';

/**
 * SVBony's SDK for Windows on the project's private mirror, as
 * `svbony/windows-SVBCameraSDK-v<version>.zip`, and the SHA-256 of the
 * vendor's original. Bump both together once the new zip is uploaded
 * (tools/private-mirror-worker/README.md).
 */
export const MIRROR = 'https://private.rustyphoton.space';
export const WINDOWS_SDK_VERSION = '1.13.4';
export const WINDOWS_SDK_SHA256 = 'c0ad5b02c475a39a93d3dbcf8d64a17c079d0d4bcab7d66184a598a15f4d4a5e';

/**
 * Each runner's SDK: on Linux and macOS, the name INDI gives its build and
 * the suffix the library takes; on Windows, SVBony's own SDK.
 */
export const RUNNERS = {
  'Linux-X64': { arch: 'amd64', suffix: '.so' },
  'Linux-ARM64': { arch: 'armv8', suffix: '.so' },
  'macOS-X64': { arch: 'mac64', suffix: '.dylib' },
  'Windows-X64': { windows: true },
};

/** Runners with no build of the SDK, and what a job on one does instead. */
export const NO_BUILD = {
  'macOS-ARM64':
    'The SVBony SDK has no build for Apple Silicon (macOS-ARM64): the INDI repository vendors mac64, for Intel, only. ' +
    'Use an Intel runner for the real link, or skip install-svbony-sdk and build with SVBONY_SKIP_NATIVE_LINK=1',
};

/**
 * The header, for completeness: libsvbony-sys's FFI is written by hand, and
 * nothing in the repository reads it.
 */
export const HEADER = 'SVBCameraSDK.h';

/** The library by the name libsvbony-sys links (`-lSVBCameraSDK`). */
export const LIBRARY = 'libSVBCameraSDK';

/** Where the SDK goes on Linux and macOS: where libsvbony-sys's build script looks. */
export const PREFIX = '/usr/local';

/**
 * Where the SDK for Windows keeps its import library and DLL, which the
 * vendor zip's SHA-256 fixes.
 */
const WINDOWS_LIB = ['SVBONY', 'lib', 'x64'];
const WINDOWS_FILES = ['SVBCameraSDK.lib', 'SVBCameraSDK.dll'];

// --- Choosing the files -------------------------------------------------------------

/**
 * What `runner` installs: its assets ({ name, url, pin }) and the cache key.
 * Throws for a runner with no build, saying what to do instead.
 */
export function resolve({ runner, server = 'https://github.com' }) {
  if (Object.hasOwn(NO_BUILD, runner)) throw new Error(NO_BUILD[runner]);
  const { arch, suffix, windows } = forRunner(RUNNERS, runner, 'The SVBony SDK');
  if (windows) {
    const name = `windows-SVBCameraSDK-v${WINDOWS_SDK_VERSION}.zip`;
    const assets = [{ name, url: `${MIRROR}/svbony/${name}`, pin: { sha256: WINDOWS_SDK_SHA256 } }];
    return { runner, assets, key: cacheKey('svbony-sdk-windows', assets) };
  }
  const base = `${server}/${INDI}/raw/${REF}/libsvbony`;
  const pin = { ref: REF };
  const assets = [
    { name: HEADER, url: `${base}/${HEADER}`, pin },
    { name: `${LIBRARY}${suffix}`, url: `${base}/${LIBRARY}_${arch}.bin`, pin },
  ];
  return { runner, assets, key: cacheKey(`svbony-sdk-${arch}`, assets) };
}

/**
 * The INDI repository's files through GitHub's raw links, and the private
 * mirror, with retries for a 429 or a timeout, as curl's --retry 3
 * --retry-delay 2 had them.
 */
export const DOWNLOAD = { attempts: 4, delayMs: 2_000 };

// --- Placing the SDK on Linux and macOS --------------------------------------------------

const LIBUSB = 'libusb-1.0.0.dylib';

/**
 * Copies the header from `from` into `<prefix>/include` and the library into
 * `<prefix>/lib`, through sudo when `sudo` says the user cannot write there.
 * On Linux the library goes in as libSVBCameraSDK.so.1, with the link
 * libSVBCameraSDK.so, and as root the loader's cache is refreshed. On macOS
 * the library gets its own path as its install name, and when it loads
 * libusb by `@rpath`, a copy of Homebrew's beside it, so that a binary whose
 * rpath lacks the prefix still loads it. No CI runner is an Intel Mac, so
 * the macOS install runs in the tests only.
 */
async function place(from, prefix, { suffix, macos, sudo, run }) {
  const as = (program, args) => (sudo ? run('sudo', [program, ...args]) : run(program, args));
  const lib = path.join(prefix, 'lib');
  const include = path.join(prefix, 'include');
  await as('install', ['-d', lib, include]);
  await as('cp', [path.join(from, HEADER), path.join(include, HEADER)]);
  const file = `${LIBRARY}${suffix}`;
  if (macos) {
    const target = path.join(lib, file);
    await as('cp', [path.join(from, file), target]);
    await as('install_name_tool', ['-id', target, target]);
    const loads = (await run('otool', ['-L', target])).split('\n').map((line) => line.trim().split(' ')[0]);
    if (loads.includes(`@rpath/${LIBUSB}`)) {
      const brew = (await run('brew', ['--prefix', 'libusb'])).trim();
      await as('cp', [path.join(brew, 'lib', LIBUSB), path.join(lib, LIBUSB)]);
      await as('install_name_tool', ['-change', `@rpath/${LIBUSB}`, path.join(lib, LIBUSB), target]);
    }
    io.stdout(`Installed ${file} into ${lib}\n`);
    return;
  }
  await as('cp', [path.join(from, file), path.join(lib, `${file}.1`)]);
  await as('ln', ['-sf', `${file}.1`, path.join(lib, file)]);
  if (sudo || process.getuid?.() === 0) await as('ldconfig', []);
  io.stdout(`Installed ${file}.1, and ${file} linked to it, into ${lib}\n`);
}

// Downloads `assets` into a work directory, which goes when it ends, beside
// `dir`, and places them into `prefix`.
async function fetchAndPlace(assets, dir, prefix, { options, ...how }) {
  await withWorkDir(dir, async (work) => {
    await downloadAll(assets, work, { download: { ...DOWNLOAD, ...options } });
    await place(work, prefix, how);
  });
}

// --- The sudo-free runner ---------------------------------------------------------------

/**
 * The library libsvbony-sys links that a -dev package would give its linker's
 * name: its runtime name, its linker's name, and the package that has it.
 */
export const RUNTIME_LIBRARIES = [['libusb-1.0.so.0', 'libusb-1.0.so', 'libusb-1.0-0']];

// Installs the SDK under `<temp>/svbony-sdk`, where libsvbony-sys's build
// script finds it by SVBONY_SDK_LIB_DIR: the runner cannot sudo, so it cannot
// install libusb's -dev package, which gives `-lusb-1.0` its library, or
// write /usr/local. A link to the system's runtime library stands in for the
// -dev package's, in the one directory the link searches, and with no
// ldconfig run, LD_LIBRARY_PATH finds the SDK at run time.
async function installUnprivileged({ assets, temp, suffix, download: options, run, find }) {
  const prefix = path.join(temp, 'svbony-sdk');
  await fetchAndPlace(assets, prefix, prefix, { options, suffix, macos: false, sudo: false, run });
  const lib = path.join(prefix, 'lib');
  await linkRuntime(lib, RUNTIME_LIBRARIES, { run, find });
  exportVariable('SVBONY_SDK_LIB_DIR', lib);
  exportVariable('LD_LIBRARY_PATH', [lib, process.env.LD_LIBRARY_PATH].filter(Boolean).join(path.delimiter));
}

// --- Windows ----------------------------------------------------------------------------

/**
 * Downloads the vendor zip of `assets` from the private mirror with `token`,
 * checks it, unpacks it, and puts its x64 import library and DLL into `lib`,
 * which is replaced only once both are found.
 */
export async function fetchWindows([asset], lib, { token, download: options, run = command }) {
  if (!token) {
    throw new Error(
      'windows-mirror-token is empty. Pass the PRIVATE_MIRROR_TOKEN repository secret, or, in a job that can build ' +
        'without the real link, such as a fork\'s, skip install-svbony-sdk and set SVBONY_SKIP_NATIVE_LINK=1 instead ' +
        '(ADR-018 §7)',
    );
  }
  setSecret(token);
  await withWorkDir(lib, async (work) => {
    const zip = path.join(work, asset.name);
    try {
      await download(asset, zip, { ...DOWNLOAD, ...options, headers: { authorization: `Bearer ${token}` } });
    } catch (err) {
      const why =
        err instanceof ChecksumError
          ? `the private mirror's ${asset.name} is not the vendor's original, whose SHA-256 this module pins; check the upload before trusting it`
          : `the private mirror did not serve svbony/${asset.name}; check the mirror (tools/private-mirror-worker/) and that the token is its READ_TOKEN`;
      throw new Error(why, { cause: err });
    }
    const unpacked = path.join(work, 'sdk');
    await unpackZips([[zip, unpacked]], { run });
    const staged = path.join(work, 'lib');
    await mkdir(staged);
    for (const file of WINDOWS_FILES) {
      try {
        await copyFile(path.join(unpacked, ...WINDOWS_LIB, file), path.join(staged, file));
      } catch (err) {
        if (err.code !== 'ENOENT') throw err;
        throw new Error(`${asset.name} holds no ${WINDOWS_LIB.join('/')}/${file}: the vendor zip's layout changed`);
      }
    }
    await rm(lib, { recursive: true, force: true });
    await rename(staged, lib);
  });
  io.stdout(`Staged ${WINDOWS_FILES.join(' and ')} from ${asset.name} into ${lib}\n`);
}

// --- Installing -------------------------------------------------------------------------

/**
 * Installs what `resolve` chose for `runner`:
 *
 * - On Linux with `sudo` and on macOS, the downloads go into
 *   `<workspace>/svbony-sdk`, the directory the cache keeps, unless `cached`
 *   says the cache restored them, and from there into PREFIX. On Linux the
 *   loader's cache must then list the library there.
 * - On Linux without `sudo`, they go under `<temp>/svbony-sdk`, uncached,
 *   and SVBONY_SDK_LIB_DIR and LD_LIBRARY_PATH point there.
 * - On Windows, the vendor zip comes from the private mirror with `token`,
 *   uncached, and its import library and DLL go into `<temp>/svbony-sdk/lib`,
 *   which SVBONY_SDK_LIB_DIR and PATH point at: the DLL loads from PATH when
 *   a test runs.
 *
 * `run` runs a program, `find` finds a runtime library; tests replace both.
 */
export async function install({
  runner,
  assets,
  workspace,
  temp,
  sudo,
  cached,
  token,
  download: options,
  run = command,
  find = findRuntime,
}) {
  const { suffix, windows } = forRunner(RUNNERS, runner, 'The SVBony SDK');
  if (windows) {
    const lib = path.join(temp, 'svbony-sdk', 'lib');
    await fetchWindows(assets, lib, { token, download: options, run });
    exportVariable('SVBONY_SDK_LIB_DIR', lib);
    addPath(lib);
    return;
  }
  const macos = runner.startsWith('macOS-');
  if (!sudo && !macos) {
    await installUnprivileged({ assets, temp, suffix, download: options, run, find });
    return;
  }
  const downloads = path.join(workspace, 'svbony-sdk');
  await downloadAll(assets, downloads, { cached, download: { ...DOWNLOAD, ...options } });
  await place(downloads, PREFIX, { suffix, macos, sudo: true, run });
  // Here rather than at the first build's link, whose error would not say why.
  const lib = path.join(PREFIX, 'lib');
  if (!macos && !(await loaderFinds(lib, [`${LIBRARY}${suffix}`], { run }))) {
    throw new Error(`the loader's cache does not list ${LIBRARY}${suffix} in ${lib} after ldconfig: is ${lib} in /etc/ld.so.conf.d?`);
  }
}

/**
 * A developer's install of what `resolve` chose into `dir`, a prefix, PREFIX
 * unless the developer names another: the library goes into `<dir>/lib`, the
 * header into `<dir>/include`, replacing those files and nothing else, with
 * sudo where the developer cannot write. Resolves to the variables a build
 * and its binaries need: SVBONY_SDK_LIB_DIR, unless the library is in PREFIX,
 * where libsvbony-sys's build looks already; and on Linux LD_LIBRARY_PATH,
 * unless the loader's cache lists it where it went, which Fedora's, for one,
 * does not for /usr/local/lib. The SDK for Windows comes from the private
 * mirror only, which developers are not given, so on Windows it throws,
 * saying where to get it.
 */
export async function installAt(dir, { runner, assets }, { download: options, run = command, writable = canWrite } = {}) {
  const { suffix, windows } = forRunner(RUNNERS, runner, 'The SVBony SDK');
  if (windows) {
    throw new Error(
      "the SVBony SDK for Windows comes from the project's private mirror, which developers are not given (ADR-018 §7). " +
        `Download SVBony's SDK for Windows, v${WINDOWS_SDK_VERSION}, from svbony.com, unpack it, and point SVBONY_SDK_LIB_DIR ` +
        `at its ${WINDOWS_LIB.join('\\')} folder, which a binary that loads the DLL also needs on PATH`,
    );
  }
  const sudo = !(await writable(path.join(dir, 'lib'))) || !(await writable(path.join(dir, 'include')));
  if (sudo) io.stdout(`Writing ${dir}/lib and ${dir}/include as root, with sudo\n`);
  const how = { options, suffix, macos: runner.startsWith('macOS-'), sudo, run };
  await fetchAndPlace(assets, path.join(os.tmpdir(), 'svbony-sdk'), dir, how);
  const lib = path.join(dir, 'lib');
  const variables = dir === PREFIX ? {} : { SVBONY_SDK_LIB_DIR: lib };
  if (!how.macos && !(await loaderFinds(lib, [`${LIBRARY}${suffix}`], { run }))) variables.LD_LIBRARY_PATH = lib;
  return variables;
}
