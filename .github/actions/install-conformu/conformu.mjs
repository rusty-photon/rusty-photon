// ConformU, ASCOM's conformance checker, which every service's
// conformu_integration test drives through CONFORMU_PATH: which release asset
// each runner takes, and its install, into RUNNER_TEMP/conformu on a runner
// or into a directory a developer names.
//
// Unlike the other install-* modules it pins no release. CI and every new
// docs/validation/ record run the latest ConformU
// (docs/skills/hardware-validation.md), so the release is the one GitHub's
// API names latest when the install runs, and each asset is checked against
// the SHA-256 the API gives for it. GitHub gives one only for assets uploaded
// since mid-2025, ConformU v4.2.1 on; a release without one is refused.

import { mkdtemp, mkdir, rename, rm } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';

import { github } from '../lib/github.mjs';
import { command, execute } from '../lib/helpers.mjs';
import { checkReplaceable, download, forRunner, markInstalled, unpack, withWorkDir } from '../lib/install.mjs';

/** The GitHub repo whose latest release is installed. */
export const REPO = 'ASCOMInitiative/ConformU';

/**
 * Each runner's release asset, as a pattern, since every name carries the
 * release's version, and the Windows installer's its build too
 * (conformu.4.5.3.linux-x64.tar.gz, ConformU-Installer.4.5.3.dmg,
 * ConformU.4.5.3+56334.Setup.exe). The Linux archives are gzipped and unpack
 * flat; the macOS disk image holds ConformU.app, a universal binary for both
 * Mac architectures; the Windows installer is Inno Setup's.
 */
export const ASSETS = {
  'Linux-X64': /^conformu\.[^/\\]+\.linux-x64\.tar\.gz$/,
  'Linux-ARM64': /^conformu\.[^/\\]+\.linux-arm64\.tar\.gz$/,
  'macOS-ARM64': /^ConformU-Installer\.[^/\\]+\.dmg$/,
  'macOS-X64': /^ConformU-Installer\.[^/\\]+\.dmg$/,
  'Windows-X64': /^ConformU\.[^/\\]+\.Setup\.exe$/,
};

// How each OS's asset installs: unpacked, copied out of the disk image, or
// run as an installer.
const KINDS = { Linux: 'archive', macOS: 'image', Windows: 'installer' };

// A release tag, v-prefixed, as ConformU's are; some older ones carry a
// build number after a +.
const TAG = /^v(\d+\.\d+\.\d+)(?:\+[0-9A-Za-z.]+)?$/;

const SHA256 = /^sha256:([0-9a-f]{64})$/;

/**
 * Where ConformU's Windows installer puts it, `{autopf}\ASCOM\ConformU`. Its
 * script disables the directory page, so the install goes there on every
 * machine, and no command line here moves it.
 */
export function windowsDir(env = process.env) {
  return path.win32.join(env.ProgramFiles || 'C:\\Program Files', 'ASCOM', 'ConformU');
}

/** The binary that each kind of install puts in `dir`. */
export function binaryIn(dir, kind) {
  if (kind === 'installer') return path.win32.join(dir, 'ConformU.exe');
  if (kind === 'image') return path.join(dir, 'ConformU.app', 'Contents', 'Resources', 'conformu');
  return path.join(dir, 'conformu');
}

/**
 * What `runner` installs from ConformU's latest release: its version, the
 * kind of install, and its asset ({ name, url, pin }), pinned to the SHA-256
 * GitHub's API gives for it. `token`, when given, authenticates the API
 * request, which a developer's install makes without one.
 */
export async function resolve({ runner, token, api = 'https://api.github.com', server = 'https://github.com' }) {
  const wanted = forRunner(ASSETS, runner, 'ConformU');
  const response = await github('Reading ConformU\'s latest release', `${api}/repos/${REPO}/releases/latest`, { token });
  const release = await response.json();
  const tag = release.tag_name;
  const version = typeof tag === 'string' ? tag.match(TAG)?.[1] : undefined;
  if (version === undefined) {
    throw new Error(`ConformU's latest release has the tag ${JSON.stringify(tag)}, which names no version`);
  }
  const matches = (release.assets ?? []).filter(({ name }) => wanted.test(name));
  if (matches.length !== 1) {
    const found = matches.length === 0 ? 'none' : matches.map(({ name }) => name).join(', ');
    throw new Error(`ConformU ${tag} should have one asset ${wanted} for ${runner}; it has ${found}`);
  }
  const [{ name, digest }] = matches;
  const sha256 = typeof digest === 'string' ? digest.match(SHA256)?.[1] : undefined;
  if (sha256 === undefined) {
    throw new Error(
      `ConformU ${tag}'s ${name} has no SHA-256 from GitHub to check it against (its digest is ${JSON.stringify(digest ?? null)}). ` +
        'GitHub gives one only for assets uploaded since mid-2025, and an unchecked install is not supported',
    );
  }
  const url = `${server}/${REPO}/releases/download/${encodeURIComponent(tag)}/${encodeURIComponent(name)}`;
  return { version, kind: KINDS[runner.split('-')[0]], asset: { name, url, pin: { sha256 } } };
}

/**
 * Throws unless `binary --version` names `version`: the install runs, and is
 * the release that was resolved.
 */
export async function checkVersion(binary, version, { execute: start = execute } = {}) {
  const printed = await start(binary, ['--version'], { signal: AbortSignal.timeout(120_000) });
  const found = printed.match(/Conform Universal (\d+\.\d+\.\d+)/)?.[1];
  if (found === undefined) throw new Error(`${binary} --version printed no "Conform Universal <version>"`);
  if (found !== version) throw new Error(`${binary} is ConformU ${found}, where ${version} was installed`);
}

// Copies ConformU.app out of the disk image `image` into `tree`, mounting it
// in `work`, out of sight of the Finder.
async function copyApp(image, tree, work, { run }) {
  const mount = path.join(work, 'mount');
  await mkdir(mount);
  await run('hdiutil', ['attach', image, '-readonly', '-nobrowse', '-noautoopen', '-mountpoint', mount]);
  try {
    await mkdir(tree);
    await run('ditto', [path.join(mount, 'ConformU.app'), path.join(tree, 'ConformU.app')]);
  } finally {
    await run('hdiutil', ['detach', mount, '-force']);
  }
}

// Runs the Windows installer from a scratch directory of its own: the
// install directory's parent is Program Files, which only an administrator
// can write.
async function runInstaller(asset, { download: options, execute: start }) {
  const work = await mkdtemp(path.join(os.tmpdir(), 'conformu-setup-'));
  try {
    const installer = path.join(work, asset.name);
    await download(asset, installer, options);
    await start(installer, ['/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/SP-']);
  } finally {
    await rm(work, { recursive: true, force: true });
  }
}

/**
 * Installs what `resolve` chose into `dir` and resolves to CONFORMU_PATH, the
 * binary's path, once it has answered `--version` with the release's version.
 * An archive or disk image replaces `dir` whole, which must be missing, empty
 * or an earlier install's (checkReplaceable). The Windows installer installs
 * into windowsDir() alone, so any other `dir` is refused there; it needs an
 * administrator, and asks for one unless the process is elevated.
 */
export async function installAt(dir, { version, kind, asset }, { download: options, run = command, execute: start = execute } = {}) {
  if (kind === 'installer') {
    if (path.win32.resolve(dir).toLowerCase() !== path.win32.resolve(windowsDir()).toLowerCase()) {
      throw new Error(`ConformU's Windows installer installs into ${windowsDir()} and nowhere else; it cannot install into ${dir}`);
    }
    await runInstaller(asset, { download: options, execute: start });
  } else {
    await checkReplaceable(dir);
    await withWorkDir(dir, async (work) => {
      const file = path.join(work, asset.name);
      await download(asset, file, options);
      const tree = path.join(work, 'conformu');
      if (kind === 'image') await copyApp(file, tree, work, { run });
      else await unpack(file, tree);
      await rm(dir, { recursive: true, force: true });
      await rename(tree, dir);
    });
    await markInstalled(dir, `ConformU ${version}, ${asset.name}`);
  }
  const binary = binaryIn(dir, kind);
  await checkVersion(binary, version, { execute: start });
  return { CONFORMU_PATH: binary };
}
