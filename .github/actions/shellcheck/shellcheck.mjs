// shellcheck.yml's job: shellcheck at a pinned release, downloaded and checked
// against its SHA-256, run over the runner pool's host shell in tools/ci.
// Conventions: docs/skills/ci-logic.md.
//
// PINNED, and not merely for reproducibility: shellcheck's finding set moves
// between releases, so an unpinned gate lints different code on different
// days. The runner image ships 0.9.0, which reports the harness stubs under
// SC2317 where 0.11 reports them under SC2329, so a suppression written
// against a locally installed 0.11 passes locally and fails here, for a
// difference that has nothing to do with the code. Same reasoning as
// buildifier being pinned through MODULE.bazel: the developer's run and CI's
// must be the same run. Bump VERSION deliberately, with its SHA-256.

import { existsSync } from 'node:fs';
import { mkdir, readdir, rename } from 'node:fs/promises';
import path from 'node:path';

import { command, execute, io } from '../lib/helpers.mjs';
import { download, forRunner, unpack, withWorkDir } from '../lib/install.mjs';

/** The GitHub repo, and its release, whose asset ASSETS pins. */
export const REPO = 'koalaman/shellcheck';
export const VERSION = 'v0.11.0';

/** Each runner's release platform, and the SHA-256 of its archive. */
export const ASSETS = {
  'Linux-X64': { platform: 'linux.x86_64', sha256: '8c3be12b05d5c177a04c29e3c78ce89ac86f1595681cab149b65b97c4e227198' },
};

/** The directory it lints, under the checkout. */
export const LINTED = 'tools/ci';

/** The release asset `runner` installs, as `{ name, url, pin }`. */
export function resolve({ runner, server = 'https://github.com' }) {
  const { platform, sha256 } = forRunner(ASSETS, runner, 'shellcheck');
  const name = `shellcheck-${VERSION}.${platform}.tar.xz`;
  return { name, url: `${server}/${REPO}/releases/download/${VERSION}/${name}`, pin: { sha256 } };
}

/**
 * Downloads `asset`, checks and unpacks it, and moves its shellcheck into
 * `dir`, with the mode the archive gives it, which lets it run; then checks
 * the binary reports VERSION, running it by its path, and resolves to that
 * path. `run` runs it (`command`).
 */
export async function install(asset, dir, { download: options, run = command } = {}) {
  const inside = [`shellcheck-${VERSION}`, 'shellcheck'];
  const file = path.join(dir, 'shellcheck');
  await withWorkDir(dir, async (work) => {
    const archive = path.join(work, asset.name);
    await download(asset, archive, options);
    const unpacked = path.join(work, 'unpacked');
    await unpack(archive, unpacked);
    const bin = path.join(unpacked, ...inside);
    if (!existsSync(bin)) throw new Error(`${asset.name} holds no ${inside.join('/')}`);
    await mkdir(dir, { recursive: true });
    await rename(bin, file);
  });
  const banner = await run(file, ['--version']);
  const version = banner.match(/^version: (\S+)$/m)?.[1];
  if (`v${version}` !== VERSION) {
    throw new Error(`${file} reports ${version === undefined ? 'no version' : `version ${version}`}, not ${VERSION}`);
  }
  io.stdout(banner);
  return file;
}

/**
 * The scripts it lints, under `root`: every file in LINTED named *.sh, and the
 * runner guest's rp-set-hostname, as paths from `root` with forward slashes,
 * sorted. A symbolic link is left out.
 */
export async function scripts(root) {
  const found = [];
  for (const entry of await readdir(path.join(root, LINTED), { recursive: true, withFileTypes: true })) {
    if (!entry.isFile() || !(entry.name.endsWith('.sh') || entry.name === 'rp-set-hostname')) continue;
    found.push(path.relative(root, path.join(entry.parentPath, entry.name)).split(path.sep).join('/'));
  }
  return found.sort();
}

/**
 * Lints the scripts under `root`, the step's working directory, with the
 * shellcheck at `shellcheck`, run by `run` (`execute`, which copies its
 * findings to the log and fails on a finding). No script to lint throws.
 */
export async function lint({ shellcheck, root, run = execute }) {
  const list = await scripts(root);
  if (list.length === 0) throw new Error(`found no script to lint under ${LINTED}`);
  io.stdout(`linting ${list.length} script(s):\n${list.map((script) => `  ${script}\n`).join('')}`);
  await run(shellcheck, list);
}
