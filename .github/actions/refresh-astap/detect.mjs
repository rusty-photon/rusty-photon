// The first job of the refresh of install-astap's pins (ADR-005 §"Pinned
// SHA-256 + refresh procedure"), which refresh-astap-shas.yml runs when the
// nightly install-astap smoke fails, the sign that SourceForge serves a new
// release under the old names. Plan item J8 in docs/plans/ci-shell-to-code.md.
//
// One job per runner downloads the archive the table names for that runner
// and compares its SHA-256 with the pin. When they differ and the job runs on
// that runner, it checks the new binary by solving plate-solver's M101 fixture
// to its known centre, and by failing to solve a field with no stars, and
// writes what it found for upload-artifact; publish.mjs acts on it.
// Windows-ARM64 has no hosted runner, so a Linux job only compares its
// archive.

import { spawn } from 'node:child_process';
import { closeSync, existsSync, openSync } from 'node:fs';
import { copyFile, mkdir, readFile, writeFile } from 'node:fs/promises';
import path from 'node:path';

import { CLI_DOWNLOAD, DATABASE_DOWNLOAD, resolve, SERVER, TABLE } from '../install-astap/astap.mjs';
import { error, io, warning, withoutRunnerVariables } from '../lib/helpers.mjs';
import { download, makeRunnable, unpack } from '../lib/install.mjs';

// --- Checking a new binary ------------------------------------------------------

/** Where the checkout holds the fixtures a new binary must solve, and fail to. */
export const FIXTURES = path.join('services', 'plate-solver', 'tests', 'fixtures');

/**
 * The field a new binary must solve: M101, to within `tolerance` degrees of
 * its centre, as plate-solver's real_astap_smoke.feature asks. The hints keep
 * the solve under a second; without them ASTAP searches the whole sky, since
 * the fixture carries no pointing. They are the feature's in ASTAP's units:
 * `-ra` in hours (210.8099 / 15), `-spd` the south polar distance (90 +
 * 54.3469), `-fov` and `-r` in degrees, as plate-solver's runner/astap.rs
 * converts them.
 */
export const M101 = {
  file: 'm101_known.fits',
  ra: 210.8099,
  dec: 54.3469,
  tolerance: 0.01,
  hints: ['-ra', '14.0540', '-spd', '144.3469', '-fov', '0.42', '-r', '3.0'],
};

/** A field with no stars, which a new binary must fail to solve, so a failure still reads as one. */
export const DEGENERATE = 'degenerate_no_stars.fits';

/**
 * How long a solve may take. ASTAP solves M101 with its hints in under a
 * second and gives up on the empty field in about as long, so a run past this
 * hangs, and the binary fails its check.
 */
export const SOLVE_TIMEOUT_MS = 120_000;

/** What a runner without its own job reports in place of a check. */
export const CROSS_MIRROR = 'cross-mirror SHA only (no runner, no verification)';

/**
 * Runs `binary` with `args` in `cwd`, with this process's environment
 * without the runner's own variables, its stdout and stderr both into `log`,
 * as `2>&1` would. Resolves to how it ended, `{ code, signal, timedOut,
 * output }`, a run past `timeoutMs` killed; rejects when it cannot start.
 */
export function runProgram(binary, args, { cwd, log, timeoutMs }) {
  const out = openSync(log, 'w');
  let child;
  try {
    // A binary being checked is one upstream released and nobody has looked
    // at yet. That is no sandbox: this process keeps the runner's variables,
    // and on Linux a process can read its parent's environment
    // (/proc/<ppid>/environ); a hosted runner's user also has passwordless
    // sudo. The job holds no token besides, with contents: read and no
    // credentials left by checkout.
    child = spawn(binary, args, { cwd, env: withoutRunnerVariables(process.env), stdio: ['ignore', out, out], windowsHide: true });
  } finally {
    closeSync(out);
  }
  return new Promise((done, failed) => {
    let notStarted;
    let timedOut = false;
    const timer = setTimeout(() => {
      timedOut = true;
      child.kill('SIGKILL');
    }, timeoutMs);
    // Node still emits 'close' after a child that could not start.
    child.on('error', (err) => {
      notStarted ??= err;
    });
    child.on('close', (code, signal) => {
      clearTimeout(timer);
      if (notStarted) failed(notStarted);
      else readFile(log, 'utf8').then((output) => done({ code, signal, timedOut, output }), failed);
    });
  });
}

const FLOAT = /^[+-]?(\d+\.?\d*|\.\d+)([EeDd][+-]?\d+)?$/;

/**
 * The centre of a solve, CRVAL1 and CRVAL2, from the `.wcs` ASTAP writes
 * beside the FITS it solved: a FITS header, 80-character cards with no line
 * breaks. Each is undefined when its card is missing or holds no number.
 * These are the two cards plate-solver's own reader (runner/wcs.rs) cannot do
 * without.
 */
export function wcsCentre(text) {
  const values = {};
  for (let at = 0; at < text.length; at += 80) {
    const card = text.slice(at, at + 80);
    const key = card.slice(0, 8).trim();
    if (key === 'END') break;
    if (card.slice(8, 10) === '= ') values[key] ??= card.slice(10).split('/')[0].trim();
  }
  const number = (value) => (value !== undefined && FLOAT.test(value) ? Number(value.replace(/[Dd]/, 'e')) : undefined);
  return { ra: number(values.CRVAL1), dec: number(values.CRVAL2) };
}

// How a run ended, for a message.
function ending({ code, signal, timedOut }, timeoutMs) {
  if (timedOut) return `did not finish in ${timeoutMs / 1000} s`;
  return signal ? `was stopped by ${signal}` : `exited ${code}`;
}

// Runs one solve and prints what ASTAP said, and how it ended.
async function solveOnce(binary, args, { dir, run, timeoutMs }) {
  const log = path.join(dir, `${path.basename(args[1])}.output`);
  const result = await run(binary, args, { cwd: dir, log, timeoutMs });
  const how = ending(result, timeoutMs);
  io.stdout(`${path.basename(binary)} ${args.join(' ')}: ${how}\n${result.output.trimEnd()}\n`);
  return { ...result, how };
}

/**
 * Whether `binary`, with the star database in `database`, solves M101 to its
 * centre and fails on the empty field, in a copy of both fixtures from
 * `fixtures` in `dir`: undefined when it does, or what went wrong. Only the
 * signals plate-solver reads are checked, its exit code, the `.wcs` and the
 * centre in it, so a change in anything else ASTAP writes does not fail it.
 */
export async function solveFixtures({ binary, database, fixtures, dir, run = runProgram, timeoutMs = SOLVE_TIMEOUT_MS }) {
  await mkdir(dir, { recursive: true });
  for (const file of [M101.file, DEGENERATE]) await copyFile(path.join(fixtures, file), path.join(dir, file));
  const solve = { dir, run, timeoutMs };

  const m101 = path.join(dir, M101.file);
  const solved = await solveOnce(binary, ['-f', m101, '-d', database, '-wcs', ...M101.hints], solve);
  if (solved.timedOut || solved.code !== 0) return `M101 solve ${solved.how}`;
  const wcs = m101.replace(/\.fits$/, '.wcs');
  if (!existsSync(wcs)) return 'M101 produced no .wcs sidecar';
  const { ra, dec } = wcsCentre(await readFile(wcs, 'latin1'));
  if (ra === undefined || dec === undefined) return 'M101 .wcs has no numeric CRVAL1 or CRVAL2';
  if (Math.abs(ra - M101.ra) > M101.tolerance) return `M101 RA ${ra} outside tolerance (expected ${M101.ra} +/- ${M101.tolerance})`;
  if (Math.abs(dec - M101.dec) > M101.tolerance) return `M101 Dec ${dec} outside tolerance (expected ${M101.dec} +/- ${M101.tolerance})`;

  const empty = path.join(dir, DEGENERATE);
  const unsolved = await solveOnce(binary, ['-f', empty, '-d', database, '-wcs'], solve);
  if (unsolved.timedOut) return `${DEGENERATE} solve ${unsolved.how}`;
  if (unsolved.code === 0) return `${DEGENERATE} unexpectedly solved`;
  if (existsSync(empty.replace(/\.fits$/, '.wcs'))) return `${DEGENERATE} produced a .wcs sidecar`;

  io.stdout(`M101 verified: RA ${ra}, Dec ${dec}; ${DEGENERATE} failed as expected\n`);
  return undefined;
}

// The new binary's banner, the first line it prints when run with no
// arguments: "ASTAP astrometric solver version CLI-2026.09.01".
async function banner(binary, dir, run) {
  const { output } = await run(binary, [], { cwd: dir, log: path.join(dir, 'banner.output'), timeoutMs: SOLVE_TIMEOUT_MS });
  return output.split('\n')[0].replace(/\r/g, '');
}

// Unpacks the CLI archive `file` and checks the binary in it: its banner,
// then the solves, with the database `database` serves, which it downloads.
// What cannot be downloaded, unpacked or started throws, as a broken mirror
// can cause; a binary that runs and fails a solve is reported, not thrown.
async function check({ file, cli, database, work, fixtures, run, options }) {
  const dir = path.join(work, 'astap');
  await unpack(file, dir);
  if (!existsSync(path.join(dir, cli.binary))) throw new Error(`${cli.name} holds no ${cli.binary} at its top`);
  await makeRunnable(dir, cli.binary);
  const binary = path.join(dir, cli.binary);
  const version = await banner(binary, work, run);
  io.stdout(`Banner: ${version}\n`);

  // Each run downloads the database: this workflow runs once a month or so,
  // and a cache entry goes after a week unused.
  const zip = path.join(work, database.name);
  const got = await download({ ...database, pin: { rolling: true } }, zip, { ...DATABASE_DOWNLOAD, ...options });
  if (got !== database.pin.sha256) {
    warning(
      `${database.name} also rotated upstream (${got}, pinned ${database.pin.sha256}). The refresh does not move that pin: ` +
        'ADR-005 has the manual steps. The new CLI is checked against the new database.',
    );
  }
  const stars = path.join(work, 'd05');
  await unpack(zip, stars);

  const problem = await solveFixtures({ binary, database: stars, fixtures, dir: path.join(work, 'solve'), run });
  if (problem !== undefined) {
    error(problem);
    return { banner: version, verified: 'failed', problem };
  }
  return { banner: version, verified: 'passed' };
}

// --- Comparing with the pin ---------------------------------------------------------

/**
 * Compares the CLI archive SourceForge serves for `runner` with its pin in
 * `table`, in the empty directory `work`. When it rotated, writes the result
 * publish reads into `results` as `<runner>.json`: `{ runner, archive,
 * pinned, upstream, banner, verified }`, `verified` being 'passed' or
 * 'failed' (with a `problem`) when `verify` asked for the check, which only
 * the runner `here` can run on its own archive, and 'skipped' otherwise.
 * Resolves to whether it rotated and upstream's SHA-256.
 */
export async function detect({
  runner,
  verify,
  here,
  work,
  results,
  fixtures,
  table = TABLE,
  server = SERVER,
  run = runProgram,
  download: options,
}) {
  if (verify && runner !== here) {
    throw new Error(`this job runs on ${here}, so it cannot run ${runner}'s binary to check it: give verify false, or run it on ${runner}`);
  }
  const { cli, database } = resolve({ runner, table, server });
  const pinned = cli.pin.sha256;
  const file = path.join(work, cli.name);
  const upstream = await download({ ...cli, pin: { rolling: true } }, file, { ...CLI_DOWNLOAD, ...options });
  if (upstream === pinned) {
    io.stdout(`No rotation: ${cli.name} is still the pinned ${pinned}\n`);
    return { rotated: false, upstream };
  }
  io.stdout(`Rotation: ${cli.name} is ${upstream}, pinned ${pinned}\n`);
  const found = verify
    ? await check({ file, cli, database, work, fixtures, run, options })
    : { banner: CROSS_MIRROR, verified: 'skipped' };
  const result = { runner, archive: cli.name, pinned, upstream, ...found };
  await mkdir(results, { recursive: true });
  await writeFile(path.join(results, `${runner}.json`), `${JSON.stringify(result, null, 2)}\n`);
  return { rotated: true, upstream };
}
