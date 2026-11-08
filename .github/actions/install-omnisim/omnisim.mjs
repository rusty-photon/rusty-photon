// OmniSim, the ASCOM Alpaca simulators rp's BDD scenarios spawn: which
// release asset each runner takes, and its install, into <workspace>/omnisim
// on a runner or into a directory a developer names.
//
// The SHA-256 values are pinned against the OmniSim release assets named
// below. They point at the patched fork ivonnyssen/ASCOM.Alpaca.Simulators
// (REPO), tag v0.5.0-467.3 = v0.5.0-326.4 + --multi-instance +
// OMNISIM_SETTINGS_DIR, with --multi-instance also skipping the Windows
// tray icon.
//
// 467.x (rusty-photon #467) enables one private OmniSim per test process
// — the enabler for parallel OmniSim BDD suites and rp:bdd sharding.
// 467.1 adds `--multi-instance` (skips upstream's machine-global
// single-instance guard: named Mutex + named-pipe command server);
// 467.2 adds the OMNISIM_SETTINGS_DIR env var (re-roots the profile
// store — the default location is not redirectable on Windows or macOS,
// where concurrent instances otherwise share one store and leak
// persisted settings across suites; that took down 4 of 8 rp:bdd shards
// on macOS CI); 467.3 makes --multi-instance skip the Windows tray icon
// too, whose identity (a hash of the binary's path) is as machine-global
// as the mutex: instances starting together fight over it, and the loser
// dies of an unhandled .NET exception (exit code 0xe0434352) before it
// serves anything. bdd-infra ALWAYS passes both, so this pin is a hard
// floor: an older binary exits at startup whenever another instance is
// running, silently shares the profile store, or crashes on the icon.
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
export const VERSION = 'v0.5.0-467.3';

/** Each runner's release asset and its SHA-256. */
export const ASSETS = {
  'Linux-X64': {
    name: 'ascom.alpaca.simulators.linux-x64.tar.xz',
    sha256: '3063f9d6476b64c31a6f849d89972c5f0282ce72154338f4e1cf7c6d1f52c5a2',
  },
  'Linux-ARM64': {
    name: 'ascom.alpaca.simulators.linux-aarch64.tar.xz',
    sha256: '395e82e341fd625d6f69bf1c37f63a70cbab422da61a3db8f7cafa8f80c4df35',
  },
  'macOS-ARM64': {
    name: 'ascom.alpaca.simulators.macos-arm64.zip',
    sha256: '878e49a1727d6ae8bc38cc3a74678a5cd296dfe12ebd2f4355531fac1ff7bc9d',
  },
  'macOS-X64': {
    name: 'ascom.alpaca.simulators.macos-x64.zip',
    sha256: '06ebd4295090a22008e2532bd487675d94c8f406832b57068b86bb0d87d8cc0f',
  },
  'Windows-X64': {
    name: 'ascom.alpaca.simulators.windows-x64.zip',
    sha256: '6b8e5859e882f033368966877fbcbf1caae66219e24d83f1a348861cfd4686ea',
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
