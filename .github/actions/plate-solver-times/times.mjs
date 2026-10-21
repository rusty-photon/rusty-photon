// plate-solver-smoke.yml's solve times: the CSV that the BDD step "I record
// the solve duration as ..." appends a row to (label, elapsed_ms), put in the
// log with a notice per row, and a coarse budget on the M 101 pair, the
// hinted solve and the blind one. Each leg checks its own CSV. Conventions:
// docs/skills/ci-logic.md.

import { readFile as fsReadFile } from 'node:fs/promises';

import { error, group, io, notice, warning } from '../lib/helpers.mjs';

// The budget. Two independent checks:
//
//   1. Ratio: the blind solve must stay at least MIN_BLIND_HINTED_RATIO
//      times slower than the hinted one. If the argv mapping in
//      services/plate-solver/src/runner/astap.rs drops a hint flag, or
//      ASTAP's `-r` search-radius optimisation regresses, the hinted solve
//      collapses toward the blind spiral's time and the ratio craters. The
//      ratio is OS-independent and self-normalising, so it tolerates slow or
//      contended runners without false alarms.
//   2. Ceilings catch a uniform slowdown the ratio would miss (a slower
//      ASTAP all round) and gross integration drift.
//
// Deliberately coarse. Seen over 2026-05-21..23 (7 runs, hosted runners):
//   hinted: ubuntu 674–822 ms · macos 134–276 ms · windows 285–453 ms
//   blind:  ubuntu 76.1–85.6 s · macos 51.5–68.6 s · windows 57.6–62.6 s
//   blind/hinted: 93×–408× (the worst leg, ubuntu, ≈ 93×)
// 10× leaves about 9× headroom on the ratio; the ceilings sit about 6×
// (hinted) and 2.1× (blind) above the worst seen, and the blind ceiling
// stays under the scenario's 300 s request timeout.
export const MIN_BLIND_HINTED_RATIO = 10;
export const HINTED_MAX_MS = 5000;
export const BLIND_MAX_MS = 180_000;

/** The CSV's rows as `{ label, ms }`, without its header, each time trimmed. */
export function parseTimes(text) {
  const rows = [];
  for (const line of text.split('\n')) {
    if (line.trim() === '') continue;
    const comma = line.indexOf(',');
    const label = comma < 0 ? line : line.slice(0, comma);
    if (label === 'label') continue;
    rows.push({ label, ms: comma < 0 ? '' : line.slice(comma + 1).trim() });
  }
  return rows;
}

/**
 * Why the M 101 times `hinted` and `blind`, in ms, break the budget on `os`,
 * one sentence each; none when they keep it.
 */
export function overBudget({ hinted, blind }, os) {
  const problems = [];
  if (blind < MIN_BLIND_HINTED_RATIO * hinted) {
    problems.push(
      `hint speedup regressed on ${os}: blind=${blind}ms is only ${Math.floor(blind / hinted)}× hinted=${hinted}ms ` +
        `(need ≥ ${MIN_BLIND_HINTED_RATIO}×). Most likely a dropped hint flag in services/plate-solver/src/runner/astap.rs.`,
    );
  }
  if (hinted > HINTED_MAX_MS) problems.push(`hinted solve too slow on ${os}: ${hinted}ms exceeds ${HINTED_MAX_MS}ms ceiling.`);
  if (blind > BLIND_MAX_MS) problems.push(`blind solve too slow on ${os}: ${blind}ms exceeds ${BLIND_MAX_MS}ms ceiling.`);
  return problems;
}

// The last row's time for `label`, as a whole number of ms over 0, or
// undefined.
function timeOf(rows, label) {
  const ms = rows.findLast((row) => row.label === label)?.ms;
  return /^\d+$/.test(ms ?? '') && Number(ms) > 0 ? Number(ms) : undefined;
}

/**
 * Puts the CSV at `csv` in the log, a notice per row, and with `check`
 * checks the budget, throwing when the CSV is missing, lacks the M 101 pair,
 * or breaks the budget (an error annotation each why). A missing CSV is a
 * warning when nothing is checked.
 */
export async function report({ csv, os, check, readFile = fsReadFile }) {
  let text;
  try {
    text = await readFile(csv, 'utf8');
  } catch (err) {
    if (err.code !== 'ENOENT') throw err;
    warning(`No timing CSV at ${csv}: the BDD run recorded no solve times.`);
    if (check) throw new Error(`No timing CSV at ${csv}, so the solve-time budget cannot be checked.`);
    return;
  }
  const rows = parseTimes(text);
  await group(`M 101 hinted-vs-blind timing (${os})`, () => io.stdout(text.endsWith('\n') ? text : `${text}\n`));
  for (const { label, ms } of rows) notice(`plate-solver perf [${os}] ${label} = ${ms} ms`);
  if (!check) return;

  const times = { hinted: timeOf(rows, 'm101_hinted'), blind: timeOf(rows, 'm101_blind') };
  if (times.hinted === undefined || times.blind === undefined) {
    throw new Error(`The timing CSV lacks a positive m101_hinted or m101_blind row, so the solve-time budget cannot be checked.`);
  }
  const problems = overBudget(times, os);
  if (problems.length > 0) {
    for (const problem of problems) error(problem);
    throw new Error(`plate-solver solve-time budget exceeded on ${os}: see the annotations above.`);
  }
  notice(
    `plate-solver budget OK [${os}]: hinted=${times.hinted}ms blind=${times.blind}ms ratio=${Math.floor(times.blind / times.hinted)}× ` +
      `(need ≥${MIN_BLIND_HINTED_RATIO}×, hinted≤${HINTED_MAX_MS}ms, blind≤${BLIND_MAX_MS}ms)`,
  );
}
