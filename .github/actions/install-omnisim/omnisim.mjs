// OmniSim, the ASCOM Alpaca simulators rp's BDD scenarios spawn: which
// release asset each runner takes, and its install, into <workspace>/omnisim
// on a runner or into a directory a developer names.
//
// The SHA-256 values are pinned against the OmniSim release assets named
// below. They point at the patched fork ivonnyssen/ASCOM.Alpaca.Simulators
// (REPO), tag v0.5.0-467.2 = v0.5.0-326.4 + --multi-instance +
// OMNISIM_SETTINGS_DIR.
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
// Reverting to upstream (REPO = ASCOMInitiative/ASCOM.Alpaca.Simulators,
// VERSION = v0.5.0) is no longer SHA-refresh-only: upstream has no
// --multi-instance, so the bdd-infra spawn path would have to be reverted
// too. Moving REPO or VERSION requires refreshing the SHAs — the download
// fails closed on any mismatch.

import { existsSync } from 'node:fs';
import { rename, rm } from 'node:fs/promises';
import path from 'node:path';

import { addPath, exportVariable, io, warning } from '../lib/helpers.mjs';
import {
  cacheKey,
  checkReplaceable,
  download,
  forRunner,
  makeRunnable,
  markInstalled,
  onlyDirectory,
  unpack,
  withWorkDir,
} from '../lib/install.mjs';

/** The GitHub repo, and its release, whose assets ASSETS pins. */
export const REPO = 'ivonnyssen/ASCOM.Alpaca.Simulators';
export const VERSION = 'v0.5.0-467.2';

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

/**
 * What `runner` installs: the asset ({ name, url, pin }), the binary's file
 * name, and the cache key.
 */
export function resolve({ runner, server = 'https://github.com' }) {
  const { name, sha256 } = forRunner(ASSETS, runner, 'OmniSim');
  const asset = { name, url: `${server}/${REPO}/releases/download/${VERSION}/${name}`, pin: { sha256 } };
  return {
    asset,
    binary: runner.startsWith('Windows-') ? 'ascom.alpaca.simulators.exe' : 'ascom.alpaca.simulators',
    key: cacheKey(`omnisim-${VERSION}`, [asset]),
  };
}

// Downloads, checks and unpacks the release into `dir`, replacing what is
// there. The release archive holds one directory, which becomes `dir`.
async function unpackInto({ asset, binary, dir, download: options }) {
  await withWorkDir(dir, async (work) => {
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
  });
}

/**
 * Installs OmniSim into `<workspace>/omnisim`, unless `cached` says the cache
 * restored it there, then puts that directory on PATH and exports
 * OMNISIM_PATH, the binary's path.
 */
export async function install({ asset, binary, workspace, cached, download: options }) {
  const dir = path.join(workspace, 'omnisim');
  if (cached && existsSync(path.join(dir, binary))) {
    io.stdout(`OmniSim restored from the cache into ${dir}\n`);
  } else {
    if (cached) warning(`The cache restored ${dir} without ${binary}; downloading OmniSim again`);
    await unpackInto({ asset, binary, dir, download: options });
  }
  await makeRunnable(dir, binary);
  // `${workspace}/omnisim/...`, joined with '/' on Windows too, as the
  // workflows wrote it before: Bazel forwards OMNISIM_PATH into its tests
  // (.bazelrc), so the string is part of every BDD test's cache key, and a
  // new spelling of the same path would miss every one of them once.
  addPath(`${workspace}/omnisim`);
  exportVariable('OMNISIM_PATH', `${workspace}/omnisim/${binary}`);
}

/**
 * A developer's install of what `resolve` chose into `dir`, which it replaces
 * only when it is empty or an earlier install marked it. Resolves to the
 * variables that point the BDD suites at it.
 */
export async function installAt(dir, { asset, binary }, { download: options } = {}) {
  await checkReplaceable(dir);
  await unpackInto({ asset, binary, dir, download: options });
  await markInstalled(dir, `OmniSim ${VERSION}, ${asset.name}`);
  await makeRunnable(dir, binary);
  return { OMNISIM_PATH: path.join(dir, binary) };
}
