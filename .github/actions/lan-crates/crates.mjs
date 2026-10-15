// The crates.io packages Cargo.lock pins, read from the LAN bazel-remote's
// content-addressed store instead of crates.io (plan item J14,
// docs/plans/ci-shell-to-code.md).
//
// bazel-remote keeps a blob under its SHA-256, and that is the checksum
// Cargo.lock records for a crate, so a crate's address in the store is its
// pin. Every crate is checked against Cargo.lock whoever served it, then
// unpacked into a Cargo directory source that replaces crates.io, so Cargo
// downloads nothing and reads no index. A crate the store lacks comes from
// crates.io and, given the write credential, is uploaded for the next run.
// A store that is down, slow, or serves a wrong blob is a miss, never a
// failed step.

import { createHash } from 'node:crypto';
import { mkdir, readFile, rm, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { gunzipSync } from 'node:zlib';

import { describe, fetchWithRetry, group, HttpError, io, retry, setSecret, warning } from '../lib/helpers.mjs';
import { untar } from './untar.mjs';

/** How Cargo.lock names crates.io, in lockfile versions 3 and 4. */
export const CRATES_IO = 'registry+https://github.com/rust-lang/crates.io-index';

/** The `dl` of crates.io's index (https://index.crates.io/config.json). */
export const CRATES_IO_DOWNLOADS = 'https://static.crates.io/crates';

/** The name Cargo's configuration gives the directory source. */
export const SOURCE_NAME = 'lan-crates';

// The most Cargo itself unpacks from one crate, since its fix for
// CVE-2022-36114.
const MAX_UNPACKED_BYTES = 512 * 1024 * 1024;

// crates.io's rules for a name, and the shape of a SemVer version (with
// its pre-release and build parts); both end up in a path and a URL.
const NAME = /^[A-Za-z0-9_-]{1,64}$/;
const VERSION = /^\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.+-]+)?$/;
const CHECKSUM = /^[0-9a-f]{64}$/;

/**
 * The crates.io packages in the Cargo.lock text `text`: `{ name, version,
 * checksum }`. Packages from git or a path have no checksum and are left to
 * Cargo. Reads the format Cargo writes, lockfile versions 3 and 4, and throws
 * on anything else, rather than guess.
 */
export function parseLock(text) {
  let version;
  let inTable = false;
  let inArray = false;
  let current;
  const packages = [];
  for (const [index, raw] of text.split(/\r?\n/).entries()) {
    const line = raw.trim();
    if (inArray) {
      if (line === ']') inArray = false;
      continue;
    }
    if (line === '' || line.startsWith('#')) continue;
    if (line.startsWith('[')) {
      inTable = true;
      current = line === '[[package]]' ? {} : undefined;
      if (current) packages.push(current);
      continue;
    }
    const pair = /^([A-Za-z0-9_-]+) = (.*)$/.exec(line);
    if (!pair) throw new Error(`Cargo.lock line ${index + 1} is not one this action reads: ${JSON.stringify(raw)}`);
    const [, key, value] = pair;
    if (value === '[') {
      inArray = true;
      continue;
    }
    if (!inTable && key === 'version') {
      version = value;
      continue;
    }
    if (current === undefined || value.startsWith('[')) continue;
    if (!/^"[^"\\]*"$/.test(value)) {
      throw new Error(`Cargo.lock line ${index + 1}: ${key} is not a plain string: ${JSON.stringify(raw)}`);
    }
    current[key] = value.slice(1, -1);
  }
  if (version !== '3' && version !== '4') {
    throw new Error(`Cargo.lock version ${version ?? '(none)'}: this action reads versions 3 and 4`);
  }
  return packages
    .filter((p) => p.source === CRATES_IO)
    .map(({ name, version: v, checksum }) => {
      const id = `${name} ${v}`;
      if (!NAME.test(name ?? '')) throw new Error(`Cargo.lock: ${JSON.stringify(name)} is not a crates.io name`);
      if (!VERSION.test(v ?? '')) throw new Error(`Cargo.lock: ${name} has version ${JSON.stringify(v)}`);
      if (!CHECKSUM.test(checksum ?? '')) throw new Error(`Cargo.lock: ${id} has no SHA-256 checksum`);
      return { name, version: v, checksum };
    });
}

function sha256Hex(bytes) {
  return createHash('sha256').update(bytes).digest('hex');
}

// The crate's bytes from the store, or undefined when it holds none (404).
// Any other failure throws. One attempt: a store that fails is treated as
// down for the rest of the run, and a retry would only slow that down.
async function readStore(storeUrl, checksum, timeoutMs) {
  try {
    const response = await fetchWithRetry(`${storeUrl}/cas/${checksum}`, { attempts: 1, timeoutMs });
    return Buffer.from(await response.arrayBuffer());
  } catch (err) {
    if (err instanceof HttpError && err.status === 404) return undefined;
    throw err;
  }
}

/** A crate whose bytes do not hash to the checksum Cargo.lock pins. */
export class ChecksumError extends Error {
  constructor(crate, got) {
    super(`${crate.name} ${crate.version}: SHA-256 ${got}, but Cargo.lock pins ${crate.checksum}`);
    this.name = 'ChecksumError';
  }
}

// The crate from crates.io, as Cargo would fetch it, checked against
// Cargo.lock. The download and the check are retried together, but a
// checksum that does not match is not retried: crates.io serves one file per
// version.
async function download(downloads, crate, timeoutMs, delayMs) {
  return retry(
    async () => {
      const response = await fetchWithRetry(`${downloads}/${crate.name}/${crate.version}/download`, {
        attempts: 1,
        timeoutMs,
      });
      const bytes = Buffer.from(await response.arrayBuffer());
      const got = sha256Hex(bytes);
      if (got !== crate.checksum) throw new ChecksumError(crate, got);
      return bytes;
    },
    { label: `download of ${crate.name} ${crate.version}`, delayMs },
  );
}

// A PUT is idempotent, so fetchWithRetry repeats a transient failure.
// bazel-remote refuses a blob whose SHA-256 is not its key.
async function upload(storeUrl, crate, bytes, writeAuth, timeoutMs, delayMs) {
  const response = await fetchWithRetry(`${storeUrl}/cas/${crate.checksum}`, {
    method: 'PUT',
    body: bytes,
    headers: { authorization: writeAuth, 'content-type': 'application/octet-stream' },
    timeoutMs,
    delayMs,
  });
  await response.arrayBuffer();
}

// The path of tar entry `entryPath` inside the crate's directory, as
// segments, or throws: every entry of a crate sits under `<name>-<version>/`
// (Cargo refuses a crate that has one elsewhere), and none may climb out of
// it.
function segmentsUnder(root, entryPath) {
  const trimmed = entryPath.replace(/\/+$/, '');
  if (trimmed === root) return [];
  if (!trimmed.startsWith(`${root}/`)) throw new Error(`${JSON.stringify(entryPath)} is not under ${root}/`);
  const segments = trimmed.slice(root.length + 1).split('/');
  if (segments.some((s) => s === '' || s === '.' || s === '..' || s.includes('\\'))) {
    throw new Error(`${JSON.stringify(entryPath)} is not a plain relative path`);
  }
  return segments;
}

/**
 * Unpacks the `.crate` file `bytes` into `<dir>/<name>-<version>/`, with the
 * `.cargo-checksum.json` a directory source needs. Its `package` is the
 * checksum Cargo compares with Cargo.lock; its `files` is empty, so Cargo
 * checks no file against it: the crate was checked as a whole before this.
 */
export async function unpack(bytes, crate, dir) {
  const root = `${crate.name}-${crate.version}`;
  const entries = untar(gunzipSync(bytes, { maxOutputLength: MAX_UNPACKED_BYTES }));
  const base = path.join(dir, root);
  await mkdir(base, { recursive: true });
  for (const entry of entries) {
    const target = path.join(base, ...segmentsUnder(root, entry.path));
    if (entry.type === 'directory') {
      await mkdir(target, { recursive: true });
      continue;
    }
    await mkdir(path.dirname(target), { recursive: true });
    // Executable stays executable: some build scripts run a script the
    // crate ships.
    await writeFile(target, entry.data, { mode: entry.mode & 0o111 ? 0o755 : 0o644 });
  }
  await writeFile(path.join(base, '.cargo-checksum.json'), JSON.stringify({ files: {}, package: crate.checksum }));
}

// Runs `fn` on every item, `limit` at a time. `fn` must not throw.
async function eachLimited(items, limit, fn) {
  let next = 0;
  const worker = async () => {
    while (next < items.length) await fn(items[next++]);
  };
  await Promise.all(Array.from({ length: Math.min(limit, items.length) }, worker));
}

/**
 * Fills `dir` with every crate in `crates`, from the store at `storeUrl`
 * where it holds the crate, from `downloads` (crates.io) where it does not.
 * With `writeAuth`, a crate fetched from crates.io is uploaded to the store.
 * The first failure of the store other than a 404 marks it down, and no
 * later crate asks it. Returns what happened, for the caller to report:
 * `failures` lists the crates that could not be had at all.
 */
export async function vendorCrates({
  crates,
  storeUrl,
  writeAuth = '',
  dir,
  downloads = CRATES_IO_DOWNLOADS,
  concurrency = 16,
  storeTimeoutMs = 10_000,
  downloadTimeoutMs = 60_000,
  delayMs = 1000,
}) {
  const result = { fromStore: 0, downloaded: [], uploaded: 0, uploadFailures: [], wrongInStore: [], failures: [] };
  await eachLimited(crates, concurrency, async (crate) => {
    try {
      let bytes;
      if (result.storeDown === undefined) {
        try {
          const stored = await readStore(storeUrl, crate.checksum, storeTimeoutMs);
          if (stored !== undefined && sha256Hex(stored) === crate.checksum) {
            bytes = stored;
            result.fromStore++;
          } else if (stored !== undefined) {
            result.wrongInStore.push(crate);
          }
        } catch (err) {
          result.storeDown ??= describe(err);
        }
      }
      if (bytes === undefined) {
        bytes = await download(downloads, crate, downloadTimeoutMs, delayMs);
        result.downloaded.push(crate);
        if (writeAuth !== '' && result.storeDown === undefined) {
          try {
            await upload(storeUrl, crate, bytes, writeAuth, storeTimeoutMs, delayMs);
            result.uploaded++;
          } catch (err) {
            result.uploadFailures.push({ crate, reason: describe(err) });
          }
        }
      }
      await unpack(bytes, crate, dir);
    } catch (err) {
      result.failures.push({ crate, reason: describe(err) });
    }
  });
  return result;
}

/**
 * Points Cargo at the directory source in `dir`, in `<cargoHome>/config.toml`.
 * Returns false, changing nothing, when Cargo's configuration there already
 * names a source, or is in the older file `config`, which Cargo would read
 * instead.
 */
export async function replaceCratesIo(cargoHome, dir) {
  // A TOML literal string takes any character but these.
  if (/['\r\n]/.test(dir)) throw new Error(`${JSON.stringify(dir)} cannot be written as a TOML literal string`);
  const file = path.join(cargoHome, 'config.toml');
  const read = async (name) => {
    try {
      return await readFile(path.join(cargoHome, name), 'utf8');
    } catch (err) {
      if (err.code === 'ENOENT') return undefined;
      throw err;
    }
  };
  if ((await read('config')) !== undefined) return false;
  const existing = (await read('config.toml')) ?? '';
  if (/^\s*\[\s*source\s*[.\]]/m.test(existing)) return false;
  const separator = existing === '' || existing.endsWith('\n') ? '' : '\n';
  await mkdir(cargoHome, { recursive: true });
  await writeFile(
    file,
    `${existing}${separator}` +
      '# Written by the lan-crates action: Cargo reads the crates.io packages\n' +
      '# from a directory it filled and checked against Cargo.lock.\n' +
      '[source.crates-io]\n' +
      `replace-with = "${SOURCE_NAME}"\n\n` +
      `[source.${SOURCE_NAME}]\n` +
      `directory = '${dir}'\n`,
  );
  return true;
}

function list(crates) {
  return crates.map((c) => `${c.name} ${c.version}`).join(', ');
}

/**
 * The action: with a store URL, fills `dir` from Cargo.lock at `lockPath` and
 * points Cargo at it; without one (a hosted runner, a fork), leaves Cargo to
 * download its crates itself. Only an error in the action or in Cargo.lock
 * throws; anything the store or crates.io does is reported and leaves Cargo
 * to download what it needs.
 */
export async function lanCrates({ storeUrl, writeAuth = '', lockPath, dir, cargoHome, ...options }) {
  if (!storeUrl) {
    io.stdout('No LAN cache on this runner: Cargo downloads its crates itself.\n');
    return;
  }
  if (writeAuth !== '') setSecret(writeAuth);
  // The store's address is the operator's LAN, and fetch's errors name its
  // host and port ("connect ECONNREFUSED <host>:<port>"), not the URL.
  const base = storeUrl.replace(/\/+$/, '');
  setSecret(base);
  try {
    const { host, hostname } = new URL(base);
    setSecret(host);
    setSecret(hostname);
  } catch {
    throw new Error('RP_LAN_CACHE_URL does not parse as a URL (it is not shown, as it is the LAN address)');
  }
  const crates = parseLock(await readFile(lockPath, 'utf8'));
  await rm(dir, { recursive: true, force: true });
  await mkdir(dir, { recursive: true });

  const started = Date.now();
  const result = await vendorCrates({ crates, storeUrl: base, writeAuth, dir, ...options });
  const seconds = ((Date.now() - started) / 1000).toFixed(1);
  const uploads = writeAuth === '' ? '' : `, ${result.uploaded} of them uploaded to the LAN cache`;
  io.stdout(
    `${crates.length} crates from Cargo.lock in ${seconds} s: ${result.fromStore} from the LAN cache, ` +
      `${result.downloaded.length} from crates.io${uploads}.\n`,
  );
  if (result.downloaded.length > 0) {
    await group('Crates from crates.io', async () => {
      for (const c of result.downloaded) io.stdout(`${c.name} ${c.version}\n`);
    });
  }
  if (result.storeDown !== undefined) {
    warning(`The LAN cache failed (${result.storeDown}); the crates it had not served came from crates.io.`);
  }
  if (result.wrongInStore.length > 0) {
    warning(`The LAN cache served bytes that do not match Cargo.lock for ${list(result.wrongInStore)}; those came from crates.io.`);
  }
  if (result.uploadFailures.length > 0) {
    const first = result.uploadFailures[0];
    warning(
      `${result.uploadFailures.length} crates were not uploaded to the LAN cache ` +
        `(${first.crate.name} ${first.crate.version}: ${first.reason}).`,
    );
  }
  if (result.failures.length > 0) {
    const first = result.failures[0];
    warning(
      `${result.failures.length} crates could not be had (${first.crate.name} ${first.crate.version}: ${first.reason}); ` +
        'Cargo downloads its crates itself.',
    );
    return;
  }
  if (!(await replaceCratesIo(cargoHome, dir))) {
    warning(`${cargoHome} already configures Cargo's sources; Cargo downloads its crates itself.`);
  }
}
