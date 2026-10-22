// nightly-packages.yml's publish job, before anything is published: the
// release assets' names, and the SHA-256 of each, which the release, its
// notes and the -nightly formulas all read. Conventions:
// docs/skills/ci-logic.md.

import { createReadStream } from 'node:fs';
import { readFile, readdir, rename, writeFile } from 'node:fs/promises';
import path from 'node:path';

import { io, sha256 } from '../lib/helpers.mjs';

/** The sums file, which the release carries beside the assets it lists. */
export const SUMS = 'SHA256SUMS.txt';

/**
 * The kinds of asset the release carries, in the order the sums list them,
 * each with the media type it is uploaded as (gh's, which the release's
 * assets carry).
 */
export const KINDS = [
  ['.deb', 'application/x-debian-package'],
  ['.rpm', 'application/x-rpm'],
  ['.msi', 'application/x-msi'],
  ['.tar.gz', 'application/x-gtar'],
];

/** The media type the sums file is uploaded as. */
export const SUMS_TYPE = 'text/plain; charset=utf-8';

// The files directly in `dir`, sorted.
async function files(dir) {
  return (await readdir(dir, { withFileTypes: true }))
    .filter((entry) => entry.isFile())
    .map(({ name }) => name)
    .sort();
}

/**
 * Renames each rpm in `dir` whose name holds a `^` to that name with `.` in
 * its place. GitHub rewrites `^` to `.` in an uploaded asset's name, so the
 * rename comes before the sums: a sum taken over the old name would never
 * check against the file as downloaded. Only the file's name changes; the
 * rpm's header keeps the true `^` version. Prints the rpms, and resolves to
 * their names.
 */
export async function dotRpmNames(dir) {
  for (const name of await files(dir)) {
    if (name.endsWith('.rpm') && name.includes('^')) {
      await rename(path.join(dir, name), path.join(dir, name.replaceAll('^', '.')));
    }
  }
  const rpms = (await files(dir)).filter((name) => name.endsWith('.rpm'));
  io.stdout(`${rpms.join('\n')}\n`);
  return rpms;
}

/**
 * Writes SUMS in `dir`, in sha256sum's format, which `sha256sum -c` checks:
 * each asset of KINDS in `dir`, kind by kind and by name within one, with
 * its SHA-256. A kind with no asset throws, since the release carries every
 * kind or none. Prints the sums, and resolves to the assets as
 * `{ name, sha256 }`.
 */
export async function writeSums(dir) {
  const names = await files(dir);
  const assets = [];
  for (const [extension] of KINDS) {
    const kind = names.filter((name) => name.endsWith(extension));
    if (kind.length === 0) throw new Error(`${dir} holds no *${extension} asset`);
    for (const name of kind) assets.push({ name, sha256: await sha256(createReadStream(path.join(dir, name))) });
  }
  const text = assets.map(({ name, sha256: sum }) => `${sum}  ${name}\n`).join('');
  await writeFile(path.join(dir, SUMS), text);
  io.stdout(text);
  return assets;
}

/** The assets SUMS in `dir` lists, as `{ name, sha256 }`. A line it cannot read throws. */
export async function readSums(dir) {
  const text = await readFile(path.join(dir, SUMS), 'utf8');
  return text
    .split('\n')
    .filter((line) => line !== '')
    .map((line) => {
      const [, sum, name] = line.match(/^([0-9a-f]{64}) {2}(.+)$/) ?? [];
      if (name === undefined) throw new Error(`${SUMS} has a line that is no sum and name: ${JSON.stringify(line)}`);
      return { name, sha256: sum };
    });
}

/** The media type `name` is uploaded as. */
export function mediaType(name) {
  if (name === SUMS) return SUMS_TYPE;
  const kind = KINDS.find(([extension]) => name.endsWith(extension));
  if (kind === undefined) throw new Error(`${name} is no kind of asset the release carries`);
  return kind[1];
}
