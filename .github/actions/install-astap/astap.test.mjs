import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { copyFileSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, statSync, writeFileSync } from 'node:fs';
import { EOL, tmpdir } from 'node:os';
import { basename, join } from 'node:path';
import test from 'node:test';

import { capture, serve, withEnv, zip } from '../lib/testing.mjs';
import {
  CLI_DOWNLOAD,
  DATABASE_DOWNLOAD,
  formatTable,
  install,
  MAX_STAGGER_SECONDS,
  pinOf,
  readResults,
  readTable,
  repin,
  resolve,
  SERVER,
  staggerSeconds,
  TABLE,
  TABLE_FILE,
  writeTable,
} from './astap.mjs';

// --- Fixtures -----------------------------------------------------------------

function tempDir() {
  return mkdtempSync(join(process.env.TEST_TMPDIR ?? tmpdir(), 'astap-'));
}

const sha = (bytes) => createHash('sha256').update(bytes).digest('hex');

// The runner like this machine, so the binary carries the name this OS runs.
const RUNNER = process.platform === 'win32' ? 'Windows-X64' : 'Linux-X64';
const BINARY = TABLE.cli[RUNNER].binary;

// Archives as ASTAP's are laid out: the CLI's holds the binary alone at its
// top, the database's its files.
const CLI_ZIP = zip([{ path: BINARY, data: 'the solver', mode: 0o644 }]);
const DATABASE_ZIP = zip([
  { path: 'd05_0101.1476', data: 'tile 1' },
  { path: 'd05_0201.1476', data: 'tile 2' },
]);

// A SourceForge stand-in serving `archives` (file name to bytes) at the paths
// resolve builds, and resolve's assets for it, pinned to those bytes unless
// `pins` says otherwise.
async function sourceForge(archives = { [TABLE.cli[RUNNER].archive]: CLI_ZIP, [TABLE.database.archive]: DATABASE_ZIP }, pins = {}) {
  const server = await serve((req, res) => {
    // <folder>/<archive>/download
    const body = archives[decodeURIComponent(req.url.split('/').at(-2))];
    res.statusCode = body ? 200 : 404;
    res.end(body);
  });
  const pinned = (asset) => ({ ...asset, pin: { sha256: pins[asset.name] ?? sha(archives[asset.name] ?? '') } });
  const { cli, database } = resolve({ runner: RUNNER, server: server.url });
  return { server, cli: pinned(cli), database: pinned(database) };
}

// Runs `fn` with command files to write to, and the environment it changes
// put back after; resolves to what it wrote to each.
async function inStep(fn) {
  const dir = tempDir();
  const files = { GITHUB_PATH: join(dir, 'path'), GITHUB_ENV: join(dir, 'env'), GITHUB_OUTPUT: join(dir, 'output') };
  for (const file of Object.values(files)) writeFileSync(file, '');
  const { stdout } = await withEnv({ ...files, ASTAP_BINARY: undefined, ASTAP_DB_DIR: undefined }, () => capture(fn));
  return {
    stdout,
    path: readFileSync(files.GITHUB_PATH, 'utf8'),
    env: readFileSync(files.GITHUB_ENV, 'utf8'),
    output: readFileSync(files.GITHUB_OUTPUT, 'utf8'),
  };
}

// The value an entry in a command file's multi-line form sets.
function value(text, name) {
  const lines = text.split(EOL);
  const at = lines.findIndex((line) => line.startsWith(`${name}<<`));
  return at === -1 ? undefined : lines[at + 1];
}

const fast = { delayMs: 1 };

// --- The table ------------------------------------------------------------------

test('the table pins a CLI for each runner ADR-005 names, and the database', () => {
  assert.deepEqual(Object.keys(TABLE.cli).sort(), ['Linux-ARM64', 'Linux-X64', 'Windows-ARM64', 'Windows-X64', 'macOS-ARM64']);
  assert.equal(TABLE.database.archive, 'd05_star_database.zip');
  for (const [runner, { binary }] of Object.entries(TABLE.cli)) {
    assert.equal(binary, runner.startsWith('Windows-') ? 'astap_cli.exe' : 'astap_cli', runner);
  }
});

test('assets.json is in the layout formatTable writes, so a rewrite changes only the pins', () => {
  assert.equal(readFileSync(TABLE_FILE, 'utf8'), formatTable(readTable()));
});

test('readTable refuses a table missing what an install needs', () => {
  const dir = tempDir();
  const write = (table) => {
    const file = join(dir, 'assets.json');
    writeFileSync(file, JSON.stringify(table));
    return file;
  };
  const whole = structuredClone(TABLE);
  assert.throws(() => readTable(write({ ...whole, cli: {} })), /assets.json: no cli entries/);
  const noBinary = structuredClone(TABLE);
  delete noBinary.cli['Linux-X64'].binary;
  assert.throws(() => readTable(write(noBinary)), /the CLI for Linux-X64 has no binary/);
  const shortPin = structuredClone(TABLE);
  shortPin.database.sha256 = 'abc';
  assert.throws(() => readTable(write(shortPin)), /the sha256 of the database, "abc", is not a SHA-256/);
  const upper = structuredClone(TABLE);
  upper.cli['macOS-ARM64'].sha256 = upper.cli['macOS-ARM64'].sha256.toUpperCase();
  assert.throws(() => readTable(write(upper)), /the sha256 of the CLI for macOS-ARM64/);
});

// --- Choosing the assets ----------------------------------------------------------

test('resolve gives each runner its archive on SourceForge, with its pin and binary', () => {
  // The URLs the shell built, folder and all.
  const urls = {
    'Linux-X64': 'linux_installer/astap_command-line_version_Linux_amd64.zip',
    'Linux-ARM64': 'linux_installer/astap_command-line_version_Linux_aarch64.zip',
    'macOS-ARM64': 'macOS%20installer/astap_command-line_version_macOS_M1.zip',
    'Windows-X64': 'windows_installer/astap_command-line_version_win64.zip',
    'Windows-ARM64': 'windows_installer/astap_command-line_version_win11_aarch64.zip',
  };
  for (const [runner, at] of Object.entries(urls)) {
    const { cli } = resolve({ runner });
    assert.deepEqual(
      cli,
      {
        name: basename(at),
        url: `https://sourceforge.net/projects/astap-program/files/${at}/download`,
        pin: { sha256: TABLE.cli[runner].sha256 },
        binary: TABLE.cli[runner].binary,
      },
      runner,
    );
  }
  assert.throws(() => resolve({ runner: 'macOS-X64' }), /ASTAP has no asset for macOS-X64/);
});

test('resolve gives every runner the same database', () => {
  const { database } = resolve({ runner: 'Linux-ARM64' });
  assert.deepEqual(database, {
    name: 'd05_star_database.zip',
    url: `${SERVER}/star_databases/d05_star_database.zip/download`,
    pin: { sha256: TABLE.database.sha256 },
  });
  assert.deepEqual(resolve({ runner: 'Windows-X64' }).database, database);
});

test('resolve keys each cache on its archive and pin, so moving a pin misses the old cache', () => {
  const { keys } = resolve({ runner: 'Linux-X64' });
  assert.deepEqual(keys, {
    cli: `install-astap-cli-astap_command-line_version_Linux_amd64.zip-${TABLE.cli['Linux-X64'].sha256}`,
    database: `install-astap-d05-d05_star_database.zip-${TABLE.database.sha256}`,
  });
  const moved = structuredClone(TABLE);
  moved.database.sha256 = '0'.repeat(64);
  assert.notEqual(resolve({ runner: 'Linux-X64', table: moved }).keys.database, keys.database);
});

// --- Downloading -------------------------------------------------------------------

test('the database download re-rolls a crawling mirror the way the curl flags did', () => {
  // --retry 4 --retry-all-errors, --retry-delay 5, --max-time 300,
  // --retry-max-time 600, --speed-limit 100000 --speed-time 30.
  assert.deepEqual(DATABASE_DOWNLOAD, {
    attempts: 5,
    delayMs: 5_000,
    stallMs: 30_000,
    minBytesPerSecond: 100_000,
    timeoutMs: 300_000,
    retryForMs: 600_000,
    retryAll: true,
  });
  // --retry 4 --retry-all-errors, --retry-delay 5, --connect-timeout 20,
  // --max-time 120.
  assert.deepEqual(CLI_DOWNLOAD, { attempts: 5, delayMs: 5_000, stallMs: 20_000, timeoutMs: 120_000, retryAll: true });
});

test('staggerSeconds takes a whole number of seconds up to its cap', () => {
  assert.equal(staggerSeconds('0'), 0);
  assert.equal(staggerSeconds('45'), 45);
  assert.equal(staggerSeconds(String(MAX_STAGGER_SECONDS)), MAX_STAGGER_SECONDS);
  for (const text of ['', ' 45', '-1', '1.5', '4e1', 'abc']) {
    assert.throws(() => staggerSeconds(text), /must be a non-negative integer/, JSON.stringify(text));
  }
  assert.throws(() => staggerSeconds('450'), /download-stagger-seconds is 450s, above the 300s cap/);
});

// --- Installing --------------------------------------------------------------------

test('install puts the CLI in <workspace>/astap and exports its path', async () => {
  const { server, cli } = await sourceForge();
  const workspace = tempDir();
  try {
    const step = await inStep(() => install({ workspace, cli, download: fast }));
    const dir = join(workspace, 'astap');
    assert.deepEqual(readdirSync(dir), [BINARY]);
    assert.equal(readFileSync(join(dir, BINARY), 'utf8'), 'the solver');
    if (process.platform !== 'win32') assert.equal(statSync(join(dir, BINARY)).mode & 0o777, 0o755);
    assert.equal(value(step.env, 'ASTAP_BINARY'), `${workspace}/astap/${BINARY}`);
    assert.equal(step.path.trim(), `${workspace}/astap`);
    assert.equal(value(step.output, 'astap-binary'), `${workspace}/astap/${BINARY}`);
    // No database asked for: the output is empty and nothing points at one.
    assert.equal(value(step.output, 'astap-db-dir'), '');
    assert.equal(value(step.env, 'ASTAP_DB_DIR'), undefined);
    assert.deepEqual(readdirSync(workspace), ['astap']);
    assert.equal(server.requests.length, 1);
  } finally {
    await server.close();
  }
});

test('install puts the database in <workspace>/astap-db, its files at the top, and exports it', async () => {
  const { server, cli, database } = await sourceForge();
  const workspace = tempDir();
  try {
    const step = await inStep(() => install({ workspace, cli, database, download: fast }));
    const dir = join(workspace, 'astap-db');
    assert.deepEqual(readdirSync(dir).sort(), ['d05_0101.1476', 'd05_0201.1476']);
    assert.equal(value(step.env, 'ASTAP_DB_DIR'), `${workspace}/astap-db`);
    assert.equal(value(step.output, 'astap-db-dir'), `${workspace}/astap-db`);
    assert.deepEqual(readdirSync(workspace).sort(), ['astap', 'astap-db']);
  } finally {
    await server.close();
  }
});

test('install takes what the caches restored, and downloads nothing', async () => {
  const { server, cli, database } = await sourceForge();
  const workspace = tempDir();
  mkdirSync(join(workspace, 'astap'));
  writeFileSync(join(workspace, 'astap', BINARY), 'cached solver');
  mkdirSync(join(workspace, 'astap-db'));
  writeFileSync(join(workspace, 'astap-db', 'd05_0101.1476'), 'cached tile');
  const waits = [];
  try {
    const step = await inStep(() =>
      install({
        workspace,
        cli,
        database,
        cliCached: true,
        databaseCached: true,
        stagger: 90,
        sleep: async (ms) => waits.push(ms),
      }),
    );
    assert.equal(server.requests.length, 0);
    // A warm cache waits for nothing.
    assert.deepEqual(waits, []);
    assert.equal(readFileSync(join(workspace, 'astap', BINARY), 'utf8'), 'cached solver');
    assert.match(step.stdout, /The ASTAP CLI restored from the cache/);
    assert.match(step.stdout, /The D05 star database restored from the cache/);
    assert.equal(value(step.env, 'ASTAP_DB_DIR'), `${workspace}/astap-db`);
  } finally {
    await server.close();
  }
});

test('install downloads again what a cache hit left out, with a warning', async () => {
  const { server, cli, database } = await sourceForge();
  const workspace = tempDir();
  mkdirSync(join(workspace, 'astap'));
  mkdirSync(join(workspace, 'astap-db'));
  try {
    const step = await inStep(() => install({ workspace, cli, database, cliCached: true, databaseCached: true, download: fast }));
    assert.equal(server.requests.length, 2);
    assert.match(step.stdout, new RegExp(`::warning::The cache restored .* without ${BINARY.replace('.', '\\.')}`));
    assert.match(step.stdout, /::warning::The cache restored .* without its files/);
    assert.deepEqual(readdirSync(join(workspace, 'astap-db')).sort(), ['d05_0101.1476', 'd05_0201.1476']);
  } finally {
    await server.close();
  }
});

test('install staggers only a database download, by the seconds it is given', async () => {
  const { server, cli, database } = await sourceForge();
  const waits = [];
  const sleep = async (ms) => waits.push(ms);
  try {
    const step = await inStep(() => install({ workspace: tempDir(), cli, database, stagger: 45, sleep, download: fast }));
    assert.deepEqual(waits, [45_000]);
    assert.match(step.stdout, /Waiting 45s before the D05 download/);
    // No database, or no stagger: no wait.
    await inStep(() => install({ workspace: tempDir(), cli, stagger: 45, sleep, download: fast }));
    await inStep(() => install({ workspace: tempDir(), cli, database, stagger: 0, sleep, download: fast }));
    assert.deepEqual(waits, [45_000]);
  } finally {
    await server.close();
  }
});

test('install refuses an archive that does not match its pin, and leaves nothing behind', async () => {
  const { server, cli } = await sourceForge(undefined, { [TABLE.cli[RUNNER].archive]: '1'.repeat(64) });
  const workspace = tempDir();
  try {
    await inStep(() =>
      assert.rejects(
        install({ workspace, cli, download: fast }),
        /SHA-256 mismatch: expected 1{64}.* refresh-astap-shas\.yml opens a pull request .*install-astap\/assets\.json/,
      ),
    );
    assert.equal(server.requests.length, CLI_DOWNLOAD.attempts);
    assert.deepEqual(readdirSync(workspace), []);
  } finally {
    await server.close();
  }
});

test('install tries another mirror after one that answers 404', async () => {
  // A mirror SourceForge sent the first attempt to has not synced the file.
  let answered = 0;
  const server = await serve((req, res) => {
    answered += 1;
    res.statusCode = answered === 1 ? 404 : 200;
    res.end(answered === 1 ? 'Not Found' : CLI_ZIP);
  });
  const { cli } = resolve({ runner: RUNNER, server: server.url });
  const pinned = { ...cli, pin: { sha256: sha(CLI_ZIP) } };
  const workspace = tempDir();
  try {
    await inStep(() => install({ workspace, cli: pinned, download: fast }));
    assert.equal(server.requests.length, 2);
    assert.deepEqual(readdirSync(join(workspace, 'astap')), [BINARY]);
  } finally {
    await server.close();
  }
});

test('install refuses a CLI archive without its binary at the top', async () => {
  const { server, cli } = await sourceForge({ [TABLE.cli[RUNNER].archive]: zip([{ path: `astap/${BINARY}`, data: 'x' }]) });
  const workspace = tempDir();
  try {
    await inStep(() => assert.rejects(install({ workspace, cli, download: fast }), new RegExp(`holds no ${BINARY.replace('.', '\\.')} at its top`)));
    assert.deepEqual(readdirSync(workspace), []);
  } finally {
    await server.close();
  }
});

test('install refuses a database archive with no files at the top', async () => {
  const archives = {
    [TABLE.cli[RUNNER].archive]: CLI_ZIP,
    [TABLE.database.archive]: zip([{ path: 'd05/' }, { path: 'd05/d05_0101.1476', data: 'tile' }]),
  };
  const { server, cli, database } = await sourceForge(archives);
  const workspace = tempDir();
  try {
    await inStep(() => assert.rejects(install({ workspace, cli, database, download: fast }), /d05_star_database.zip holds no files at its top/));
    assert.deepEqual(readdirSync(workspace), ['astap']);
  } finally {
    await server.close();
  }
});

// --- Refreshing the pins -------------------------------------------------------------

// The result refresh-astap-shas.yml's detect job uploads for `runner`.
function result(runner, newSha, archive = TABLE.cli[runner].archive) {
  return {
    arch_key: runner,
    archive,
    current_sha: TABLE.cli[runner].sha256,
    new_sha: newSha,
    banner: 'ASTAP astrometric solver version CLI-2099.01.01',
    m101_passed: 'true',
  };
}

test('pinOf reads the pin the workflow compares against', () => {
  assert.equal(pinOf(TABLE, 'Linux-ARM64', 'astap_command-line_version_Linux_aarch64.zip'), TABLE.cli['Linux-ARM64'].sha256);
  assert.throws(
    () => pinOf(TABLE, 'Linux-ARM64', 'astap_command-line_version_Linux_amd64.zip'),
    /the table pins astap_command-line_version_Linux_aarch64.zip for Linux-ARM64, not astap_command-line_version_Linux_amd64.zip/,
  );
  assert.throws(() => pinOf(TABLE, 'Linux-RISCV', 'x.zip'), /ASTAP has no asset for Linux-RISCV/);
});

test('repin moves the pins of the archives that rotated, and no others', () => {
  const { table, changes } = repin(TABLE, [result('Linux-X64', 'a'.repeat(64)), result('Windows-ARM64', 'b'.repeat(64))]);
  assert.equal(table.cli['Linux-X64'].sha256, 'a'.repeat(64));
  assert.equal(table.cli['Windows-ARM64'].sha256, 'b'.repeat(64));
  const unchanged = (t) => ({ ...t, cli: { ...t.cli, 'Linux-X64': undefined, 'Windows-ARM64': undefined } });
  assert.deepEqual(unchanged(table), unchanged(TABLE));
  assert.deepEqual(changes, [
    { runner: 'Linux-X64', archive: TABLE.cli['Linux-X64'].archive, from: TABLE.cli['Linux-X64'].sha256, to: 'a'.repeat(64) },
    { runner: 'Windows-ARM64', archive: TABLE.cli['Windows-ARM64'].archive, from: TABLE.cli['Windows-ARM64'].sha256, to: 'b'.repeat(64) },
  ]);
  // The table it was given is left as it was.
  assert.deepEqual(TABLE, readTable());
});

test('repin refuses a result whose archive or new pin it cannot trust', () => {
  assert.throws(() => repin(TABLE, [result('macOS-ARM64', 'a'.repeat(64), 'other.zip')]), /not other.zip: the workflow and assets.json disagree/);
  assert.throws(() => repin(TABLE, [result('macOS-ARM64', 'A'.repeat(64))]), /the new pin of .*macOS_M1.zip, "A{64}", is not a SHA-256/);
  assert.throws(() => repin(TABLE, [result('macOS-ARM64', undefined)]), /the new pin of .*, undefined, is not a SHA-256/);
});

test('a refresh rewrites a copy of assets.json at the rotated pin, and nowhere else', async () => {
  const dir = tempDir();
  const file = join(dir, 'assets.json');
  copyFileSync(TABLE_FILE, file);
  // As download-artifact lays out the results: a directory per artifact.
  const results = join(dir, 'artifacts');
  mkdirSync(join(results, 'refresh-result-macOS-ARM64'), { recursive: true });
  writeFileSync(join(results, 'refresh-result-macOS-ARM64', 'macOS-ARM64.json'), JSON.stringify(result('macOS-ARM64', 'c'.repeat(64))));
  const before = readFileSync(file, 'utf8').split('\n');

  await writeTable(file, repin(readTable(file), await readResults(results)).table);

  const after = readFileSync(file, 'utf8').split('\n');
  assert.equal(after.length, before.length);
  const changed = after.flatMap((line, i) => (line === before[i] ? [] : [[before[i], line]]));
  assert.deepEqual(changed, [[`      "sha256": "${TABLE.cli['macOS-ARM64'].sha256}"`, `      "sha256": "${'c'.repeat(64)}"`]]);
  assert.equal(readTable(file).cli['macOS-ARM64'].sha256, 'c'.repeat(64));
});

test('readResults reads every artifact directory download-artifact made, in order', async () => {
  const dir = tempDir();
  for (const runner of ['Windows-X64', 'Linux-X64']) {
    mkdirSync(join(dir, `refresh-result-${runner}`));
    writeFileSync(join(dir, `refresh-result-${runner}`, `${runner}.json`), JSON.stringify(result(runner, 'd'.repeat(64))));
  }
  assert.deepEqual(
    (await readResults(dir)).map(({ arch_key }) => arch_key),
    ['Linux-X64', 'Windows-X64'],
  );
});
