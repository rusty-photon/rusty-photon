import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { mkdirSync, mkdtempSync, readFileSync, readdirSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { test } from 'node:test';

import { capture } from '../lib/testing.mjs';
import { KINDS, SUMS, dotRpmNames, mediaType, readSums, writeSums } from './assets.mjs';

function tempDir() {
  return mkdtempSync(path.join(process.env.TEST_TMPDIR ?? tmpdir(), 'assets-'));
}

const sha = (text) => createHash('sha256').update(text).digest('hex');

// A directory holding `files`, each with its own name as its content.
function assetsDir(files) {
  const dir = tempDir();
  for (const name of files) writeFileSync(path.join(dir, name), name);
  return dir;
}

// One of each kind, as the legs name them, with a second deb and rpm that
// sort first.
const LEGS = [
  'rusty-photon-rp_0.1.0+nightly.202607120507.gba09dc9_amd64.deb',
  'rusty-photon-dsd-fp2_0.1.0+nightly.202607120507.gba09dc9_arm64.deb',
  'rusty-photon-rp-0.1.0^202607120507.gba09dc9-1.x86_64.rpm',
  'rusty-photon-dsd-fp2-0.1.0^202607120507.gba09dc9-1.aarch64.rpm',
  'rusty-photon-0.1.0+nightly.202607120507.gba09dc9-x64.msi',
  'rusty-photon-rp-0.1.0+nightly.202607120507.gba09dc9-aarch64-apple-darwin.tar.gz',
];

// --- The rpms' names ------------------------------------------------------------------

test('an rpm’s ^ becomes a dot in its name, and nothing else is renamed', async () => {
  const dir = assetsDir([...LEGS, 'odd^name.deb', 'two^carets^.rpm']);
  const { value, stdout } = await capture(() => dotRpmNames(dir));
  assert.deepEqual(value, [
    'rusty-photon-dsd-fp2-0.1.0.202607120507.gba09dc9-1.aarch64.rpm',
    'rusty-photon-rp-0.1.0.202607120507.gba09dc9-1.x86_64.rpm',
    'two.carets..rpm',
  ]);
  assert.equal(stdout, `${value.join('\n')}\n`);
  assert.ok(readdirSync(dir).includes('odd^name.deb'));
  // The bytes stay with the file.
  assert.equal(
    readFileSync(path.join(dir, 'rusty-photon-rp-0.1.0.202607120507.gba09dc9-1.x86_64.rpm'), 'utf8'),
    'rusty-photon-rp-0.1.0^202607120507.gba09dc9-1.x86_64.rpm',
  );
});

// --- The sums -----------------------------------------------------------------------

test('the sums list each kind in turn, by name within it, in sha256sum’s format', async () => {
  const dir = assetsDir([...LEGS, 'notes.txt', 'formula.rb', 'archive.gz']);
  mkdirSync(path.join(dir, 'a-directory.deb'));
  const { value, stdout } = await capture(() => writeSums(dir));
  const order = [LEGS[1], LEGS[0], LEGS[3], LEGS[2], LEGS[4], LEGS[5]];
  assert.deepEqual(
    value,
    order.map((name) => ({ name, sha256: sha(name) })),
  );
  const text = order.map((name) => `${sha(name)}  ${name}\n`).join('');
  assert.equal(readFileSync(path.join(dir, SUMS), 'utf8'), text);
  assert.equal(stdout, text);
});

test('a kind with no asset fails the sums, and none is written', async () => {
  for (const [extension] of KINDS) {
    const dir = assetsDir(LEGS.filter((name) => !name.endsWith(extension)));
    await assert.rejects(capture(() => writeSums(dir)), new RegExp(`holds no \\*${extension.replaceAll('.', '\\.')} asset$`));
    assert.equal(readdirSync(dir).includes(SUMS), false);
  }
});

test('the sums read back as they were written', async () => {
  const dir = assetsDir(LEGS);
  const { value } = await capture(() => writeSums(dir));
  assert.deepEqual(await readSums(dir), value);
});

test('a sums line that is no sum and name is refused', async () => {
  for (const line of [`${'a'.repeat(64)} one-space.deb`, `${'A'.repeat(64)}  upper.deb`, 'garbage']) {
    const dir = tempDir();
    writeFileSync(path.join(dir, SUMS), `${line}\n`);
    await assert.rejects(readSums(dir), /^Error: SHA256SUMS\.txt has a line that is no sum and name: /);
  }
});

// --- The media types ----------------------------------------------------------------

test('each asset goes up as gh uploaded it', () => {
  assert.equal(mediaType(LEGS[0]), 'application/x-debian-package');
  assert.equal(mediaType(LEGS[2]), 'application/x-rpm');
  assert.equal(mediaType(LEGS[4]), 'application/x-msi');
  assert.equal(mediaType(LEGS[5]), 'application/x-gtar');
  assert.equal(mediaType(SUMS), 'text/plain; charset=utf-8');
  assert.throws(() => mediaType('notes.txt'), /^Error: notes\.txt is no kind of asset the release carries$/);
});
