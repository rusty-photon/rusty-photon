// Every JavaScript file under .github/actions must parse. node_syntax_test in
// node_test.bzl puts all of them in this test's runfiles, and says why.

import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { readdirSync } from 'node:fs';
import { join } from 'node:path';
import test from 'node:test';

// The test runs in the main repository's directory in the runfiles tree.
const ROOT = join('.github', 'actions');

const files = readdirSync(ROOT, { recursive: true })
  .filter((file) => /\.[cm]?js$/.test(file))
  .sort();

test('the check sees the files it is meant to', () => {
  // A floor, not the list: an empty or partial `data` glob would otherwise
  // pass by checking nothing.
  for (const known of [join('lib', 'helpers.mjs'), join('lib', 'helpers.test.mjs'), 'syntax.test.mjs']) {
    assert.ok(files.includes(known), `${known} is missing from ${JSON.stringify(files)}`);
  }
});

for (const file of files) {
  test(`${file} parses`, () => {
    const result = spawnSync(process.execPath, ['--check', join(ROOT, file)], { encoding: 'utf8' });
    assert.equal(result.status, 0, result.stderr || result.error?.message);
  });
}
