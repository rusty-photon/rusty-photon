// Pebble, Let's Encrypt's ACME test server, and pebble-challtestsrv, its DNS
// sidecar, which doctor's @pebble BDD scenarios run (docs/skills/testing.md
// §5.6): which release assets each runner takes, and their install, into
// <workspace>/pebble on a runner or into a directory a developer names.
//
// The SHA-256 values are pinned against the assets of the letsencrypt/pebble
// release VERSION. Moving VERSION requires refreshing every one — the download
// fails closed on any mismatch.

import { existsSync } from 'node:fs';
import { chmod, mkdir, rename } from 'node:fs/promises';
import path from 'node:path';

import { exportVariable, io, warning } from '../lib/helpers.mjs';
import { cacheKey, download, forRunner, unpack, withWorkDir } from '../lib/install.mjs';

/** The GitHub repo, and its release, whose assets ASSETS pins. */
export const REPO = 'letsencrypt/pebble';
export const VERSION = 'v2.10.1';

/** The binaries, each in an archive of its own, and the variable that locates each. */
export const BINARIES = [
  { name: 'pebble', variable: 'PEBBLE_PATH' },
  { name: 'pebble-challtestsrv', variable: 'PEBBLE_CHALLTESTSRV_PATH' },
];

/** Each runner's release platform, and the SHA-256 of each binary's archive for it. */
export const ASSETS = {
  'Linux-X64': {
    platform: 'linux-amd64',
    sha256: {
      pebble: '4f2fcb5bca8c85c9cf73ad140fccfc0d2be40bd81ab99879c79b7b8a0b4f70ed',
      'pebble-challtestsrv': 'e93a5aa25ecdf3af2f9fbb2de32b0173e64a2eae81002a4ccfe35fa6f4f60b92',
    },
  },
  'Linux-ARM64': {
    platform: 'linux-arm64',
    sha256: {
      pebble: 'b53fd072a69eb7692451de4e8b0667e0bdf5cccd7e36fc51b8eaf2fcc135ed9f',
      'pebble-challtestsrv': 'db8e1a79ccdb2195c489fbe4f40fddb7f30e86f9cd8a07912566ee5025094d6c',
    },
  },
  'macOS-ARM64': {
    platform: 'darwin-arm64',
    sha256: {
      pebble: '09a3a4e6ebed71e8d83294a26d361232262f45a7488f5de7bccb5887b395217f',
      'pebble-challtestsrv': '59bf917fe39c96e2edca980fc2899f4f04aa1ce5485f28d419d18237b902cf82',
    },
  },
  'macOS-X64': {
    platform: 'darwin-amd64',
    sha256: {
      pebble: 'e670ff869886022637e077502a62e7f23be693c45a5a6727ebd76da8fdce64dc',
      'pebble-challtestsrv': '796bd923f2c595dd7bf15ae693096abfb1df962cb3673c7981ff306daa5c4a52',
    },
  },
  'Windows-X64': {
    platform: 'windows-amd64',
    sha256: {
      pebble: '3201b904957c8ad81a27ee879cd246d97e7a9c79b01d840b09ec478f065794b3',
      'pebble-challtestsrv': 'ba4a63d3edfbab639ec1c6fc5b1fe9d36f9d1a724790ef94556c80ead2636c25',
    },
  },
};

/**
 * What `runner` installs: one asset per binary ({ name, url, pin }, with the
 * binary's file name, where its archive holds it, and its variable), and the
 * cache key.
 */
export function resolve({ runner, server = 'https://github.com' }) {
  const { platform, sha256 } = forRunner(ASSETS, runner, 'Pebble');
  const [os, arch] = platform.split('-');
  const exe = runner.startsWith('Windows-') ? '.exe' : '';
  const assets = BINARIES.map(({ name: binary, variable }) => {
    const name = `${binary}-${platform}.tar.gz`;
    return {
      name,
      url: `${server}/${REPO}/releases/download/${VERSION}/${name}`,
      pin: { sha256: sha256[binary] },
      file: `${binary}${exe}`,
      inside: [`${binary}-${platform}`, os, arch, `${binary}${exe}`],
      variable,
    };
  });
  return { assets, key: cacheKey(`pebble-${VERSION}`, assets) };
}

// Downloads, checks and unpacks each binary, then moves them into `dir`, which
// keeps whatever else it holds. Neither moves until both are checked, so a
// failure leaves `dir` as it was.
async function fetchInto(assets, dir, options) {
  await withWorkDir(dir, async (work) => {
    const found = [];
    for (const asset of assets) {
      const archive = path.join(work, asset.name);
      await download(asset, archive, options);
      const unpacked = path.join(work, `${asset.file}-unpacked`);
      await unpack(archive, unpacked);
      const bin = path.join(unpacked, ...asset.inside);
      if (!existsSync(bin)) throw new Error(`${asset.name} holds no ${asset.inside.join('/')}`);
      found.push([bin, path.join(dir, asset.file)]);
    }
    await mkdir(dir, { recursive: true });
    for (const [from, to] of found) await rename(from, to);
  });
}

async function makeRunnable(assets, dir) {
  for (const { file } of assets) {
    if (!file.endsWith('.exe')) await chmod(path.join(dir, file), 0o755);
  }
}

/**
 * Installs both binaries into `<workspace>/pebble`, unless `cached` says the
 * cache restored them there, then exports each one's variable.
 */
export async function install({ assets, workspace, cached, download: options }) {
  const dir = path.join(workspace, 'pebble');
  const missing = assets.filter(({ file }) => !existsSync(path.join(dir, file))).map(({ file }) => file);
  if (cached && missing.length === 0) {
    io.stdout(`Pebble restored from the cache into ${dir}\n`);
  } else {
    if (cached) warning(`The cache restored ${dir} without ${missing.join(' and ')}; downloading Pebble again`);
    await fetchInto(assets, dir, options);
  }
  await makeRunnable(assets, dir);
  // `${workspace}/pebble/...`, joined with '/' on Windows too, as the
  // workflows wrote it before: Bazel forwards both variables into its tests
  // (.bazelrc), so the strings are part of doctor's BDD test's cache key.
  for (const { file, variable } of assets) exportVariable(variable, `${workspace}/pebble/${file}`);
}

/**
 * A developer's install of what `resolve` chose into `dir`, where it replaces
 * the two binaries and nothing else. Resolves to the variables that point
 * doctor's BDD suite at them.
 */
export async function installAt(dir, { assets }, { download: options } = {}) {
  await fetchInto(assets, dir, options);
  await makeRunnable(assets, dir);
  return Object.fromEntries(assets.map(({ file, variable }) => [variable, path.join(dir, file)]));
}
