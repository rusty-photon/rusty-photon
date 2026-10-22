// The ZWO ASI camera, EFW filter-wheel and EAF focuser SDKs, MIT-licensed,
// which libzwo-sys links (ADR-008, ADR-010, ADR-014): which files each runner
// takes, and their install where libzwo-sys's build script and the loader
// find them. libzwo-sys links the SDK even into a `simulation` build, which
// removes the hardware, not the link, so every job that builds zwo-camera or
// zwo-focuser installs it.
//
// Linux and macOS take ZWO's prebuilt libraries as the INDI repository
// vendors them, under libasi/ at the commit REF: they have no hash of their
// own, and a commit's content cannot move. Bump REF to adopt a newer SDK.
// The tools crate's macOS builder, `cargo ci build-tarballs`, downloads the
// libraries it ships from REF, which it reads from its line below, and the
// packaging guard (tools/rusty-photon-ci/src/pkg_assets.rs) checks
// build-packages.sh's ZWO_SDK_REF against it: the packages ship the
// libraries CI links.
//
// Windows takes ZWO's developer SDKs from its CDN, whose links are rolling
// "latest" ones, with no version to pin or hash to check. The cache keeps a
// week's download, and when ZWO's CDN fails, as it often does, the newest
// earlier week's stands in. The packaging guard reads CAMERA_SDK_URL and
// EAF_SDK_URL from their lines below, to check that the MSI's build fetches
// from the same links (build-msi.ps1).

import { existsSync, readdirSync } from 'node:fs';
import { copyFile, mkdir, rename, rm } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';

import { addPath, command, exportVariable, io } from '../lib/helpers.mjs';
import {
  cacheKey,
  canWrite,
  checkReplaceable,
  download,
  downloadAll,
  findRuntime,
  forRunner,
  linkRuntime,
  loaderFinds,
  markInstalled,
  restoreKey,
  unpackZips,
  withWorkDir,
} from '../lib/install.mjs';

/** The repository that vendors the libraries for Linux and macOS, and its commit they come from. */
export const INDI = 'indilib/indi-3rdparty';
export const REF = 'b0802f28055b67aa6a99580d260c3bb4c27eba4b';

/** ZWO's rolling links to its developer SDKs for Windows. */
export const CAMERA_SDK_URL = 'https://dl.zwoastro.com/software?app=DeveloperCameraSdk&platform=windows86&region=Overseas';
export const EFW_SDK_URL = 'https://dl.zwoastro.com/software?app=DeveloperEfwSdk&platform=windows86&region=Overseas';
export const EAF_SDK_URL = 'https://dl.zwoastro.com/software?app=DeveloperEafSdk&platform=windows86&region=Overseas';

/**
 * Each runner's SDK: on Linux and macOS, the folder under libasi/ that holds
 * its libraries and the suffix they take; on Windows, ZWO's own SDKs.
 */
export const RUNNERS = {
  'Linux-X64': { arch: 'x64', suffix: '.so' },
  'Linux-ARM64': { arch: 'armv8', suffix: '.so' },
  'macOS-ARM64': { arch: 'mac_arm64', suffix: '.dylib' },
  'macOS-X64': { arch: 'mac_x64', suffix: '.dylib' },
  'Windows-X64': { windows: true },
};

/**
 * The headers, and the license, which goes where they go. bindgen reads the
 * copies libzwo-sys vendors, so these are for completeness, and for the
 * license to travel with the libraries.
 */
export const HEADERS = ['ASICamera2.h', 'EFW_filter.h', 'EAF_focuser.h', 'license.txt'];

/**
 * The libraries, by the names libzwo-sys links (`-lASICamera2`...). All three
 * are required: Bazel builds libzwo-sys with every device's feature, so it
 * links all three.
 */
export const LIBRARIES = ['ASICamera2', 'EFWFilter', 'EAFFocuser'];

/**
 * ZWO's three SDKs for Windows, and the library each holds: by the name ZWO
 * gives it, which its DLL keeps, since the import library names the DLL a
 * binary built with it loads, and the name libzwo-sys links its import
 * library by.
 */
export const WINDOWS_SDKS = [
  { name: 'asi.zip', url: CAMERA_SDK_URL, library: 'ASICamera2', as: 'ASICamera2' },
  { name: 'efw.zip', url: EFW_SDK_URL, library: 'EFW_filter', as: 'EFWFilter' },
  { name: 'eaf.zip', url: EAF_SDK_URL, library: 'EAF_focuser', as: 'EAFFocuser' },
];

/** Where the SDK goes on Linux and macOS: where libzwo-sys's build script looks, in Bazel's actions too. */
export const PREFIX = '/usr/local';

/**
 * Where a Windows runner has LLVM, whose libclang bindgen needs. The hosted
 * image ships it; a runner without it gets it from Chocolatey.
 */
export const LLVM = 'C:\\Program Files\\LLVM\\bin';

// --- Choosing the files -------------------------------------------------------------

/**
 * What `runner` installs: its assets ({ name, url, pin }, and on Windows the
 * library each holds), the cache key, and the start of the key an earlier
 * week's download has (restoreKey), empty but on Windows.
 */
export function resolve({ runner, server = 'https://github.com', now }) {
  const { arch, suffix, windows } = forRunner(RUNNERS, runner, 'The ZWO SDK');
  if (windows) {
    const assets = WINDOWS_SDKS.map((sdk) => ({ ...sdk, pin: { rolling: true } }));
    return { runner, assets, key: cacheKey('zwo-sdk-windows', assets, { now }), restoreKey: restoreKey('zwo-sdk-windows', assets) };
  }
  // INDI names each library <name>.bin, which is ZWO's .so or .dylib.
  const base = `${server}/${INDI}/raw/${REF}/libasi`;
  const pin = { ref: REF };
  const assets = [
    ...HEADERS.map((name) => ({ name, url: `${base}/${name}`, pin })),
    ...LIBRARIES.map((name) => ({ name: `lib${name}${suffix}`, url: `${base}/${arch}/lib${name}.bin`, pin })),
  ];
  return { runner, assets, key: cacheKey(`zwo-sdk-${arch}`, assets), restoreKey: '' };
}

// --- Downloading ----------------------------------------------------------------------

/**
 * The INDI repository's files, through GitHub's raw links, with retries for
 * the 429s and timeouts it gives CI's ephemeral addresses, as curl's
 * --retry 3 --retry-delay 2 had them.
 */
export const INDI_DOWNLOAD = { attempts: 4, delayMs: 2_000 };

/**
 * ZWO's CDN, which drops or black-holes connections routinely: any failure
 * tries again (retryAll, as curl's --retry-all-errors did), and a connection
 * that brings nothing for 20 s is a dead one, as curl's --connect-timeout 20
 * had it. The camera SDK is about 110 MB.
 */
export const CDN_DOWNLOAD = { attempts: 5, delayMs: 5_000, stallMs: 20_000, timeoutMs: 300_000, retryAll: true };

// --- Placing the SDK on Linux and macOS --------------------------------------------------

const LIBUSB = 'libusb-1.0.0.dylib';

/**
 * Copies the headers from `from` into `<prefix>/include` and the libraries
 * into `<prefix>/lib`, through sudo when `sudo` says the user cannot write
 * there. On macOS, it then gives each library its own path as its install
 * name and points each one that loads libusb by `@rpath` at a copy of
 * Homebrew's beside it, so that a binary whose rpath lacks the prefix, as a
 * Bazel test's does, still loads them; on Linux as root, it refreshes the
 * loader's cache.
 */
async function place(from, prefix, { suffix, macos, sudo, run }) {
  const as = (program, args) => (sudo ? run('sudo', [program, ...args]) : run(program, args));
  const lib = path.join(prefix, 'lib');
  const include = path.join(prefix, 'include');
  await as('install', ['-d', lib, include]);
  for (const header of HEADERS) await as('cp', [path.join(from, header), path.join(include, header)]);
  const libraries = LIBRARIES.map((name) => `lib${name}${suffix}`);
  for (const file of libraries) await as('cp', [path.join(from, file), path.join(lib, file)]);
  if (macos) {
    const brew = (await run('brew', ['--prefix', 'libusb'])).trim();
    await as('cp', [path.join(brew, 'lib', LIBUSB), path.join(lib, LIBUSB)]);
    for (const file of libraries) {
      const target = path.join(lib, file);
      await as('install_name_tool', ['-id', target, target]);
      const loads = (await run('otool', ['-L', target])).split('\n').map((line) => line.trim().split(' ')[0]);
      if (loads.includes(`@rpath/${LIBUSB}`)) {
        await as('install_name_tool', ['-change', `@rpath/${LIBUSB}`, path.join(lib, LIBUSB), target]);
      }
    }
  } else if (sudo || process.getuid?.() === 0) {
    await as('ldconfig', []);
  }
  io.stdout(`Installed ${libraries.join(', ')} into ${lib}\n`);
}

// Downloads `assets` into a work directory, which goes when it ends, beside
// `dir`, and places them into `prefix`.
async function fetchAndPlace(assets, dir, prefix, { options, ...how }) {
  await withWorkDir(dir, async (work) => {
    for (const asset of assets) await download(asset, path.join(work, asset.name), { ...INDI_DOWNLOAD, ...options });
    await place(work, prefix, how);
  });
}

// --- The sudo-free runner ---------------------------------------------------------------

/**
 * The libraries libzwo-sys links that a -dev package would give their
 * linker's name: each one's runtime name, its linker's name, and the package
 * that has it.
 */
export const RUNTIME_LIBRARIES = [
  ['libusb-1.0.so.0', 'libusb-1.0.so', 'libusb-1.0-0'],
  ['libudev.so.1', 'libudev.so', 'libudev1'],
];

// Installs the SDK under `<temp>/zwo-sdk`, where libzwo-sys's build script
// finds it by ZWO_SDK_LIB_DIR: the runner cannot sudo, so it cannot install
// the -dev packages, which give `-lusb-1.0` and `-ludev` their libraries, or
// write /usr/local. Links to the system's runtime libraries stand in for the
// -dev packages' links, in the one directory the link searches.
async function installUnprivileged({ assets, temp, suffix, download: options, run, find }) {
  const prefix = path.join(temp, 'zwo-sdk');
  await fetchAndPlace(assets, prefix, prefix, { options, suffix, macos: false, sudo: false, run });
  const lib = path.join(prefix, 'lib');
  await linkRuntime(lib, RUNTIME_LIBRARIES, { run, find });
  // The libraries carry no SONAME, so what links them asks the loader for
  // the bare file name, which it finds by LD_LIBRARY_PATH.
  exportVariable('ZWO_SDK_LIB_DIR', lib);
  exportVariable('LD_LIBRARY_PATH', [lib, process.env.LD_LIBRARY_PATH].filter(Boolean).join(path.delimiter));
}

// --- Windows ----------------------------------------------------------------------------

// The SDK for Windows inside each of ZWO's downloads, beside ones for other
// systems: ASI_Windows_SDK_V1.41.zip.
const WINDOWS_SDK = /Windows.*SDK.*\.zip$/i;

// The builds a search for a library passes over: 32-bit ones (Win32, x86) and
// a demo's, which bundles OpenCV's.
const NOT_X64 = /win32|(^|\/)x86\/|opencv|demo/i;
const X64 = /(^|\/)x64\/|amd64/i;

// Every file under `root`, by its path from there with '/' between folders.
function filesUnder(root) {
  return readdirSync(root, { recursive: true, withFileTypes: true })
    .filter((entry) => entry.isFile())
    .map((entry) => path.relative(root, path.join(entry.parentPath, entry.name)).split(path.sep).join('/'));
}

/**
 * The file named `file`, in any case, that `root` holds the x64 build of: an
 * x64 or amd64 folder first, and among those the first by path, so
 * lib/Windows/x64 comes before lib/Windows7/x64. Throws when there is none.
 */
export function pickX64(root, file, what) {
  const found = filesUnder(root)
    .filter((rel) => path.posix.basename(rel).toLowerCase() === file.toLowerCase() && !NOT_X64.test(rel))
    .sort((a, b) => Number(!X64.test(a)) - Number(!X64.test(b)) || (a < b ? -1 : a > b ? 1 : 0));
  if (found.length === 0) throw new Error(`${what} holds no x64 ${file}`);
  return found[0];
}

/**
 * Unpacks each of ZWO's SDKs in `downloads`, and the SDK for Windows inside
 * it, and puts the x64 import library and DLL of each into `lib`: the import
 * library under the name libzwo-sys links, the DLL under ZWO's. `lib` is
 * replaced only once all six are found.
 */
export async function stageWindows(assets, downloads, lib) {
  await withWorkDir(lib, async (work) => {
    const unpacked = (sdk) => path.join(work, path.basename(sdk.name, '.zip'));
    await unpackZips(assets.map((sdk) => [path.join(downloads, sdk.name), unpacked(sdk)]));
    const inner = assets.flatMap((sdk) =>
      filesUnder(unpacked(sdk))
        .filter((rel) => WINDOWS_SDK.test(path.posix.basename(rel)))
        .map((rel) => [path.join(unpacked(sdk), rel), path.join(unpacked(sdk), `inner-${path.posix.basename(rel, '.zip')}`)]),
    );
    await unpackZips(inner);
    const staged = path.join(work, 'lib');
    await mkdir(staged);
    for (const sdk of assets) {
      for (const [file, as] of [[`${sdk.library}.lib`, `${sdk.as}.lib`], [`${sdk.library}.dll`, `${sdk.library}.dll`]]) {
        const found = pickX64(unpacked(sdk), file, sdk.name);
        await copyFile(path.join(unpacked(sdk), found), path.join(staged, as));
        io.stdout(`${as}: ${sdk.name}'s ${found}\n`);
      }
    }
    await rm(lib, { recursive: true, force: true });
    await rename(staged, lib);
  });
}

async function installWindows({ assets, workspace, temp, cached, restored, download: options, run, llvm }) {
  if (!existsSync(path.join(llvm, 'libclang.dll'))) await run('choco', ['install', 'llvm', '--no-progress', '-y']);
  exportVariable('LIBCLANG_PATH', llvm);
  const downloads = path.join(workspace, 'zwo-sdk');
  const downloaded = await downloadAll(assets, downloads, { cached, restored, download: { ...CDN_DOWNLOAD, ...options } });
  // `<RUNNER_TEMP>\zwo-sdk\lib`, as the action had it before: Bazel passes
  // ZWO_SDK_LIB_DIR to every action on Windows (.bazelrc), so the string is
  // part of every action's cache key there.
  const lib = path.join(temp, 'zwo-sdk', 'lib');
  await stageWindows(assets, downloads, lib);
  // The DLLs load from PATH when a test runs.
  exportVariable('ZWO_SDK_LIB_DIR', lib);
  addPath(lib);
  return downloaded;
}

// --- Installing -------------------------------------------------------------------------

/**
 * Installs what `resolve` chose for `runner`, and resolves to true when
 * `<workspace>/zwo-sdk`, the directory the cache keeps, holds a new download
 * for it to save:
 *
 * - On Linux with `sudo` and on macOS, the downloads go into
 *   `<workspace>/zwo-sdk`, unless `cached` says the cache restored them, and
 *   from there into PREFIX.
 * - On Linux without `sudo`, they go under `<temp>/zwo-sdk`, uncached, and
 *   ZWO_SDK_LIB_DIR and LD_LIBRARY_PATH point there.
 * - On Windows, the downloads go into `<workspace>/zwo-sdk`, unless `cached`
 *   says the cache restored this week's, or the CDN fails and the cache
 *   restored an earlier week's (`restored`); the libraries go into
 *   `<temp>/zwo-sdk/lib`, which ZWO_SDK_LIB_DIR and PATH point at.
 *   LIBCLANG_PATH points at LLVM.
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
  restored,
  download: options,
  run = command,
  find = findRuntime,
  llvm = LLVM,
}) {
  const { suffix, windows } = forRunner(RUNNERS, runner, 'The ZWO SDK');
  if (windows) return installWindows({ assets, workspace, temp, cached, restored, download: options, run, llvm });
  const macos = runner.startsWith('macOS-');
  if (!sudo && !macos) {
    await installUnprivileged({ assets, temp, suffix, download: options, run, find });
    return false;
  }
  const downloads = path.join(workspace, 'zwo-sdk');
  const downloaded = await downloadAll(assets, downloads, { cached, download: { ...INDI_DOWNLOAD, ...options } });
  await place(downloads, PREFIX, { suffix, macos, sudo: true, run });
  return downloaded;
}

/**
 * A developer's install of what `resolve` chose into `dir`. On Linux and
 * macOS, `dir` is a prefix, PREFIX unless the developer names another: the
 * libraries go into `<dir>/lib`, the headers into `<dir>/include`, replacing
 * those files and nothing else, with sudo where the developer cannot write.
 * On Windows, `dir` is installed whole: it is replaced only when it is empty
 * or an earlier install marked it, and only once all six files are found.
 * Resolves to the variables a build and its binaries need: ZWO_SDK_LIB_DIR,
 * unless the libraries are in PREFIX, where libzwo-sys's build looks
 * already; and on Linux LD_LIBRARY_PATH, unless the loader's cache lists them
 * where they went, which Fedora's, for one, does not for /usr/local/lib.
 */
export async function installAt(
  dir,
  { runner, assets },
  { download: options, run = command, writable = canWrite, now = new Date() } = {},
) {
  const { suffix, windows } = forRunner(RUNNERS, runner, 'The ZWO SDK');
  if (windows) {
    await checkReplaceable(dir);
    await withWorkDir(dir, async (work) => {
      for (const asset of assets) await download(asset, path.join(work, asset.name), { ...CDN_DOWNLOAD, ...options });
      await stageWindows(assets, work, dir);
    });
    const day = now.toISOString().slice(0, 10);
    await markInstalled(dir, `ZWO's SDKs for Windows, ${assets.map(({ name }) => name).join(', ')}, as its CDN served them on ${day}`);
    return { ZWO_SDK_LIB_DIR: dir };
  }
  const sudo = !(await writable(path.join(dir, 'lib'))) || !(await writable(path.join(dir, 'include')));
  if (sudo) io.stdout(`Writing ${dir}/lib and ${dir}/include as root, with sudo\n`);
  const how = { options, suffix, macos: runner.startsWith('macOS-'), sudo, run };
  await fetchAndPlace(assets, path.join(os.tmpdir(), 'zwo-sdk'), dir, how);
  const lib = path.join(dir, 'lib');
  const variables = dir === PREFIX ? {} : { ZWO_SDK_LIB_DIR: lib };
  // The libraries carry no SONAME, and libzwo-sys adds no rpath, so a binary
  // finds them with no LD_LIBRARY_PATH only where the loader's cache lists them.
  const files = LIBRARIES.map((name) => `lib${name}${suffix}`);
  if (!how.macos && !(await loaderFinds(lib, files, { run }))) variables.LD_LIBRARY_PATH = lib;
  return variables;
}
