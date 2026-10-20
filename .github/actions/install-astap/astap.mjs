// ASTAP, the plate solver plate-solver runs (ADR-005): which CLI archive each
// runner takes, and its install, with the D05 star database when a job asks
// for it, into <workspace>/astap and <workspace>/astap-db.
//
// The table is assets.json, beside this file, rather than a constant here,
// because a workflow rewrites it. When the nightly install-astap smoke fails,
// refresh-astap-shas.yml reads each runner's pin (pin/), checks the archive
// SourceForge now serves by solving a known field with it, and writes the new
// SHA-256 back (repin/) in a pull request. Both read and write the table as
// JSON, whole, so neither depends on how the file is laid out; formatTable is
// its one layout, so a rewrite changes the sha256 lines and nothing else.
//
// The URLs are SourceForge's for the newest file of each name, which upstream
// replaces in place at each release. The SHA-256 pins one release: a new one
// fails the check, closed, until the refresh moves the pin.

import { existsSync, readdirSync, readFileSync } from 'node:fs';
import { chmod, readdir, readFile, rename, rm, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { setTimeout as sleepFor } from 'node:timers/promises';
import { fileURLToPath } from 'node:url';

import { addPath, exportVariable, io, setOutput, warning } from '../lib/helpers.mjs';
import { cacheKey, ChecksumError, download, forRunner, unpack, withWorkDir } from '../lib/install.mjs';

/** The table of what each runner downloads, and its pins. */
export const TABLE_FILE = fileURLToPath(new URL('./assets.json', import.meta.url));

/** Where SourceForge keeps the astap-program project's files. */
export const SERVER = 'https://sourceforge.net/projects/astap-program/files';

const SHA256 = /^[0-9a-f]{64}$/;

// --- The table ------------------------------------------------------------------

function checkEntry(entry, fields, what, where) {
  for (const field of fields) {
    if (typeof entry?.[field] !== 'string' || entry[field] === '') {
      throw new Error(`${where}: ${what} has no ${field}`);
    }
  }
  if (!SHA256.test(entry.sha256)) {
    throw new Error(`${where}: the sha256 of ${what}, ${JSON.stringify(entry.sha256)}, is not a SHA-256 in lowercase hex`);
  }
}

function checkTable(table, where) {
  if (typeof table?.cli !== 'object' || table.cli === null || Object.keys(table.cli).length === 0) {
    throw new Error(`${where}: no cli entries`);
  }
  for (const [runner, entry] of Object.entries(table.cli)) {
    checkEntry(entry, ['folder', 'archive', 'binary', 'sha256'], `the CLI for ${runner}`, where);
  }
  checkEntry(table.database, ['folder', 'archive', 'sha256'], 'the database', where);
}

/**
 * The table in `file`: `cli`, each runner's CLI archive ({ folder, archive,
 * binary, sha256 }), keyed `${RUNNER_OS}-${RUNNER_ARCH}`, and `database`, the
 * D05 archive ({ folder, archive, sha256 }). Throws when an entry lacks one.
 */
export function readTable(file = TABLE_FILE) {
  const table = JSON.parse(readFileSync(file, 'utf8'));
  checkTable(table, path.basename(file));
  return table;
}

/** The table's one layout, which assets.json keeps. */
export function formatTable(table) {
  return `${JSON.stringify(table, null, 2)}\n`;
}

/** Writes `table` to `file` in that layout, once it is known to be whole. */
export async function writeTable(file, table) {
  checkTable(table, path.basename(file));
  await writeFile(file, formatTable(table));
}

/** The table assets.json holds. */
export const TABLE = readTable();

// --- Choosing the assets ----------------------------------------------------------

function url(server, { folder, archive }) {
  return `${server}/${encodeURIComponent(folder)}/${encodeURIComponent(archive)}/download`;
}

/**
 * What `runner` installs: its CLI archive ({ name, url, pin }, with the
 * binary it holds) and the database's, and the cache key of each.
 */
export function resolve({ runner, table = TABLE, server = SERVER }) {
  const entry = forRunner(table.cli, runner, 'ASTAP');
  const cli = { name: entry.archive, url: url(server, entry), pin: { sha256: entry.sha256 }, binary: entry.binary };
  const database = { name: table.database.archive, url: url(server, table.database), pin: { sha256: table.database.sha256 } };
  return {
    cli,
    database,
    keys: { cli: cacheKey('astap-cli', [cli]), database: cacheKey('astap-d05', [database]) },
  };
}

// --- Downloading -------------------------------------------------------------------

// SourceForge sends each client to one of many mirrors, and a bad one can
// fail in any way: a bad certificate, a malformed body, a 404 for a file it
// has not synced yet, a transfer dropped part way, or one served at a crawl.
// The next attempt likely lands on another mirror, so both downloads try
// again after any failure (retryAll), as curl's --retry-all-errors did. A
// crawl does not fail: only a time limit or a floor on the rate ends such an
// attempt.

/**
 * The CLI archive, about 1 MB. A mirror that takes two minutes over it is
 * broken, and a new attempt beats waiting on it.
 */
export const CLI_DOWNLOAD = { attempts: 5, delayMs: 5_000, stallMs: 20_000, timeoutMs: 120_000, retryAll: true };

/**
 * The database, about 100 MB. A healthy mirror serves it in about 5 s
 * (~19 MB/s), so a floor of 100,000 bytes/s over 30 s sits about 190 times
 * below one and cannot fire on a merely unlucky transfer; a mirror under it
 * is broken, and a new attempt in 30 s beats waiting out the 300 s limit.
 * Without the floor, each attempt on a crawling mirror would take the full
 * 300 s, and two of them would use up the time the attempts have in all. No
 * attempt starts after 10 minutes, so a run of bad mirrors cannot stretch the
 * job unboundedly.
 */
export const DATABASE_DOWNLOAD = {
  attempts: 5,
  delayMs: 5_000,
  stallMs: 30_000,
  minBytesPerSecond: 100_000,
  timeoutMs: 300_000,
  retryForMs: 600_000,
  retryAll: true,
};

/**
 * The longest download-stagger-seconds may ask for. Spacing the legs of a
 * matrix only has to exceed one healthy database download, well under 30 s,
 * so a value near this is a typo: a stray digit turning 45 into 450 would
 * otherwise spend runner minutes on nothing, silently.
 */
export const MAX_STAGGER_SECONDS = 300;

/** The input download-stagger-seconds, `text`, as a number of seconds. */
export function staggerSeconds(text) {
  if (!/^[0-9]+$/.test(text)) {
    throw new Error(`download-stagger-seconds must be a non-negative integer, got '${text}'`);
  }
  const seconds = Number(text);
  if (seconds > MAX_STAGGER_SECONDS) {
    throw new Error(`download-stagger-seconds is ${seconds}s, above the ${MAX_STAGGER_SECONDS}s cap`);
  }
  return seconds;
}

// --- Installing --------------------------------------------------------------------

// What to do when a pin no longer matches: SourceForge serves each release
// under the same name, so a mismatch is most likely a new one.
const ROTATED =
  'If upstream released a new one, refresh-astap-shas.yml opens a pull request with the new pin; ' +
  'the pins are in .github/actions/install-astap/assets.json, and ADR-005 §"Pinned SHA-256 + refresh procedure" ' +
  'has the manual steps.';

// Downloads, checks and unpacks `asset` into `dir`, replacing what is there,
// once `check` accepts the unpacked directory.
async function unpackInto(asset, dir, options, check) {
  await withWorkDir(dir, async (work) => {
    const archive = path.join(work, asset.name);
    try {
      await download(asset, archive, options);
    } catch (err) {
      if (err instanceof ChecksumError) throw new ChecksumError(`${err.message} ${ROTATED}`);
      throw err;
    }
    const unpacked = path.join(work, 'unpacked');
    await unpack(archive, unpacked);
    check(unpacked);
    await rm(dir, { recursive: true, force: true });
    await rename(unpacked, dir);
  });
}

function holdsFiles(dir) {
  return existsSync(dir) && readdirSync(dir, { withFileTypes: true }).some((entry) => entry.isFile());
}

async function installCli(cli, dir, options) {
  await unpackInto(cli, dir, { ...CLI_DOWNLOAD, ...options }, (unpacked) => {
    if (!existsSync(path.join(unpacked, cli.binary))) throw new Error(`${cli.name} holds no ${cli.binary} at its top`);
  });
}

// ASTAP reads the database's files from the directory it is given, so they
// must be at the archive's top.
async function installDatabase(database, dir, options) {
  await unpackInto(database, dir, { ...DATABASE_DOWNLOAD, ...options }, (unpacked) => {
    if (!holdsFiles(unpacked)) throw new Error(`${database.name} holds no files at its top`);
  });
}

/**
 * Installs the CLI into `<workspace>/astap`, unless `cliCached` says the
 * cache restored it there, and, given a `database`, the database into
 * `<workspace>/astap-db` the same way, waiting `stagger` seconds before
 * downloading it. Then puts the CLI's directory on PATH and exports
 * ASTAP_BINARY, and ASTAP_DB_DIR with a database, setting the step's outputs
 * astap-binary and astap-db-dir to the same paths (astap-db-dir empty without
 * one). `download` overrides the download options of both.
 */
export async function install({
  workspace,
  cli,
  database,
  cliCached,
  databaseCached,
  stagger = 0,
  download: options,
  sleep = sleepFor,
}) {
  const dir = path.join(workspace, 'astap');
  if (cliCached && existsSync(path.join(dir, cli.binary))) {
    io.stdout(`The ASTAP CLI restored from the cache into ${dir}\n`);
  } else {
    if (cliCached) warning(`The cache restored ${dir} without ${cli.binary}; downloading the ASTAP CLI again`);
    await installCli(cli, dir, options);
  }
  if (!cli.binary.endsWith('.exe')) await chmod(path.join(dir, cli.binary), 0o755);
  // `${workspace}/astap/...`, joined with '/' on Windows too, as the action
  // wrote them before.
  const binary = `${workspace}/astap/${cli.binary}`;
  addPath(`${workspace}/astap`);
  exportVariable('ASTAP_BINARY', binary);
  setOutput('astap-binary', binary);
  if (database === undefined) {
    setOutput('astap-db-dir', '');
    return;
  }

  const dbDir = path.join(workspace, 'astap-db');
  if (databaseCached && holdsFiles(dbDir)) {
    io.stdout(`The D05 star database restored from the cache into ${dbDir}\n`);
  } else {
    if (databaseCached) warning(`The cache restored ${dbDir} without its files; downloading the D05 star database again`);
    // Only before a download: a warm cache waits for nothing.
    if (stagger > 0) {
      io.stdout(`Waiting ${stagger}s before the D05 download, to offset this leg from its siblings\n`);
      await sleep(stagger * 1_000);
    }
    await installDatabase(database, dbDir, options);
  }
  exportVariable('ASTAP_DB_DIR', `${workspace}/astap-db`);
  setOutput('astap-db-dir', `${workspace}/astap-db`);
}

// --- Refreshing the pins -------------------------------------------------------------

/**
 * The SHA-256 `table` pins for `runner`'s CLI archive, which must be
 * `archive`: refresh-astap-shas.yml names each runner's archive in its own
 * matrix, and downloads that one to compare.
 */
export function pinOf(table, runner, archive) {
  const entry = forRunner(table.cli, runner, 'ASTAP');
  if (entry.archive !== archive) {
    throw new Error(`the table pins ${entry.archive} for ${runner}, not ${archive}: the workflow and assets.json disagree`);
  }
  return entry.sha256;
}

/**
 * `table` with the new pin of each of `results` in it, and what changed. A
 * result is a runner's from refresh-astap-shas.yml, `{ arch_key, archive,
 * new_sha }`, and its archive must be the one the table names for that
 * runner.
 */
export function repin(table, results) {
  const next = structuredClone(table);
  const changes = [];
  for (const { arch_key: runner, archive, new_sha: sha256 } of results) {
    const from = pinOf(next, runner, archive);
    if (!SHA256.test(sha256 ?? '')) {
      throw new Error(`the new pin of ${archive}, ${JSON.stringify(sha256)}, is not a SHA-256 in lowercase hex`);
    }
    next.cli[runner].sha256 = sha256;
    changes.push({ runner, archive, from, to: sha256 });
  }
  return { table: next, changes };
}

/**
 * The results refresh-astap-shas.yml's jobs upload, read from `dir` as
 * download-artifact lays them out: a directory per artifact, each holding one
 * runner's JSON file.
 */
export async function readResults(dir) {
  const results = [];
  const artifacts = (await readdir(dir, { withFileTypes: true })).filter((entry) => entry.isDirectory());
  for (const artifact of artifacts.map((entry) => entry.name).sort()) {
    const files = (await readdir(path.join(dir, artifact))).filter((name) => name.endsWith('.json')).sort();
    for (const file of files) results.push(JSON.parse(await readFile(path.join(dir, artifact, file), 'utf8')));
  }
  return results;
}
