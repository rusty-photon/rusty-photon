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
//   it again. When that fails, the newest earlier week's download, which the
//   cache restores by restoreKey, stands in (downloadAll).
//
// The cache key carries the pin, so moving a pin misses the old cache.

import { createHash } from 'node:crypto';
import { createWriteStream, existsSync, rmSync } from 'node:fs';
import { access, chmod, constants, mkdir, mkdtemp, readdir, rename, rm, symlink, writeFile } from 'node:fs/promises';
import http from 'node:http';
import https from 'node:https';
import path from 'node:path';
import { Transform } from 'node:stream';
import { pipeline } from 'node:stream/promises';

import { command, describe, HttpError, io, isTransient, parseRetryAfter, retry, warning } from './helpers.mjs';

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
 * name and pin, where a run of assets that share a pin, such as the files of
 * one commit or a week's rolling downloads, carries it once, after the last.
 * `name` names the action and what it keeps; change it when what the action
 * puts in its cached directory changes.
 */
export function cacheKey(name, assets, { now } = {}) {
  const pins = assets.map((asset) => pinKey(asset.pin, now));
  const parts = assets.flatMap((asset, i) => (pins[i] === pins[i + 1] ? [asset.name] : [asset.name, pins[i]]));
  return ['install', name, ...parts].join('-');
}

/**
 * For `assets` that are all rolling downloads, the start of their cache key
 * that every earlier week's key shares, for actions/cache's restore-keys: it
 * restores the newest earlier week's download, which downloadAll falls back
 * on. Empty for any other assets, whose pins say what they are, so another
 * download cannot stand in for them.
 */
export function restoreKey(name, assets, { now } = {}) {
  if (!assets.every(({ pin }) => pin?.rolling === true)) return '';
  return cacheKey(name, assets, { now }).replace(/[0-9]{4}-[0-9]{2}$/, '');
}

// --- Downloading ------------------------------------------------------------------

/** A download whose SHA-256 is not its pin's. */
export class ChecksumError extends Error {
  constructor(message) {
    super(message);
    this.name = 'ChecksumError';
  }
}

// The statuses that send a GET on to the URL in their Location header.
const REDIRECTS = new Set([301, 302, 303, 307, 308]);

// As many redirects as fetch follows.
const MAX_REDIRECTS = 20;

// One GET of `target`, resolving to its response, whatever its status.
function request(target, { headers, signal }) {
  const client = { 'https:': https, 'http:': http }[target.protocol];
  if (client === undefined) return Promise.reject(new TypeError(`GET ${target.origin}: only http and https are fetched`));
  return new Promise((resolve, reject) => {
    client.get(target, { headers, signal }, resolve).on('error', reject);
  });
}

/**
 * Where a redirect from `from` to `location` goes: `location` read against
 * `from`, unless it would take an https request to plain http, which drops
 * the transport's checks.
 */
export function redirectTarget(from, location) {
  const next = new URL(location, from);
  if (from.protocol === 'https:' && next.protocol !== 'https:') {
    throw new TypeError(`GET ${from.origin}: refused a redirect from https to ${next.protocol}`);
  }
  return next;
}

// `headers` without an Authorization header, which a redirect to another
// origin does not carry on, as fetch's redirects do not: a token is for the
// origin it was given for.
function withoutAuthorization(headers) {
  return Object.fromEntries(Object.entries(headers).filter(([name]) => name.toLowerCase() !== 'authorization'));
}

/**
 * The response to a GET of `url` by Node's own HTTP client, once one answers
 * 2xx after following redirects, an Authorization header going to `url`'s
 * origin only; any other answer throws an HttpError. Not
 * fetch: Node 24's (undici 7) crashes the process, beyond any catch, when a
 * server closes the connection while the reader has paused the body
 * (nodejs/undici#5360, fixed in undici 8 only). A SourceForge mirror answers
 * `Connection: close`, and a disk slower than the network pauses the body, so
 * between them every large download risked it.
 */
export async function get(url, { headers = {}, signal } = {}) {
  let target = new URL(url);
  let sent = headers;
  for (let hops = 0; ; hops++) {
    const response = await request(target, { headers: sent, signal });
    const { statusCode: status, headers: answer } = response;
    if (REDIRECTS.has(status) && answer.location) {
      response.resume();
      if (hops === MAX_REDIRECTS) throw new TypeError(`GET ${target.origin}: more than ${MAX_REDIRECTS} redirects`);
      const next = redirectTarget(target, answer.location);
      if (next.origin !== target.origin) sent = withoutAuthorization(sent);
      target = next;
      continue;
    }
    if (status < 200 || status > 299) {
      response.resume();
      const now = Date.parse(answer.date ?? '');
      throw new HttpError('GET', target, status, response.statusMessage ?? '', {
        retryAfterMs: parseRetryAfter(answer['retry-after'] ?? null, Number.isNaN(now) ? Date.now() : now),
      });
    }
    return response;
  }
}

// Streams `url` into `file`, with `headers` beside its own, aborting when no
// data comes for `stallMs`, or, with a `minBytesPerSecond`, when a stretch of
// `stallMs` brings fewer bytes than that rate would, and resolves to the
// SHA-256 of what it wrote.
async function fetchTo(url, file, { stallMs, minBytesPerSecond, timeoutMs, headers }) {
  const stall = new AbortController();
  let timer;
  const wait = () => {
    clearTimeout(timer);
    timer = setTimeout(() => stall.abort(new DOMException(`no data for ${stallMs} ms`, 'TimeoutError')), stallMs);
  };
  wait();
  // The rate is measured over each stretch of `stallMs` in turn, from the
  // request on, so a transfer that slows part way through is caught within
  // two of them.
  let received = 0;
  const floor = (minBytesPerSecond * stallMs) / 1000;
  const slow =
    minBytesPerSecond > 0 &&
    setInterval(() => {
      if (received < floor) {
        stall.abort(new DOMException(`slower than ${minBytesPerSecond} bytes/s for ${stallMs} ms`, 'TimeoutError'));
      }
      received = 0;
    }, stallMs);
  const signal = AbortSignal.any([stall.signal, AbortSignal.timeout(timeoutMs)]);
  try {
    // Asking for no Content-Encoding keeps the hash on the file the server
    // holds.
    const response = await get(url, {
      signal,
      headers: { ...headers, 'accept-encoding': 'identity', 'user-agent': 'rusty-photon-install' },
    });
    const hash = createHash('sha256');
    await pipeline(
      response,
      new Transform({
        transform(chunk, _encoding, done) {
          wait();
          received += chunk.length;
          hash.update(chunk);
          done(null, chunk);
        },
      }),
      createWriteStream(file),
    );
    return hash.digest('hex');
  } catch (err) {
    // What the request was aborted for, a stall or the time limit, rather
    // than Node's AbortError; and a failure of the connection in the shape
    // fetch gives one, a TypeError whose cause carries the code, which
    // isTransient reads.
    if (signal.aborted) throw signal.reason;
    if (err instanceof HttpError || err instanceof TypeError || typeof err?.code !== 'string') throw err;
    throw new TypeError(`download failed: ${err.message}`, { cause: err });
  } finally {
    clearTimeout(timer);
    clearInterval(slow);
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
 * pin. A download that fails, stalls for `stallMs`, runs slower than
 * `minBytesPerSecond` over `stallMs` (0, the default, sets no floor), takes
 * longer than `timeoutMs` or does not match is tried again, up to `attempts`
 * times in all, as one unit with its check; a body cut short of the length
 * its server announced fails its attempt. With `retryForMs`, no attempt starts once
 * that long has passed since the first began, though one under way runs on,
 * as curl's --retry-max-time works. A failure that a later attempt would
 * meet again, such as a 404 or a bad certificate, is not tried again, unless
 * `retryAll` says the server is one of a pool of mirrors, any of which can be
 * broken in any way, where the next attempt likely reaches another: then
 * every failure is, as curl's --retry-all-errors did. `headers` go with each
 * request, such as the Authorization a private mirror asks for. Nothing is
 * left at `file` unless every check passed: the bytes land beside it first.
 */
export async function download(
  asset,
  file,
  {
    attempts = 3,
    delayMs = 2_000,
    stallMs = 30_000,
    minBytesPerSecond = 0,
    timeoutMs = 600_000,
    retryForMs,
    retryAll = false,
    headers = {},
  } = {},
) {
  pinKey(asset.pin);
  await mkdir(path.dirname(file), { recursive: true });
  const part = `${file}.part`;
  try {
    await retry(
      async () => {
        io.stdout(`Downloading ${asset.url}\n`);
        check(asset, await fetchTo(asset.url, part, { stallMs, minBytesPerSecond, timeoutMs, headers }));
      },
      {
        attempts,
        delayMs,
        label: `download ${asset.name}`,
        isRetryable: (err) => retryAll || err instanceof ChecksumError || isTransient(err),
        // retry starts no attempt once this aborts; the fetch under way does
        // not see it.
        signal: retryForMs === undefined ? undefined : AbortSignal.timeout(retryForMs),
      },
    );
    await rename(part, file);
  } finally {
    await rm(part, { force: true });
  }
}

/**
 * Downloads each of `assets` into `dir`, as its name, with `download`'s
 * options, and resolves to true; or, when `cached` says the cache restored
 * them there and `dir` holds every one, downloads nothing and resolves to
 * false. The downloads land beside `dir` and move in once every one has
 * passed its check, so a failure leaves what `dir` held as it was.
 *
 * That is the fallback of a rolling download. When every asset is one, and
 * `dir` holds each from an earlier week's download, which the cache restored
 * by restoreKey (`restored` names the key it matched), a failed download warns
 * and resolves to false, leaving those to install: an earlier week's SDK
 * builds, where a vendor's failing CDN would otherwise fail the job.
 */
export async function downloadAll(assets, dir, { cached = false, restored = '', download: options } = {}) {
  const missing = assets.filter(({ name }) => !existsSync(path.join(dir, name))).map(({ name }) => name);
  if (cached && missing.length === 0) {
    io.stdout(`${assets.map(({ name }) => name).join(', ')} restored from the cache into ${dir}\n`);
    return false;
  }
  if (cached) warning(`The cache restored ${dir} without ${missing.join(' and ')}; downloading again`);
  try {
    await withWorkDir(dir, async (work) => {
      for (const asset of assets) await download(asset, path.join(work, asset.name), options);
      await mkdir(dir, { recursive: true });
      for (const { name } of assets) await rename(path.join(work, name), path.join(dir, name));
    });
    return true;
  } catch (err) {
    if (missing.length > 0 || !assets.every(({ pin }) => pin.rolling === true)) throw err;
    warning(`${describe(err)}. Installing the earlier download the cache restored instead${restored ? `, ${restored}` : ''}`);
    return false;
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

/** Windows PowerShell, by its full path, which every Windows has. */
export function powershellProgram() {
  return path.win32.join(process.env.SystemRoot || 'C:\\Windows', 'System32', 'WindowsPowerShell', 'v1.0', 'powershell.exe');
}

// A PowerShell string of `text`, quoted so nothing in it is read: in single
// quotes, each single quote doubled, curly ones too, which PowerShell also
// takes for one.
function psLiteral(text) {
  return `'${text.replace(/['\u2018\u2019\u201A\u201B]/g, (quote) => quote + quote)}'`;
}

/**
 * Unpacks each zip `[file, dir]` in `zips` into its directory, for a vendor's
 * zip that may hold a name Windows' code page cannot: Windows' tar.exe
 * (bsdtar 3.8.8) skips such an entry, then fails, and ZWO's EFW and EAF SDKs
 * each hold one ("EFW SDK快速入门.pdf"). So on Windows PowerShell's
 * Expand-Archive unpacks them, which reads the names as Unicode: all of them
 * in one PowerShell, which takes a second or more to start. Elsewhere unpack
 * does.
 */
export async function unpackZips(zips, { platform = process.platform, run = command } = {}) {
  if (platform !== 'win32') {
    for (const [file, dir] of zips) await unpack(file, dir);
    return;
  }
  if (zips.length === 0) return;
  const expand = zips.map(([file, dir]) => `Expand-Archive -LiteralPath ${psLiteral(file)} -DestinationPath ${psLiteral(dir)} -Force`);
  const script =
    "$ErrorActionPreference = 'Stop'; $ProgressPreference = 'SilentlyContinue'; " +
    `try { ${expand.join('; ')} } catch { [Console]::Error.WriteLine($_.Exception.Message); exit 1 }`;
  await run(powershellProgram(), ['-NoProfile', '-NonInteractive', '-EncodedCommand', Buffer.from(script, 'utf16le').toString('base64')]);
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
 * removed when `fn` ends, however it ends, an interrupt (Ctrl-C) or a SIGTERM
 * included.
 */
export async function withWorkDir(dir, fn) {
  const parent = path.dirname(dir);
  await mkdir(parent, { recursive: true });
  const work = await mkdtemp(path.join(parent, `.${path.basename(dir)}-download-`));
  // A signal ends the process without running `finally`: remove the
  // directory, then raise the signal again, which now ends the process as it
  // would have.
  const interrupted = (signal) => {
    rmSync(work, { recursive: true, force: true });
    process.kill(process.pid, signal);
  };
  process.once('SIGINT', interrupted);
  process.once('SIGTERM', interrupted);
  try {
    return await fn(work);
  } finally {
    process.off('SIGINT', interrupted);
    process.off('SIGTERM', interrupted);
    await rm(work, { recursive: true, force: true });
  }
}

/** The file a developer's install leaves in a directory it installed whole. */
export const INSTALLED = '.installed-by-rusty-photon';

/** Marks `dir` as one a developer's install of `what` put there whole. */
export async function markInstalled(dir, what) {
  await writeFile(path.join(dir, INSTALLED), `${what}\n`);
}

/**
 * Throws unless `dir` is missing, empty, or marked by an earlier install
 * (markInstalled): the directories a developer's install may replace whole,
 * so a mistyped prefix cannot take what else is there with it.
 */
export async function checkReplaceable(dir) {
  let entries;
  try {
    entries = await readdir(dir);
  } catch (err) {
    if (err.code === 'ENOENT') return;
    throw err;
  }
  if (entries.length > 0 && !entries.includes(INSTALLED)) {
    throw new Error(
      `${dir} holds files and no ${INSTALLED}, so no earlier install of ours made it, and it is not ours to replace; ` +
        'choose another directory',
    );
  }
}

/** Lets `binary` in `dir` run, unless it is a Windows .exe, which needs no mode for that. */
export async function makeRunnable(dir, binary) {
  if (!binary.endsWith('.exe')) await chmod(path.join(dir, binary), 0o755);
}

// --- Libraries a build links -------------------------------------------------------

// Where a runtime library is when the loader's cache cannot say.
const RUNTIME_DIRS = ['/usr/lib/aarch64-linux-gnu', '/lib/aarch64-linux-gnu', '/usr/lib/x86_64-linux-gnu', '/usr/lib', '/lib'];

/**
 * Where this machine has the runtime library `soname`: the first entry for it
 * in the loader's cache (`ldconfig -p`), or else in `dirs`.
 */
export async function findRuntime(soname, { run = command, dirs = RUNTIME_DIRS } = {}) {
  try {
    for (const line of (await run('ldconfig', ['-p'])).split('\n')) {
      const fields = line.trim().split(/\s+/);
      if (fields[0] === soname) return fields.at(-1);
    }
  } catch {
    // No ldconfig on PATH, or no cache: the directories below are the rest.
  }
  return dirs.map((dir) => path.join(dir, soname)).find((file) => existsSync(file));
}

/**
 * Whether the loader's cache lists each of `files` in `lib`, where a binary
 * that asks for it by that name finds it with no LD_LIBRARY_PATH.
 */
export async function loaderFinds(lib, files, { run = command } = {}) {
  for (const file of files) {
    if ((await findRuntime(file, { run, dirs: [] })) !== path.join(lib, file)) return false;
  }
  return true;
}

/**
 * Links each library `[soname, linker, pkg]` of `libraries` into `lib` under
 * its linker's name, `-l`'s, to the runtime library `find` finds, for a
 * runner without sudo, which cannot install the -dev package that has the
 * link. Throws, naming `pkg`, when the runtime library is not there.
 */
export async function linkRuntime(lib, libraries, { run = command, find = findRuntime } = {}) {
  for (const [soname, linker, pkg] of libraries) {
    const found = await find(soname, { run });
    if (found === undefined) {
      throw new Error(
        `the system library ${soname} is not on this machine. A runner without sudo links against the host's, ` +
          'which scripts/setup-pi-runner.sh §1 installs once (docs/skills/raspberry-pi-runner.md). ' +
          `Install the package that has it, then run the job again: sudo apt-get install -y ${pkg}`,
      );
    }
    await rm(path.join(lib, linker), { force: true });
    await symlink(found, path.join(lib, linker));
    io.stdout(`${linker} -> ${found}\n`);
  }
}

/** Whether this user can write `dir`, or the directory it would be made in. */
export async function canWrite(dir) {
  for (let at = dir; ; at = path.dirname(at)) {
    try {
      await access(at, constants.W_OK);
      return true;
    } catch (err) {
      if (err.code !== 'ENOENT' || path.dirname(at) === at) return false;
    }
  }
}
