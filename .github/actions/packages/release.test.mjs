import assert from 'node:assert/strict';
import { test } from 'node:test';

import { capture } from '../lib/testing.mjs';
import { checkTag } from './release.mjs';

const CARGO_TOML = '[workspace]\nmembers = []\n\n[workspace.package]\nversion = "0.2.0"\n';

test('a tag naming the workspace version passes, and its version is the tag without its v', async () => {
  const { value, stdout } = await capture(() => checkTag('v0.2.0', CARGO_TOML));
  assert.equal(value, '0.2.0');
  assert.equal(stdout, "The tag v0.2.0 names the workspace's version, 0.2.0.\n");
});

test('a Cargo.toml checked out with CRLF line ends, as Windows checks it out, reads the same', () => {
  assert.equal(checkTag('v0.2.0', CARGO_TOML.replaceAll('\n', '\r\n')), '0.2.0');
});

test('a tag naming another version fails', () => {
  for (const [tag, named] of [['v0.2.1', '0.2.1'], ['v0.2.0-rc1', '0.2.0-rc1'], ['vv0.2.0', 'v0.2.0'], ['0.1.9', '0.1.9']]) {
    assert.throws(() => checkTag(tag, CARGO_TOML), new RegExp(`^Error: the tag ${tag} names the version ${named}, but the workspace's is 0\\.2\\.0$`));
  }
});

test('a Cargo.toml with no version fails', () => {
  assert.throws(() => checkTag('v0.2.0', '[workspace]\n'), /could not read the workspace version from Cargo\.toml/);
});
