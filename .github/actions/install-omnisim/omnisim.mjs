// OmniSim, the ASCOM Alpaca simulators rp's BDD scenarios spawn: which
// release asset each runner takes, and its install into <workspace>/omnisim.
//
// The SHA-256 values are pinned against the OmniSim release assets named
// below. They point at the patched fork ivonnyssen/ASCOM.Alpaca.Simulators
// (the action's default `repo` input), tag v0.5.0-467.2 = v0.5.0-326.4 +
// --multi-instance + OMNISIM_SETTINGS_DIR.
//
// 467.x (rusty-photon #467) enables one private OmniSim per test process
// — the enabler for parallel OmniSim BDD suites and rp:bdd sharding.
// 467.1 adds `--multi-instance` (skips upstream's machine-global
// single-instance guard: named Mutex + named-pipe command server);
// 467.2 adds the OMNISIM_SETTINGS_DIR env var (re-roots the profile
// store — the default location is not redirectable on Windows or macOS,
// where concurrent instances otherwise share one store and leak
// persisted settings across suites; that took down 4 of 8 rp:bdd shards
// on macOS CI). bdd-infra ALWAYS passes both now, so this pin is a hard
// floor: an older binary exits at startup whenever another instance is
// running, or silently shares the profile store.
//
// The carried 326.x series fixes TelescopeHardware slew bugs behind the
// center_on_target hang/flake (rusty-photon/rusty-photon #326, #319):
// 326.1 locked the slew-engine writers + IsSlewing/RA/Dec readers;
// 326.2 adds the AtPark/SlewState readers (closes the park-poll hang);
// 326.3 disposes the prior slew timer on Init() and resets the slew
// state under hardwareLock; 326.4 closes the center_on_target IsSlewing
// wedge proper (issue #319). ConformU-clean vs upstream.
//
// Reverting to upstream (repo=ASCOMInitiative/..., version=v0.5.0) is no
// longer SHA-refresh-only: upstream has no --multi-instance, so the
// bdd-infra spawn path would have to be reverted too. Bumping either
// input requires refreshing the SHAs — the download fails closed on
// any mismatch.

import { existsSync } from 'node:fs';
import { chmod, rename, rm } from 'node:fs/promises';
import path from 'node:path';

import { addPath, exportVariable, io, warning } from '../lib/helpers.mjs';
import { cacheKey, download, forRunner, onlyDirectory, unpack } from '../lib/install.mjs';

/** Each runner's release asset and its SHA-256. */
export const ASSETS = {
  'Linux-X64': {
    name: 'ascom.alpaca.simulators.linux-x64.tar.xz',
    sha256: '238cc7aa301b2d6cb3ad3ea07de8e55fa190007b086d49291eccd608741f834b',
  },
  'Linux-ARM64': {
    name: 'ascom.alpaca.simulators.linux-aarch64.tar.xz',
    sha256: '22706e0f52d3ac46ab2c8612c4f231bb6e8c01a9eb814b3325f350ed5239ff49',
  },
  'macOS-ARM64': {
    name: 'ascom.alpaca.simulators.macos-arm64.zip',
    sha256: '8abc316ae14b9f40e860ff2423b377c32e6b348cd8d8b12dee4f4b37654aa4c3',
  },
  'macOS-X64': {
    name: 'ascom.alpaca.simulators.macos-x64.zip',
    sha256: '9f08339510ac72c14c073fc7e1d5cb7b14133d83564a5bbd1a95d7c3838d33e0',
  },
  'Windows-X64': {
    name: 'ascom.alpaca.simulators.windows-x64.zip',
    sha256: 'dc5dc71c7149b66ad9b5cc728d3a3c384d0b12587dc6f3b3d405f28fcf5f46ba',
  },
};

// What a repo and a release tag may hold, so neither can change the URL's
// shape: no '/', and no part that is only '.' or '..', which a URL resolves.
const PART = '(?!\\.{1,2}(?:/|$))[A-Za-z0-9_.-]+';
const REPO = new RegExp(`^${PART}/${PART}$`);
const VERSION = /^(?!\.{1,2}$)[A-Za-z0-9_.+-]+$/;

/**
 * What `runner` installs from release `version` of the GitHub repo `repo`:
 * the asset ({ name, url, pin }), the binary's file name, and the cache key.
 */
export function resolve({ repo, version, runner, server = 'https://github.com' }) {
  if (!REPO.test(repo)) throw new Error(`repo ${JSON.stringify(repo)} is not a GitHub owner/name`);
  if (!VERSION.test(version)) throw new Error(`version ${JSON.stringify(version)} is not a release tag this action takes`);
  const { name, sha256 } = forRunner(ASSETS, runner, 'OmniSim');
  const asset = { name, url: `${server}/${repo}/releases/download/${version}/${name}`, pin: { sha256 } };
  return {
    asset,
    binary: runner.startsWith('Windows-') ? 'ascom.alpaca.simulators.exe' : 'ascom.alpaca.simulators',
    key: cacheKey(`omnisim-${version}`, [asset]),
  };
}

/**
 * Installs OmniSim into `<workspace>/omnisim`, unless `cached` says the cache
 * restored it there, then puts that directory on PATH and exports
 * OMNISIM_PATH, the binary's path. The release archive holds one directory,
 * which becomes `omnisim`.
 */
export async function install({ asset, binary, workspace, cached, download: options }) {
  const dir = path.join(workspace, 'omnisim');
  const bin = path.join(dir, binary);
  if (cached && existsSync(bin)) {
    io.stdout(`OmniSim restored from the cache into ${dir}\n`);
  } else {
    if (cached) warning(`The cache restored ${dir} without ${binary}; downloading OmniSim again`);
    const work = path.join(workspace, 'omnisim-download');
    await rm(work, { recursive: true, force: true });
    const archive = path.join(work, asset.name);
    await download(asset, archive, options);
    const unpacked = path.join(work, 'unpacked');
    await unpack(archive, unpacked);
    const top = await onlyDirectory(unpacked);
    if (!existsSync(path.join(top, binary))) {
      throw new Error(`${asset.name} holds no ${binary} in ${path.basename(top)}`);
    }
    await rm(dir, { recursive: true, force: true });
    await rename(top, dir);
    await rm(work, { recursive: true, force: true });
  }
  if (!binary.endsWith('.exe')) await chmod(bin, 0o755);
  // `${workspace}/omnisim/...`, joined with '/' on Windows too, as the
  // workflows wrote it before: Bazel forwards OMNISIM_PATH into its tests
  // (.bazelrc), so the string is part of every BDD test's cache key, and a
  // new spelling of the same path would miss every one of them once.
  addPath(`${workspace}/omnisim`);
  exportVariable('OMNISIM_PATH', `${workspace}/omnisim/${binary}`);
}
