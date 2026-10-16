// The pool `stable / clippy`'s compiled dependencies, kept in the LAN
// bazel-remote (plan item J1, docs/plans/ci-shell-to-code.md).
//
// A push to the default branch builds cold, and its post step packs target/,
// without the workspace's own crates, and $CARGO_HOME/git (the git
// dependencies), uploads each archive to the content-addressed store under its
// SHA-256, and points an action-cache entry at them. A pull request reads the
// pointer, downloads the archives, checks each against its SHA-256 and unpacks
// them before Cargo runs. A writer never restores: the archives hold
// build-script and proc-macro binaries the job runs, and the pointer is read
// over plain HTTP, so a tampered restore must never run where the write
// credential is, or be saved back for every later pull request. A cache that
// is missing, down, slow or wrong is a cold build, never a failed step.

import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import { createReadStream, createWriteStream, existsSync, openAsBlob } from 'node:fs';
import { mkdir, readdir, readFile, rm, stat } from 'node:fs/promises';
import { availableParallelism } from 'node:os';
import path from 'node:path';
import { Readable, Transform } from 'node:stream';
import { pipeline } from 'node:stream/promises';

import {
  describe,
  fetchWithRetry,
  getState,
  io,
  maskUrl,
  requestFailure,
  saveState,
  setSecret,
  sha256,
  warning,
} from '../lib/helpers.mjs';

/** Part of every key: a change to what the archives hold changes it. */
export const SCHEMA = 'cargo-cache 1';

// What a pointer may name: one archive per root, compressed or not.
const ARCHIVE = /^(target|git)\.tar(\.zst)?$/;
const SHA256 = /^[0-9a-f]{64}$/;

// --- Processes ------------------------------------------------------------------

/**
 * Runs `cmd` with `args` and resolves to what it prints, or rejects with the
 * last line of its stderr when it exits other than 0, and with the spawn
 * error (ENOENT) when it cannot start.
 */
export function command(cmd, args, { cwd, env } = {}) {
  return new Promise((resolve, reject) => {
    const child = spawn(cmd, args, { cwd, env: env ? { ...process.env, ...env } : process.env, stdio: ['ignore', 'pipe', 'pipe'] });
    const out = [];
    const err = [];
    child.stdout.on('data', (chunk) => out.push(chunk));
    child.stderr.on('data', (chunk) => err.push(chunk));
    child.on('error', reject);
    child.on('close', (code, signal) => {
      if (code === 0) {
        resolve(Buffer.concat(out).toString('utf8'));
        return;
      }
      const last = Buffer.concat(err).toString('utf8').trim().split('\n').pop();
      reject(new Error(`${cmd} ${code === null ? `was stopped by ${signal}` : `exited with ${code}`}${last ? `: ${last}` : ''}`));
    });
  });
}

/** Whether `cmd --version` runs: a tool the runner may lack. */
export async function available(cmd) {
  try {
    await command(cmd, ['--version']);
    return true;
  } catch {
    return false;
  }
}

// --- The key ------------------------------------------------------------------------

/**
 * The action-cache key of one cache, as bazel-remote takes it: a SHA-256 in
 * hex. Each part changes what a restore would hold or where it must go: the
 * job's own Cargo commands, the toolchain (`rustc -vV`), the runner's system
 * packages, and the paths Cargo records (the workspace, CARGO_HOME, and
 * RUNNER_TEMP, under which lan-crates unpacks the crates).
 */
export function cacheKey({ job, rustc, system, workspace, cargoHome, runnerTemp }) {
  const text = [
    SCHEMA,
    `job ${job}`,
    `rustc ${rustc.trim()}`,
    `system ${system}`,
    `workspace ${workspace}`,
    `cargo-home ${cargoHome}`,
    `runner-temp ${runnerTemp}`,
  ].join('\n');
  return createHash('sha256').update(text).digest('hex');
}

/**
 * What the key reads from the runner: the toolchain, and a hash of the
 * installed system packages. Build scripts probe the system (pkg-config,
 * bindgen's libclang) and Cargo does not rerun them when it changes, so a
 * pool template rebuilt with other packages starts a new cache.
 */
export async function identify() {
  const rustc = await command('rustc', ['-vV']);
  const packages = await command('dpkg-query', ['-W', '-f', '${binary:Package} ${Version}\n']);
  const system = createHash('sha256')
    .update(packages.split('\n').filter(Boolean).sort().join('\n'))
    .digest('hex');
  return { rustc, system };
}

// --- The pointer --------------------------------------------------------------------

/**
 * The action-cache entry pointing at `archives`, as bazel-remote reads an
 * ActionResult in JSON. Its `worker` keeps bazel-remote from recording the
 * writer's address there, which it does when the field is empty and which
 * anyone could read back. `note` goes in `stdoutRaw`.
 */
export function pointerBody(archives, note) {
  return JSON.stringify({
    outputFiles: archives.map((a) => ({ path: a.file, digest: { hash: a.hash, sizeBytes: String(a.size) } })),
    stdoutRaw: Buffer.from(JSON.stringify(note)).toString('base64'),
    executionMetadata: { worker: 'cargo-cache' },
  });
}

/**
 * The archives a pointer names, `{ name, file, compressed, hash, size }`, and
 * the commit and run that saved them, where the pointer says. Throws on a
 * pointer this action did not write.
 */
export function parsePointer(value) {
  const files = value?.outputFiles;
  if (!Array.isArray(files) || files.length === 0) throw new Error('it names no archives');
  const seen = new Set();
  const archives = files.map((f) => {
    const match = ARCHIVE.exec(f?.path ?? '');
    if (!match) throw new Error(`${JSON.stringify(f?.path)} is not an archive this action writes`);
    const [, name, zst] = match;
    if (seen.has(name)) throw new Error(`it names ${name} twice`);
    seen.add(name);
    const hash = f.digest?.hash ?? '';
    const size = String(f.digest?.sizeBytes ?? '');
    if (!SHA256.test(hash)) throw new Error(`${f.path} has no SHA-256`);
    if (!/^[1-9]\d{0,14}$/.test(size)) throw new Error(`${f.path} has size ${JSON.stringify(size)}`);
    return { name, file: f.path, compressed: zst !== undefined, hash, size: Number(size) };
  });
  let note = {};
  try {
    note = JSON.parse(Buffer.from(value.stdoutRaw ?? '', 'base64').toString('utf8'));
  } catch {
    // No note, or one this action did not write: nothing to print.
  }
  return {
    archives,
    commit: /^[0-9a-f]{40}$/.test(note?.commit ?? '') ? note.commit : undefined,
    run: /^\d{1,20}$/.test(note?.run ?? '') ? note.run : undefined,
  };
}

// The pointer under `key`, as the JSON text bazel-remote turns it into, or
// undefined when the store has none or has lost an archive it names (it
// answers 404 to both). Any other failure throws.
async function readPointer(base, key, timeoutMs) {
  try {
    const response = await fetchWithRetry(`${base}/ac/${key}`, {
      headers: { accept: 'application/json' },
      attempts: 1,
      timeoutMs,
    });
    return await response.text();
  } catch (err) {
    if (err?.status === 404) return undefined;
    throw err;
  }
}

// --- Archives -----------------------------------------------------------------------

/** An archive whose bytes are not the ones its pointer names. */
export class IntegrityError extends Error {
  constructor(archive, hash, size) {
    super(`${archive.file}: ${size} bytes with SHA-256 ${hash}, but the pointer names ${archive.size} bytes with ${archive.hash}`);
    this.name = 'IntegrityError';
  }
}

// Downloads `archive` to `file`, hashing it on the way, and throws an
// IntegrityError unless it is the one the pointer names.
async function download(base, archive, file, timeoutMs) {
  const response = await fetchWithRetry(`${base}/cas/${archive.hash}`, { attempts: 1, timeoutMs });
  const hash = createHash('sha256');
  let size = 0;
  const count = new Transform({
    transform(chunk, _encoding, done) {
      hash.update(chunk);
      size += chunk.length;
      done(null, chunk);
    },
  });
  await pipeline(Readable.fromWeb(response.body), count, createWriteStream(file));
  const got = hash.digest('hex');
  if (got !== archive.hash || size !== archive.size) throw new IntegrityError(archive, got, size);
}

/** Packs `<root>/<name>` into `file`, with zstd on all cores when `compress`. */
export async function pack(root, name, file, compress) {
  const env = { ZSTD_NBTHREADS: String(availableParallelism()) };
  await command('tar', ['-c', ...(compress ? ['--zstd'] : []), '-f', file, '-C', root, '--', name], { env });
}

/**
 * Unpacks `<name>` from `file` into `root`, and nothing else the archive may
 * hold. Files keep their mtimes: Cargo compares them with the sources'.
 */
export async function unpack(file, root, name, compressed) {
  await command('tar', ['-x', ...(compressed ? ['--zstd'] : []), '-f', file, '-C', root, '--', name]);
}

// --- The workspace's own outputs ----------------------------------------------------

function escape(name) {
  return name.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

/**
 * The workspace's own package and target names, from `cargo metadata`, which
 * the save leaves out of the archive.
 */
export async function workspaceNames(cwd) {
  const metadata = JSON.parse(await command('cargo', ['metadata', '--no-deps', '--format-version', '1', '--offline'], { cwd }));
  const packages = new Set();
  const targets = new Set();
  for (const p of metadata.packages) {
    packages.add(p.name);
    for (const t of p.targets) targets.add(t.name);
  }
  return { packages: [...packages], targets: [...targets] };
}

async function removeMatching(dir, pattern) {
  let names;
  try {
    names = await readdir(dir);
  } catch (err) {
    if (err.code === 'ENOENT') return;
    throw err;
  }
  await Promise.all(names.filter((n) => pattern.test(n)).map((n) => rm(path.join(dir, n), { recursive: true, force: true })));
}

/**
 * Removes the workspace's own outputs from the debug profile of `targetDir`.
 * Every run lints those crates anyway, and a restored fingerprint of one
 * could let Cargo skip linting it. Cargo names a unit's fingerprint and
 * build-script directories after its package, and its compiled files after
 * its target, with '-' as '_' and `lib` before a library, each followed by a
 * 16-digit hash. Incremental state and examples belong to the workspace
 * alone.
 */
export async function removeWorkspaceOutputs(targetDir, { packages, targets }) {
  const profile = path.join(targetDir, 'debug');
  const byPackage = new RegExp(`^(?:${packages.map(escape).join('|')})-[0-9a-f]{16}$`);
  const crateNames = targets.map((t) => escape(t.replace(/-/g, '_')));
  const byTarget = new RegExp(`^(?:lib)?(?:${crateNames.join('|')})-[0-9a-f]{16}(?:\\..*)?$`);
  await removeMatching(path.join(profile, '.fingerprint'), byPackage);
  await removeMatching(path.join(profile, 'build'), byPackage);
  await removeMatching(path.join(profile, 'deps'), byTarget);
  await rm(path.join(profile, 'incremental'), { recursive: true, force: true });
  await rm(path.join(profile, 'examples'), { recursive: true, force: true });
}

// --- Restore ------------------------------------------------------------------------

function mib(bytes) {
  return `${(bytes / 1048576).toFixed(0)} MiB`;
}

function seconds(since) {
  return `${((Date.now() - since) / 1000).toFixed(1)} s`;
}

/**
 * Restores the cache under `key` from the store at `base`: target/ into
 * `workspace`, git/ into `cargoHome`. Anything the store does, a pointer this
 * action did not write, an archive that does not match its pointer or a root
 * that already exists is a cold build, said in the log.
 */
export async function restore({ base, key, workspace, cargoHome, tempDir, pointerTimeoutMs = 10_000, archiveTimeoutMs = 300_000 }) {
  const started = Date.now();
  let text;
  try {
    text = await readPointer(base, key, pointerTimeoutMs);
  } catch (err) {
    warning(`The LAN cache failed (${requestFailure(err)}); the build runs cold.`);
    return;
  }
  if (text === undefined) {
    io.stdout('The LAN cache holds no build cache for this toolchain and runner yet: the build runs cold.\n');
    return;
  }
  let pointer;
  try {
    pointer = parsePointer(JSON.parse(text));
  } catch (err) {
    warning(`The build cache's pointer is not one this action wrote (${err.message}); the build runs cold.`);
    return;
  }
  const roots = { target: workspace, git: cargoHome };
  const present = pointer.archives.filter((a) => existsSync(path.join(roots[a.name], a.name)));
  if (present.length > 0) {
    io.stdout(`${path.join(roots[present[0].name], present[0].name)} already exists: the build cache is not restored.\n`);
    return;
  }
  if (pointer.archives.some((a) => a.compressed) && !(await available('zstd'))) {
    warning('The build cache is compressed with zstd, which this runner lacks; the build runs cold.');
    return;
  }

  await mkdir(tempDir, { recursive: true });
  try {
    for (const archive of pointer.archives) {
      const file = path.join(tempDir, archive.file);
      try {
        await download(base, archive, file, archiveTimeoutMs);
      } catch (err) {
        throw err instanceof IntegrityError ? err : new Error(`the LAN cache failed (${requestFailure(err)})`);
      }
      await unpack(file, roots[archive.name], archive.name, archive.compressed);
      await rm(file, { force: true });
    }
  } catch (err) {
    // Half a cache is worse than none: take back whatever was unpacked.
    await Promise.all(pointer.archives.map((a) => rm(path.join(roots[a.name], a.name), { recursive: true, force: true })));
    warning(`The build cache was not restored (${describe(err)}); the build runs cold.`);
    return;
  } finally {
    await rm(tempDir, { recursive: true, force: true });
  }
  const total = pointer.archives.reduce((sum, a) => sum + a.size, 0);
  const from = pointer.commit ? `, saved by ${pointer.commit}${pointer.run ? ` (run ${pointer.run})` : ''}` : '';
  io.stdout(`Restored the build cache in ${seconds(started)}: ${mib(total)}${from}.\n`);
}

// --- Save ---------------------------------------------------------------------------

/**
 * The head of `branch` in `repository`, from the GitHub API at `apiUrl`.
 */
export async function branchHead({ apiUrl, repository, branch, token, timeoutMs = 30_000 }) {
  const ref = branch.split('/').map(encodeURIComponent).join('/');
  const response = await fetchWithRetry(`${apiUrl}/repos/${repository}/git/ref/heads/${ref}`, {
    headers: {
      accept: 'application/vnd.github+json',
      authorization: `Bearer ${token}`,
      'x-github-api-version': '2022-11-28',
    },
    timeoutMs,
  });
  return (await response.json())?.object?.sha;
}

/**
 * Saves the cache under `key`, from a push of commit `sha` to `ref`: only
 * when `ref` is the default branch and `sha` is still its head (`head()`),
 * so an older push that finishes last does not replace a newer push's cache.
 * Anything that stops the save is a line in the log, or a warning when
 * something failed, never a failed step.
 */
export async function save({
  base,
  key,
  writeAuth,
  workspace,
  cargoHome,
  tempDir,
  ref,
  sha,
  run,
  defaultBranch,
  head,
  names = () => workspaceNames(workspace),
  zstd = 'zstd',
  pointerTimeoutMs = 10_000,
  archiveTimeoutMs = 300_000,
}) {
  if (ref !== `refs/heads/${defaultBranch}`) {
    io.stdout(`${ref} is not the default branch's: the build cache is not saved.\n`);
    return;
  }
  let current;
  try {
    current = await head();
  } catch (err) {
    warning(`The build cache was not saved: the head of ${defaultBranch} could not be read (${requestFailure(err)}).`);
    return;
  }
  if (current !== sha) {
    io.stdout(`${defaultBranch} has moved on to ${current}: the build cache is left to that push.\n`);
    return;
  }
  const targetDir = path.join(workspace, 'target');
  if (!existsSync(targetDir)) {
    io.stdout(`${targetDir} does not exist: there is no build cache to save.\n`);
    return;
  }

  const started = Date.now();
  const compress = await available(zstd);
  const archives = [];
  await mkdir(tempDir, { recursive: true });
  try {
    try {
      await removeWorkspaceOutputs(targetDir, await names());
      for (const [name, root] of [['target', workspace], ['git', cargoHome]]) {
        if (!existsSync(path.join(root, name))) continue;
        const file = path.join(tempDir, `${name}.tar${compress ? '.zst' : ''}`);
        await pack(root, name, file, compress);
        const hash = await sha256(createReadStream(file));
        archives.push({ name, file: path.basename(file), local: file, hash, size: (await stat(file)).size });
      }
    } catch (err) {
      warning(`The build cache was not saved: ${describe(err)}`);
      return;
    }
    const headers = { authorization: writeAuth };
    try {
      for (const archive of archives) {
        const response = await fetchWithRetry(`${base}/cas/${archive.hash}`, {
          method: 'PUT',
          body: await openAsBlob(archive.local),
          headers: { ...headers, 'content-type': 'application/octet-stream' },
          attempts: 1,
          timeoutMs: archiveTimeoutMs,
        });
        await response.arrayBuffer();
      }
      // Last, so the pointer never names an archive the store does not hold.
      const response = await fetchWithRetry(`${base}/ac/${key}`, {
        method: 'PUT',
        body: pointerBody(archives, { commit: sha, run }),
        headers: { ...headers, 'content-type': 'application/json' },
        attempts: 1,
        timeoutMs: pointerTimeoutMs,
      });
      await response.arrayBuffer();
    } catch (err) {
      warning(`The build cache was not saved: the LAN cache failed (${requestFailure(err)}).`);
      return;
    }
  } finally {
    await rm(tempDir, { recursive: true, force: true });
  }
  const parts = archives.map((a) => `${a.name} ${mib(a.size)}`).join(', ');
  io.stdout(`Saved the build cache in ${seconds(started)}: ${parts}${compress ? '' : ' (uncompressed: no zstd on this runner)'}.\n`);
}

// --- The two steps ------------------------------------------------------------------

/**
 * The main step. Without a store URL (a hosted runner, a fork) it does
 * nothing. With one, it names the cache and keeps the key for the post step;
 * then a writer (`writeAuth`) leaves the build cold, and anyone else
 * restores.
 */
export async function main({ storeUrl, writeAuth = '', workspace, cargoHome, runnerTemp, job, identifyRunner = identify, ...options }) {
  if (!storeUrl) {
    io.stdout('No LAN cache on this runner: the build runs cold.\n');
    return;
  }
  if (writeAuth !== '') setSecret(writeAuth);
  const base = maskUrl(storeUrl, 'RP_LAN_CACHE_URL');
  let key;
  try {
    const { rustc, system } = await identifyRunner();
    key = cacheKey({ job, rustc, system, workspace, cargoHome, runnerTemp });
    io.stdout(`Build cache key ${key}: ${rustc.split('\n')[0]}, system packages ${system.slice(0, 12)}.\n`);
  } catch (err) {
    warning(`The build cache cannot be named (${describe(err)}); the build runs cold.`);
    return;
  }
  saveState('key', key);
  if (writeAuth !== '') {
    io.stdout('With the write credential the build runs cold, and the post step saves it if the job succeeds.\n');
    return;
  }
  await restore({ base, key, workspace, cargoHome, tempDir: path.join(runnerTemp, 'cargo-cache'), ...options });
}

/**
 * The post step, which runs if the job succeeded: saves the cache the main
 * step named, when that step had the write credential.
 */
export async function post({ storeUrl, writeAuth = '', workspace, cargoHome, runnerTemp, eventPath, ref, sha, run, apiUrl, repository, token, ...options }) {
  const key = getState('key');
  if (writeAuth === '' || key === '') return;
  setSecret(writeAuth);
  const base = maskUrl(storeUrl, 'RP_LAN_CACHE_URL');
  let defaultBranch;
  try {
    defaultBranch = JSON.parse(await readFile(eventPath, 'utf8'))?.repository?.default_branch;
  } catch (err) {
    warning(`The build cache was not saved: the event payload could not be read (${describe(err)}).`);
    return;
  }
  if (!defaultBranch) {
    warning('The build cache was not saved: the event payload names no default branch.');
    return;
  }
  await save({
    base,
    key,
    writeAuth,
    workspace,
    cargoHome,
    tempDir: path.join(runnerTemp, 'cargo-cache'),
    ref,
    sha,
    run,
    defaultBranch,
    head: () => branchHead({ apiUrl, repository, branch: defaultBranch, token }),
    ...options,
  });
}
