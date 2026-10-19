// What every install-* action does: choose the asset for the runner, download
// it, check it against its pin and unpack it (plan item J2,
// docs/plans/ci-shell-to-code.md). An action keeps its assets in a table keyed
// by runner, and runs as a composite of three steps (docs/skills/ci-logic.md
// §An action that needs another action): resolve the asset and its cache key,
// restore the cache with that key, then install on a miss and export the
// paths either way. A developer installs the same release with the same code
// through `bazel run //.github/actions:install` (local-install/).
//
// Every asset carries one of three kinds of pin:
//
// - `{ sha256 }`: the download is checked against it.
// - `{ ref }`: a git commit, which the asset's URL names. The blobs the INDI
//   repository vendors have no hash of their own, and a commit's content
//   cannot move.
// - `{ rolling: true }`: none. A vendor's "latest" link has no version to pin,
//   so the cache key carries the ISO week, and the first run each week fetches
//   it again.
//
// The cache key carries the pin, so moving a pin misses the old cache.

import { createHash } from 'node:crypto';
import { createWriteStream } from 'node:fs';
import { mkdir, mkdtemp, readdir, rename, rm } from 'node:fs/promises';
import path from 'node:path';
import { Readable, Transform } from 'node:stream';
import { pipeline } from 'node:stream/promises';

import { command, fetchWithRetry, io, isTransient, retry } from './helpers.mjs';

// --- Choosing the asset ---------------------------------------------------------

/** The runner as the tables key it, `${RUNNER_OS}-${RUNNER_ARCH}`: Linux-X64. */
export function runnerKey({ os = process.env.RUNNER_OS, arch = process.env.RUNNER_ARCH } = {}) {
  if (!os || !arch) {
    throw new Error('RUNNER_OS and RUNNER_ARCH are not set: this runs only as a GitHub Actions step');
  }
  return `${os}-${arch}`;
}

// RUNNER_OS and RUNNER_ARCH for each of Node's process.platform and
// process.arch values a runner can have.
const RUNNER_OS = { linux: 'Linux', darwin: 'macOS', win32: 'Windows' };
const RUNNER_ARCH = { x64: 'X64', arm64: 'ARM64', ia32: 'X86', arm: 'ARM' };

/**
 * The key of a runner with this machine's OS and architecture, for an install
 * a developer runs outside CI: Linux-X64 on an x86-64 Linux machine.
 */
export function hostKey({ platform = process.platform, arch = process.arch } = {}) {
  if (!Object.hasOwn(RUNNER_OS, platform) || !Object.hasOwn(RUNNER_ARCH, arch)) {
    throw new Error(`no GitHub runner has this machine's OS and architecture, ${platform} ${arch}`);
  }
  return `${RUNNER_OS[platform]}-${RUNNER_ARCH[arch]}`;
}

/** `table`'s entry for `runner`, or an error naming the runners it has one for. */
export function forRunner(table, runner, what) {
  if (!Object.hasOwn(table, runner)) {
    throw new Error(`${what} has no asset for ${runner}; it has one for ${Object.keys(table).join(', ')}`);
  }
  return table[runner];
}

// --- Pins and the cache key -----------------------------------------------------

const SHA256 = /^[0-9a-f]{64}$/;
const COMMIT = /^[0-9a-f]{40}$/;

/** The ISO 8601 week `date` falls in, in UTC, as `date -u +%G-%V` prints it: 2026-42. */
export function isoWeek(date) {
  const day = new Date(Date.UTC(date.getUTCFullYear(), date.getUTCMonth(), date.getUTCDate()));
  // A week belongs to the year its Thursday falls in.
  day.setUTCDate(day.getUTCDate() + 4 - (day.getUTCDay() || 7));
  const year = day.getUTCFullYear();
  const week = Math.ceil(((day - Date.UTC(year, 0, 1)) / 86_400_000 + 1) / 7);
  return `${year}-${String(week).padStart(2, '0')}`;
}

/**
 * What `pin` puts in a cache key, once it is known to be well formed: the
 * SHA-256, the commit, or the ISO week of `now` for a rolling download. A ref
 * must be a commit's full SHA; a branch or tag could move.
 */
export function pinKey(pin, now = new Date()) {
  const kinds = ['sha256', 'ref', 'rolling'].filter((kind) => pin?.[kind] !== undefined);
  if (kinds.length !== 1) {
    throw new Error(`a pin is one of { sha256 }, { ref } and { rolling: true }; got ${JSON.stringify(pin)}`);
  }
  if (pin.sha256 !== undefined) {
    if (!SHA256.test(pin.sha256)) throw new Error(`pin sha256 ${JSON.stringify(pin.sha256)} is not a SHA-256 in lowercase hex`);
    return pin.sha256;
  }
  if (pin.ref !== undefined) {
    if (!COMMIT.test(pin.ref)) throw new Error(`pin ref ${JSON.stringify(pin.ref)} is not a commit's full SHA, so what it names could move`);
    return pin.ref;
  }
  if (pin.rolling !== true) throw new Error(`pin rolling must be true; got ${JSON.stringify(pin.rolling)}`);
  return `week-${isoWeek(now)}`;
}

/**
 * The cache key of `assets`, which an action keeps under `name`: each asset's
 * name and pin. `name` names the action and what it keeps; change it when
 * what the action puts in its cached directory changes.
 */
export function cacheKey(name, assets, { now } = {}) {
  return ['install', name, ...assets.flatMap((asset) => [asset.name, pinKey(asset.pin, now)])].join('-');
}

// --- Downloading ------------------------------------------------------------------

/** A download whose SHA-256 is not its pin's. */
export class ChecksumError extends Error {
  constructor(message) {
    super(message);
    this.name = 'ChecksumError';
  }
}

// Streams `url` into `file`, aborting when no data comes for `stallMs`, and
// resolves to the SHA-256 of what it wrote.
async function fetchTo(url, file, { stallMs, timeoutMs }) {
  const stall = new AbortController();
  let timer;
  const wait = () => {
    clearTimeout(timer);
    timer = setTimeout(() => stall.abort(new DOMException(`no data for ${stallMs} ms`, 'TimeoutError')), stallMs);
  };
  wait();
  try {
    // fetch decodes a body the server sent with a Content-Encoding, where
    // curl keeps the bytes as they came: asking for none keeps the hash on
    // the file the server holds.
    const response = await fetchWithRetry(url, {
      attempts: 1,
      timeoutMs,
      signal: stall.signal,
      headers: { 'accept-encoding': 'identity' },
    });
    const hash = createHash('sha256');
    await pipeline(
      response.body ? Readable.fromWeb(response.body) : Readable.from([]),
      new Transform({
        transform(chunk, _encoding, done) {
          wait();
          hash.update(chunk);
          done(null, chunk);
        },
      }),
      createWriteStream(file),
    );
    return hash.digest('hex');
  } finally {
    clearTimeout(timer);
  }
}

// Checks `digest` against `asset`'s pin, and says what it checked.
function check(asset, digest) {
  const { pin } = asset;
  if (pin.sha256 === undefined) {
    const why = pin.ref !== undefined ? `pinned by the commit in its URL, ${pin.ref}` : 'a rolling download with no pin';
    io.stdout(`${asset.name}: SHA-256 ${digest}, not checked: ${why}\n`);
    return;
  }
  if (digest !== pin.sha256) {
    throw new ChecksumError(
      `${asset.name}: SHA-256 mismatch: expected ${pin.sha256}, got ${digest}. ` +
        'Upstream may have replaced the asset, or its version moved without its pin.',
    );
  }
  io.stdout(`${asset.name}: SHA-256 ${digest} matches its pin\n`);
}

/**
 * Downloads `asset` ({ name, url, pin }) to `file` and checks it against its
 * pin. A download that fails, stalls for `stallMs` or does not match is tried
 * again, up to `attempts` times in all, as one unit with its check; a body cut
 * short fails its attempt, as fetch refuses it. Nothing is left at `file`
 * unless every check passed: the bytes land beside it first.
 */
export async function download(asset, file, { attempts = 3, delayMs = 2_000, stallMs = 30_000, timeoutMs = 600_000 } = {}) {
  pinKey(asset.pin);
  await mkdir(path.dirname(file), { recursive: true });
  const part = `${file}.part`;
  try {
    await retry(
      async () => {
        io.stdout(`Downloading ${asset.url}\n`);
        check(asset, await fetchTo(asset.url, part, { stallMs, timeoutMs }));
      },
      {
        attempts,
        delayMs,
        label: `download ${asset.name}`,
        isRetryable: (err) => err instanceof ChecksumError || isTransient(err),
      },
    );
    await rename(part, file);
  } finally {
    await rm(part, { force: true });
  }
}

// --- Unpacking --------------------------------------------------------------------

const FORMATS = [
  ['.tar.gz', 'tar'],
  ['.tgz', 'tar'],
  ['.tar.xz', 'tar'],
  ['.tar', 'tar'],
  ['.zip', 'zip'],
];

/**
 * The tar each OS runs. Windows' is bsdtar, in System32, run by its full path:
 * a Windows runner also has Git for Windows' GNU tar, which can come first on
 * PATH and reads no zip. macOS's tar is bsdtar too, and Linux's GNU tar.
 */
export function tarProgram(platform = process.platform) {
  if (platform !== 'win32') return 'tar';
  return path.win32.join(process.env.SystemRoot || 'C:\\Windows', 'System32', 'tar.exe');
}

/**
 * Unpacks the archive `file` (.tar.gz, .tgz, .tar.xz, .tar or .zip) into
 * `dir`, with the tool each OS has: tar, which reads every one of them on
 * macOS and Windows, where it is bsdtar, and `unzip` for a zip on Linux, whose
 * GNU tar reads none. Node's built-ins read no archive format and have no xz
 * decoder.
 */
export async function unpack(file, dir, { platform = process.platform } = {}) {
  const format = FORMATS.find(([suffix]) => file.endsWith(suffix))?.[1];
  if (format === undefined) {
    throw new Error(`${path.basename(file)}: cannot unpack it; an archive is one of ${FORMATS.map(([s]) => s).join(', ')}`);
  }
  await mkdir(dir, { recursive: true });
  if (format === 'zip' && platform === 'linux') {
    await command('unzip', ['-q', '-o', file, '-d', dir]);
  } else {
    await command(tarProgram(platform), ['-x', '-f', file, '-C', dir]);
  }
}

/**
 * The one directory at the top of `dir`, where an archive that unpacks into a
 * folder of its own put it. A `__MACOSX` folder beside it, which macOS adds to
 * a zip it makes, is not counted.
 */
export async function onlyDirectory(dir) {
  const entries = (await readdir(dir, { withFileTypes: true })).filter((entry) => entry.name !== '__MACOSX');
  if (entries.length !== 1 || !entries[0].isDirectory()) {
    const found = entries.map((entry) => entry.name).join(', ') || 'nothing';
    throw new Error(`expected the archive to hold one directory at its top; it holds ${found}`);
  }
  return path.join(dir, entries[0].name);
}

// --- Placing the install ------------------------------------------------------------

/**
 * Runs `fn` with a new, empty directory beside `dir`, on the same file system,
 * so what `fn` unpacks there can be renamed into `dir`. The directory is
 * removed when `fn` ends, however it ends.
 */
export async function withWorkDir(dir, fn) {
  const parent = path.dirname(dir);
  await mkdir(parent, { recursive: true });
  const work = await mkdtemp(path.join(parent, `.${path.basename(dir)}-download-`));
  try {
    return await fn(work);
  } finally {
    await rm(work, { recursive: true, force: true });
  }
}

/**
 * Throws unless `dir` is missing, empty, or holds `file`, which an earlier
 * install put there: the directories a developer's install may replace whole,
 * so a mistyped prefix cannot take what else is there with it.
 */
export async function checkReplaceable(dir, file) {
  let entries;
  try {
    entries = await readdir(dir);
  } catch (err) {
    if (err.code === 'ENOENT') return;
    throw err;
  }
  if (entries.length > 0 && !entries.includes(file)) {
    throw new Error(`${dir} holds other files and no ${file}, so it is not an earlier install to replace; choose another directory`);
  }
}
