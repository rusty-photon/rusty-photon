import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';

import { TABLE } from '../install-astap/astap.mjs';
import { capture, serve, withEnv, zip } from '../lib/testing.mjs';
import { CROSS_MIRROR, DEGENERATE, detect, M101, runProgram, solveFixtures, wcsCentre } from './detect.mjs';

// --- Fixtures -----------------------------------------------------------------

function tempDir() {
  return mkdtempSync(join(process.env.TEST_TMPDIR ?? tmpdir(), 'refresh-'));
}

const sha = (bytes) => createHash('sha256').update(bytes).digest('hex');

// The .wcs the real ASTAP CLI-2026.09.01 wrote for M101 on Linux x64, solved
// as the check solves it: 72 cards of 80 characters, padded to two FITS
// blocks, with no line breaks.
const RECORDED_WCS = readFileSync(new URL('./testdata/m101_known.wcs', import.meta.url), 'latin1');

// The runner like this machine, so the binary carries the name this OS runs.
const RUNNER = process.platform === 'win32' ? 'Windows-X64' : 'Linux-X64';
const ENTRY = TABLE.cli[RUNNER];

// What the real ASTAP does, as a Node script started in the binary's place: it
// reads the scenario the binary's file holds. Run with no arguments it prints
// the banner; given a field with -f it does what the scenario says for that
// field: prints `say` and `err`, writes `wcs` beside it, and exits `code`, or
// hangs. Each run is recorded, with the names in its environment, in the file
// FAKE_ASTAP_CALLS names.
const FAKE = `
const fs = require('node:fs');
const path = require('node:path');
const [binary, ...args] = process.argv.slice(1);
const scenario = JSON.parse(fs.readFileSync(binary, 'utf8'));
if (process.env.FAKE_ASTAP_CALLS) {
  fs.appendFileSync(process.env.FAKE_ASTAP_CALLS, JSON.stringify({ args, cwd: process.cwd(), env: Object.keys(process.env) }) + '\\n');
}
if (args.length === 0) {
  process.stdout.write(scenario.banner + '\\r\\nUsage: astap_cli -f file\\n');
  process.exit(0);
}
const fits = args[args.indexOf('-f') + 1];
const field = path.basename(fits) === 'm101_known.fits' ? scenario.m101 : scenario.empty;
if (field.hang) {
  setInterval(() => {}, 1000);
} else {
  process.stdout.write(field.say ?? '');
  process.stderr.write(field.err ?? '');
  if (field.wcs !== undefined) fs.writeFileSync(fits.replace(/\\.fits$/, '.wcs'), field.wcs, 'latin1');
  process.exit(field.code);
}
`;

// runProgram, with the fake in the binary's place.
const fake = (binary, args, options) => runProgram(process.execPath, ['-e', FAKE, binary, ...args], options);

const BANNER = 'ASTAP astrometric solver version CLI-2099.01.01';

// A release that does what the real one does: solves M101 to the recorded
// .wcs, and gives up on the empty field.
const GOOD = {
  banner: BANNER,
  m101: { code: 0, wcs: RECORDED_WCS, say: 'Solution found: 14: 03  14.4 +54d 20  49\n' },
  empty: { code: 1, say: 'Only 0 stars found in image. Abort\n' },
};

// The two fixtures, in a directory of their own.
function fixtures() {
  const dir = tempDir();
  writeFileSync(join(dir, M101.file), 'the M101 field');
  writeFileSync(join(dir, DEGENERATE), 'a field with no stars');
  return dir;
}

// A file holding `scenario`, for solveFixtures to run the fake on.
function scenarioFile(scenario) {
  const file = join(tempDir(), 'astap_cli');
  writeFileSync(file, JSON.stringify(scenario));
  return file;
}

const DATABASE_ZIP = zip([
  { path: 'd05_0101.1476', data: 'tile 1' },
  { path: 'd05_0201.1476', data: 'tile 2' },
]);

// A SourceForge stand-in serving this runner's CLI archive, its binary
// holding `scenario`, and the database, at the paths resolve builds; and the
// table pointing at it, pinning the CLI to `pin` (the served archive's own
// SHA-256 unless given) and the database to the served one.
async function sourceForge(scenario, { pin, database = DATABASE_ZIP, entry = { path: ENTRY.binary } } = {}) {
  const cli = zip([{ ...entry, data: JSON.stringify(scenario) }]);
  const archives = { [ENTRY.archive]: cli, [TABLE.database.archive]: database };
  const server = await serve((req, res) => {
    // <folder>/<archive>/download
    const body = archives[decodeURIComponent(req.url.split('/').at(-2))];
    res.statusCode = body ? 200 : 404;
    res.end(body);
  });
  const table = structuredClone(TABLE);
  table.cli[RUNNER].sha256 = pin ?? sha(cli);
  table.database.sha256 = sha(DATABASE_ZIP);
  return { server, table, upstream: sha(cli) };
}

const OLD_PIN = 'a'.repeat(64);

// Runs detect for this runner against `site`, recording the fake's runs.
async function detectHere(site, options = {}) {
  const work = tempDir();
  const results = join(tempDir(), 'refresh-result');
  const calls = join(tempDir(), 'calls.jsonl');
  const { value, stdout } = await withEnv({ FAKE_ASTAP_CALLS: calls }, () =>
    capture(() =>
      detect({
        runner: RUNNER,
        verify: true,
        here: RUNNER,
        work,
        results,
        fixtures: fixtures(),
        table: site.table,
        server: site.server.url,
        run: fake,
        download: { delayMs: 1 },
        ...options,
      }),
    ),
  );
  const runs = existsSync(calls) ? readFileSync(calls, 'utf8').trim().split('\n').map((line) => JSON.parse(line)) : [];
  const file = join(results, `${options.runner ?? RUNNER}.json`);
  const result = existsSync(file) ? JSON.parse(readFileSync(file, 'utf8')) : undefined;
  return { value, stdout, runs, result, work };
}

const downloads = (site) => site.server.requests.map(({ url }) => decodeURIComponent(url.split('/').at(-2)));

// --- Comparing with the pin -----------------------------------------------------

test('an archive that is still the pinned one is no rotation, and nothing runs or is written', async (t) => {
  const site = await sourceForge(GOOD);
  t.after(site.server.close);
  const { value, stdout, runs, result } = await detectHere(site);
  assert.deepEqual(value, { rotated: false, upstream: site.upstream });
  assert.match(stdout, new RegExp(`No rotation: ${ENTRY.archive} is still the pinned ${site.upstream}`));
  assert.deepEqual(downloads(site), [ENTRY.archive]);
  assert.deepEqual(runs, []);
  assert.equal(result, undefined);
});

test('a rotated archive whose binary solves M101 is a passed result, with its banner', async (t) => {
  const site = await sourceForge(GOOD, { pin: OLD_PIN });
  t.after(site.server.close);
  const { value, stdout, runs, result, work } = await detectHere(site);
  assert.deepEqual(value, { rotated: true, upstream: site.upstream });
  assert.deepEqual(result, {
    runner: RUNNER,
    archive: ENTRY.archive,
    pinned: OLD_PIN,
    upstream: site.upstream,
    banner: BANNER,
    verified: 'passed',
  });
  assert.deepEqual(downloads(site), [ENTRY.archive, TABLE.database.archive]);
  assert.match(stdout, /M101 verified: RA 210\.8098818046, Dec 54\.346932537; degenerate_no_stars\.fits failed as expected/);
  // What ASTAP said goes to the log.
  assert.match(stdout, /Solution found: 14: 03 {2}14\.4/);

  // The banner, then each field, solved in a copy of the fixtures against the
  // database as it unpacked.
  const solve = join(work, 'solve');
  assert.deepEqual(
    runs.map(({ args, cwd }) => [args, cwd]),
    [
      [[], work],
      [['-f', join(solve, M101.file), '-d', join(work, 'd05'), '-wcs', '-ra', '14.0540', '-spd', '144.3469', '-fov', '0.42', '-r', '3.0'], solve],
      [['-f', join(solve, DEGENERATE), '-d', join(work, 'd05'), '-wcs'], solve],
    ],
  );
  assert.deepEqual(readdirSync(join(work, 'd05')).sort(), ['d05_0101.1476', 'd05_0201.1476']);
});

test('without verify, a rotated archive is compared and nothing more', async (t) => {
  const site = await sourceForge(GOOD, { pin: OLD_PIN });
  t.after(site.server.close);
  const { value, runs, result } = await detectHere(site, { here: 'Linux-ARM64', verify: false });
  assert.equal(value.rotated, true);
  assert.deepEqual(result, {
    runner: RUNNER,
    archive: ENTRY.archive,
    pinned: OLD_PIN,
    upstream: site.upstream,
    banner: CROSS_MIRROR,
    verified: 'skipped',
  });
  assert.deepEqual(downloads(site), [ENTRY.archive]);
  assert.deepEqual(runs, []);
});

test("verify on another runner's archive is refused before anything is downloaded", async (t) => {
  const site = await sourceForge(GOOD, { pin: OLD_PIN });
  t.after(site.server.close);
  await assert.rejects(
    detectHere(site, { here: 'macOS-ARM64' }),
    new RegExp(`this job runs on macOS-ARM64, so it cannot run ${RUNNER}'s binary to check it`),
  );
  assert.deepEqual(site.server.requests, []);
});

test('a binary that fails a solve is a failed result with the reason, annotated, and the step goes on', async (t) => {
  const site = await sourceForge({ ...GOOD, m101: { code: 2 } }, { pin: OLD_PIN });
  t.after(site.server.close);
  const saved = process.exitCode;
  t.after(() => {
    process.exitCode = saved;
  });
  process.exitCode = undefined;
  const { stdout, result } = await detectHere(site);
  assert.equal(result.verified, 'failed');
  assert.equal(result.problem, 'M101 solve exited 2');
  assert.match(stdout, /^::error::M101 solve exited 2$/m);
  assert.equal(process.exitCode, undefined);
});

test('a database that rotated too is warned about, and the binary is checked against it', async (t) => {
  const site = await sourceForge(GOOD, { pin: OLD_PIN, database: zip([{ path: 'd05_0101.1476', data: 'a new tile' }]) });
  t.after(site.server.close);
  const { stdout, result } = await detectHere(site);
  assert.match(stdout, /^::warning::d05_star_database\.zip also rotated upstream \([0-9a-f]{64}, pinned [0-9a-f]{64}\)\. The refresh does not move that pin/m);
  assert.equal(result.verified, 'passed');
});

test('an archive without the binary at its top fails the step, as it is no release to check', async (t) => {
  const site = await sourceForge(GOOD, { pin: OLD_PIN, entry: { path: `astap/${ENTRY.binary}` } });
  t.after(site.server.close);
  await assert.rejects(detectHere(site), new RegExp(`${ENTRY.archive} holds no ${ENTRY.binary.replace('.', '\\.')} at its top`));
});

test("a binary that cannot start fails the step, and what upstream's release is stays unsaid", async (t) => {
  const site = await sourceForge(GOOD, { pin: OLD_PIN });
  t.after(site.server.close);
  const missing = (binary, args, options) => runProgram(join(tempDir(), 'no-such-program'), args, options);
  await assert.rejects(detectHere(site, { run: missing }), (err) => err.code === 'ENOENT');
});

// --- Checking a new binary ------------------------------------------------------

// solveFixtures on the fake with `scenario`, in a new directory.
async function solveWith(scenario, options = {}) {
  const dir = join(tempDir(), 'solve');
  const { value } = await capture(() =>
    solveFixtures({ binary: scenarioFile(scenario), database: tempDir(), fixtures: fixtures(), dir, run: fake, ...options }),
  );
  return value;
}

// The recorded .wcs with the value of `key`'s card replaced by `value`.
function withCard(key, value) {
  const at = RECORDED_WCS.indexOf(`${key.padEnd(8)}= `);
  assert.notEqual(at, -1, key);
  const card = `${key.padEnd(8)}= ${value.padStart(20)} / changed`.padEnd(80);
  return RECORDED_WCS.slice(0, at) + card + RECORDED_WCS.slice(at + 80);
}

test('the release the recorded .wcs came from passes', async () => {
  assert.equal(await solveWith(GOOD), undefined);
});

test('each way a solve of M101 can fail is named', async () => {
  const m101 = (field) => solveWith({ ...GOOD, m101: field });
  assert.equal(await m101({ code: 1 }), 'M101 solve exited 1');
  assert.equal(await m101({ code: 0 }), 'M101 produced no .wcs sidecar');
  assert.equal(await m101({ code: 0, wcs: RECORDED_WCS.replace('CRVAL2  =', 'CRVALX  =') }), 'M101 .wcs has no numeric CRVAL1 or CRVAL2');
  assert.equal(await m101({ code: 0, wcs: withCard('CRVAL1', "'210.81'") }), 'M101 .wcs has no numeric CRVAL1 or CRVAL2');
  assert.equal(
    await m101({ code: 0, wcs: withCard('CRVAL1', '2.108300000000E+002') }),
    'M101 RA 210.83 outside tolerance (expected 210.8099 +/- 0.01)',
  );
  assert.equal(
    await m101({ code: 0, wcs: withCard('CRVAL2', '5.433000000000E+001') }),
    'M101 Dec 54.33 outside tolerance (expected 54.3469 +/- 0.01)',
  );
});

test('a centre just inside the tolerance passes, and one just outside does not', async () => {
  const m101 = (ra) => solveWith({ ...GOOD, m101: { code: 0, wcs: withCard('CRVAL1', ra) } });
  assert.equal(await m101('210.8189'), undefined);
  assert.match(await m101('210.8201'), /^M101 RA 210\.8201 outside tolerance/);
});

test('a binary that solves the empty field, or leaves a .wcs for it, fails', async () => {
  assert.equal(await solveWith({ ...GOOD, empty: { code: 0 } }), 'degenerate_no_stars.fits unexpectedly solved');
  assert.equal(await solveWith({ ...GOOD, empty: { code: 1, wcs: RECORDED_WCS } }), 'degenerate_no_stars.fits produced a .wcs sidecar');
});

test('a solve that hangs is stopped at the timeout and fails', async () => {
  assert.equal(await solveWith({ ...GOOD, m101: { hang: true } }, { timeoutMs: 300 }), 'M101 solve did not finish in 0.3 s');
  assert.equal(await solveWith({ ...GOOD, empty: { hang: true } }, { timeoutMs: 300 }), 'degenerate_no_stars.fits solve did not finish in 0.3 s');
});

// --- Reading the .wcs ---------------------------------------------------------------

test('wcsCentre reads the centre from the .wcs the real ASTAP wrote', () => {
  assert.deepEqual(wcsCentre(RECORDED_WCS), { ra: 210.8098818046, dec: 54.346932537 });
});

test('wcsCentre reads cards, not text: a keyword in a comment, or after END, is not a card', () => {
  const comment = 'COMMENT CRVAL1  =  1.0 / not a card'.padEnd(80);
  const end = 'END'.padEnd(80);
  const crval2 = 'CRVAL2  =  5.434693253700E+001'.padEnd(80);
  assert.deepEqual(wcsCentre(comment + crval2 + end + 'CRVAL1  =  2.108098818046E+002'.padEnd(80)), { ra: undefined, dec: 54.346932537 });
});

test('wcsCentre takes FITS numbers, a D exponent too, and nothing else', () => {
  const card = (key, value) => `${key.padEnd(8)}= ${value}`.padEnd(80);
  assert.deepEqual(wcsCentre(card('CRVAL1', '2.1081D+02') + card('CRVAL2', '54') + 'END'.padEnd(80)), { ra: 210.81, dec: 54 });
  assert.deepEqual(wcsCentre(card('CRVAL1', 'T') + card('CRVAL2', "'54.3'") + 'END'.padEnd(80)), { ra: undefined, dec: undefined });
  assert.deepEqual(wcsCentre(card('CRVAL1', '') + 'END'.padEnd(80)), { ra: undefined, dec: undefined });
});

// --- Running the binary ---------------------------------------------------------------

test('runProgram puts stdout and stderr in one log, in order, and gives the exit code', async () => {
  const dir = tempDir();
  const log = join(dir, 'out');
  const script = "process.stdout.write('one\\n'); process.stderr.write('two\\n'); process.stdout.write('three\\n'); process.exit(3)";
  const result = await runProgram(process.execPath, ['-e', script], { cwd: dir, log, timeoutMs: 10_000 });
  assert.deepEqual(result, { code: 3, signal: null, timedOut: false, output: 'one\ntwo\nthree\n' });
});

test("runProgram keeps the runner's ACTIONS_* variables and the action's inputs from the binary", async () => {
  const dir = tempDir();
  const script = 'process.stdout.write(JSON.stringify(Object.keys(process.env)))';
  const vars = { ACTIONS_RUNTIME_TOKEN: 'secret', ACTIONS_CACHE_URL: 'https://cache', INPUT_RUNNER: 'Linux-X64', KEPT_FOR_ASTAP: 'yes' };
  const { output } = await withEnv(vars, () => runProgram(process.execPath, ['-e', script], { cwd: dir, log: join(dir, 'out'), timeoutMs: 10_000 }));
  const names = JSON.parse(output);
  assert.ok(names.includes('KEPT_FOR_ASTAP'));
  assert.deepEqual(
    names.filter((name) => /^(ACTIONS|INPUT)_/i.test(name)),
    [],
  );
});

test('runProgram kills a run past its timeout', async () => {
  const dir = tempDir();
  const started = Date.now();
  const result = await runProgram(process.execPath, ['-e', 'setInterval(() => {}, 1000)'], { cwd: dir, log: join(dir, 'out'), timeoutMs: 200 });
  assert.equal(result.timedOut, true);
  assert.ok(Date.now() - started < 5_000);
});

test('runProgram rejects a program that cannot start', async () => {
  const dir = tempDir();
  mkdirSync(join(dir, 'empty'));
  await assert.rejects(runProgram(join(dir, 'empty', 'astap_cli'), [], { cwd: dir, log: join(dir, 'out'), timeoutMs: 10_000 }), { code: 'ENOENT' });
});
