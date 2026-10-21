import assert from 'node:assert/strict';
import { mkdirSync, mkdtempSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { test } from 'node:test';

import { capture } from '../lib/testing.mjs';
import { BLIND_MAX_MS, HINTED_MAX_MS, MIN_BLIND_HINTED_RATIO, overBudget, parseTimes, report } from './times.mjs';

// --- Fixtures -----------------------------------------------------------------

// A CSV in the shape the BDD step writes: a header, then a row per solve.
const CSV = 'label,elapsed_ms\nm101_hinted,712\nm101_blind,80123\n';

function tempDir() {
  const base = process.env.TEST_TMPDIR ?? path.join(process.cwd(), '.tmp');
  mkdirSync(base, { recursive: true });
  return mkdtempSync(path.join(base, 'plate-solver-times-'));
}

function csvFile(text) {
  const file = path.join(tempDir(), 'plate-solver-perf.csv');
  writeFileSync(file, text);
  return file;
}

const LOGGED =
  '::group::M 101 hinted-vs-blind timing (ubuntu-latest)\n' +
  CSV +
  '::endgroup::\n' +
  '::notice::plate-solver perf [ubuntu-latest] m101_hinted = 712 ms\n' +
  '::notice::plate-solver perf [ubuntu-latest] m101_blind = 80123 ms\n';

// --- parseTimes ---------------------------------------------------------------

test('parseTimes reads each row after the header, trimming a Windows line end', () => {
  assert.deepEqual(parseTimes('label,elapsed_ms\r\nm101_hinted,712\r\nm101_blind,80123\r\n'), [
    { label: 'm101_hinted', ms: '712' },
    { label: 'm101_blind', ms: '80123' },
  ]);
});

test('parseTimes keeps a last row with no line end, and skips blank lines', () => {
  assert.deepEqual(parseTimes('label,elapsed_ms\n\nm101_hinted,712\nm101_blind,80123'), [
    { label: 'm101_hinted', ms: '712' },
    { label: 'm101_blind', ms: '80123' },
  ]);
});

// --- overBudget ---------------------------------------------------------------

test('the budget is the one its baselines set', () => {
  assert.deepEqual([MIN_BLIND_HINTED_RATIO, HINTED_MAX_MS, BLIND_MAX_MS], [10, 5000, 180_000]);
});

test('overBudget passes times inside the budget, at its edges included', () => {
  assert.deepEqual(overBudget({ hinted: 712, blind: 80123 }, 'ubuntu-latest'), []);
  assert.deepEqual(overBudget({ hinted: 5000, blind: 50_000 }, 'ubuntu-latest'), []);
  assert.deepEqual(overBudget({ hinted: 100, blind: 180_000 }, 'ubuntu-latest'), []);
});

test('overBudget names a hinted solve that is under ten times faster than the blind one', () => {
  assert.deepEqual(overBudget({ hinted: 9000, blind: 89_999 }, 'macos-latest'), [
    'hint speedup regressed on macos-latest: blind=89999ms is only 9× hinted=9000ms (need ≥ 10×). ' +
      'Most likely a dropped hint flag in services/plate-solver/src/runner/astap.rs.',
    'hinted solve too slow on macos-latest: 9000ms exceeds 5000ms ceiling.',
  ]);
});

test('overBudget names each ceiling a solve exceeds', () => {
  assert.deepEqual(overBudget({ hinted: 5001, blind: 180_001 }, 'windows-latest'), [
    'hinted solve too slow on windows-latest: 5001ms exceeds 5000ms ceiling.',
    'blind solve too slow on windows-latest: 180001ms exceeds 180000ms ceiling.',
  ]);
});

// --- report -------------------------------------------------------------------

test('report logs the CSV and a notice per row, and says the budget held', async () => {
  const { stdout } = await capture(() => report({ csv: csvFile(CSV), os: 'ubuntu-latest', check: true }));
  assert.equal(
    stdout,
    LOGGED + '::notice::plate-solver budget OK [ubuntu-latest]: hinted=712ms blind=80123ms ratio=112× (need ≥10×, hinted≤5000ms, blind≤180000ms)\n',
  );
});

test('report without check logs the times and checks nothing, a broken budget included', async () => {
  const { stdout } = await capture(() => report({ csv: csvFile(CSV), os: 'ubuntu-latest', check: false }));
  assert.equal(stdout, LOGGED);
  const slow = 'label,elapsed_ms\nm101_hinted,9000\n';
  await capture(() => report({ csv: csvFile(slow), os: 'ubuntu-latest', check: false }));
});

test('report fails a broken budget with an error annotation each why', async () => {
  const csv = csvFile('label,elapsed_ms\nm101_hinted,6000\nm101_blind,30000\n');
  const { stdout } = await capture(() =>
    assert.rejects(report({ csv, os: 'ubuntu-latest', check: true }), {
      message: 'plate-solver solve-time budget exceeded on ubuntu-latest: see the annotations above.',
    }),
  );
  assert.match(stdout, /::error::hint speedup regressed on ubuntu-latest: blind=30000ms is only 5× hinted=6000ms/);
  assert.match(stdout, /::error::hinted solve too slow on ubuntu-latest: 6000ms exceeds 5000ms ceiling\.\n$/);
});

test('report takes the last row of a label, as a re-run scenario appends one', async () => {
  const csv = csvFile(`${CSV}m101_hinted,60000\n`);
  await capture(() => assert.rejects(report({ csv, os: 'ubuntu-latest', check: true }), /budget exceeded/));
});

test('report fails a CSV that lacks a positive M 101 pair, when it checks', async () => {
  for (const text of [
    'label,elapsed_ms\nm101_hinted,712\n',
    'label,elapsed_ms\nm101_hinted,0\nm101_blind,80123\n',
    'label,elapsed_ms\nm101_hinted,7.5\nm101_blind,80123\n',
    'label,elapsed_ms\nm101_hinted,\nm101_blind,80123\n',
    'label,elapsed_ms\n',
  ]) {
    await capture(() =>
      assert.rejects(report({ csv: csvFile(text), os: 'ubuntu-latest', check: true }), {
        message: 'The timing CSV lacks a positive m101_hinted or m101_blind row, so the solve-time budget cannot be checked.',
      }),
    );
  }
});

test('report warns of a missing CSV, and fails on it only when it checks', async () => {
  const csv = path.join(tempDir(), 'plate-solver-perf.csv');
  const warned = `::warning::No timing CSV at ${csv}: the BDD run recorded no solve times.\n`;
  const logged = await capture(() => report({ csv, os: 'ubuntu-latest', check: false }));
  assert.equal(logged.stdout, warned);
  const checked = await capture(() =>
    assert.rejects(report({ csv, os: 'ubuntu-latest', check: true }), {
      message: `No timing CSV at ${csv}, so the solve-time budget cannot be checked.`,
    }),
  );
  assert.equal(checked.stdout, warned);
});

test('report throws a CSV it cannot read for another reason', async () => {
  const denied = Object.assign(new Error('EACCES: permission denied'), { code: 'EACCES' });
  const readFile = async () => {
    throw denied;
  };
  await assert.rejects(report({ csv: 'x.csv', os: 'ubuntu-latest', check: false, readFile }), denied);
});
