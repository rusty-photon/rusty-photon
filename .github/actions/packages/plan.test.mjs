import assert from 'node:assert/strict';
import { test } from 'node:test';

import { capture, serve } from '../lib/testing.mjs';
import { nightlyVersion, plan, publishedNightly, workspaceVersion } from './plan.mjs';

const HEAD = 'ba09dc9'.padEnd(40, 'a');
const OTHER = 'c'.repeat(40);
const TAG = 't'.repeat(40);
const OUTER = 'o'.repeat(40);

// The root Cargo.toml's shape: the members, then the version, then a
// dependency's own version, indented or in a table of its own.
const CARGO_TOML = [
  '[workspace]',
  'members = [',
  '  "crates/rp-fits",',
  ']',
  '',
  '[workspace.package]',
  'version = "0.1.0"',
  'edition = "2024"',
  '',
  '[workspace.dependencies.serde]',
  'version = "1.0.228"',
  '',
].join('\n');

const NOW = new Date('2026-07-12T05:07:59.999Z');

function json(res, status, value) {
  res.writeHead(status, { 'content-type': 'application/json' }).end(JSON.stringify(value));
}

// A fake GitHub API for repository o/r, where `refs` maps a ref's path under
// git/ref/ to the object it names (none: 404), as GitHub answers it, and
// `tags` maps an annotated tag's sha to the object it tags. `status`, when
// given, answers every request.
async function fakeGitHub({ refs = {}, tags = {}, status } = {}) {
  const server = await serve((req, res) => {
    const url = new URL(req.url, server.url);
    const ref = url.pathname.match(/^\/repos\/o\/r\/git\/ref\/(.+)$/)?.[1];
    const tag = url.pathname.match(/^\/repos\/o\/r\/git\/tags\/(\w+)$/)?.[1];
    if (status !== undefined) {
      json(res, status, { message: 'Bad credentials' });
    } else if (ref !== undefined && refs[ref]) {
      json(res, 200, { ref: `refs/${ref}`, object: refs[ref] });
    } else if (tag !== undefined && tags[tag]) {
      json(res, 200, { sha: tag, tag: 'nightly', object: tags[tag] });
    } else {
      json(res, 404, { message: 'Not Found', status: '404' });
    }
  });
  return server;
}

const commit = (sha) => ({ sha, type: 'commit' });
const annotated = (sha) => ({ sha, type: 'tag' });
const paths = (api) => api.requests.map((r) => `${r.method} ${r.url}`);

function planWith(api, options = {}) {
  return capture(() => plan({ api: api.url, repo: 'o/r', token: 'tok', head: HEAD, cargoToml: CARGO_TOML, now: NOW, delayMs: 1, ...options }));
}

// --- The version -------------------------------------------------------------------

test('the workspace version is the first version line in the first column', () => {
  assert.equal(workspaceVersion(CARGO_TOML), '0.1.0');
  assert.equal(workspaceVersion('[package]\n  version = "9.9.9"\nversion = "0.2.0"\n'), '0.2.0');
  assert.equal(workspaceVersion('[workspace.package]\r\nversion = "0.3.0"\r\n'), '0.3.0');
});

test('a Cargo.toml with no version line is an error', () => {
  for (const text of ['[workspace]\nmembers = []\n', 'version="0.1.0"\n', 'version = 0.1.0\n']) {
    assert.throws(() => workspaceVersion(text), /^Error: could not read the workspace version from Cargo\.toml$/);
  }
});

test('the nightly version stamps the UTC minute, truncated, and the short commit', () => {
  assert.deepEqual(nightlyVersion({ base: '0.1.0', now: NOW, short: 'ba09dc9' }), {
    date: '20260712',
    version: '0.1.0+nightly.202607120507.gba09dc9',
  });
  // A time zone the runner's clock might be in does not move it.
  assert.deepEqual(nightlyVersion({ base: '1.2.3', now: new Date('2026-12-31T23:59:00-02:00'), short: 'abcdef0' }), {
    date: '20270101',
    version: '1.2.3+nightly.202701010159.gabcdef0',
  });
});

// --- The nightly tag ---------------------------------------------------------------------

test('a lightweight nightly tag names its commit', async (t) => {
  const api = await fakeGitHub({ refs: { 'tags/nightly': commit(OTHER) } });
  t.after(api.close);
  assert.equal(await publishedNightly({ api: api.url, repo: 'o/r', token: 'tok' }), OTHER);
  assert.deepEqual(paths(api), ['GET /repos/o/r/git/ref/tags/nightly']);
  assert.equal(api.requests[0].headers.authorization, 'Bearer tok');
});

test('an annotated nightly tag is peeled to its commit, through a tag of a tag', async (t) => {
  const api = await fakeGitHub({
    refs: { 'tags/nightly': annotated(OUTER) },
    tags: { [OUTER]: annotated(TAG), [TAG]: commit(OTHER) },
  });
  t.after(api.close);
  assert.equal(await publishedNightly({ api: api.url, repo: 'o/r', token: 'tok' }), OTHER);
  assert.deepEqual(paths(api), ['GET /repos/o/r/git/ref/tags/nightly', `GET /repos/o/r/git/tags/${OUTER}`, `GET /repos/o/r/git/tags/${TAG}`]);
});

test('no nightly tag is undefined', async (t) => {
  const api = await fakeGitHub();
  t.after(api.close);
  assert.equal(await publishedNightly({ api: api.url, repo: 'o/r', token: 'tok' }), undefined);
});

// --- The plan --------------------------------------------------------------------------

test('a nightly tag on the checked-out commit skips, and reads no version', async (t) => {
  const api = await fakeGitHub({ refs: { 'tags/nightly': commit(HEAD) } });
  t.after(api.close);
  const { value, stdout } = await planWith(api, { cargoToml: '' });
  assert.deepEqual(value, { skip: 'true', sha: HEAD });
  assert.equal(stdout, `The nightly tag is already at ${HEAD}; nothing to do.\n`);
});

test('an annotated nightly tag on the checked-out commit skips', async (t) => {
  const api = await fakeGitHub({ refs: { 'tags/nightly': annotated(TAG) }, tags: { [TAG]: commit(HEAD) } });
  t.after(api.close);
  assert.equal((await planWith(api)).value.skip, 'true');
});

test('a nightly tag on another commit publishes this one, with its version', async (t) => {
  const api = await fakeGitHub({ refs: { 'tags/nightly': commit(OTHER) } });
  t.after(api.close);
  const { value, stdout } = await planWith(api);
  assert.deepEqual(value, { skip: 'false', sha: HEAD, date: '20260712', nightly_version: '0.1.0+nightly.202607120507.gba09dc9' });
  assert.equal(stdout, `Publishing ${HEAD} (nightly tag: ${OTHER}).\nNightly version: 0.1.0+nightly.202607120507.gba09dc9\n`);
});

test('no nightly tag publishes, as the first nightly', async (t) => {
  const api = await fakeGitHub();
  t.after(api.close);
  const { value, stdout } = await planWith(api);
  assert.equal(value.skip, 'false');
  assert.match(stdout, /^Publishing \w+ \(nightly tag: none\)\.\n/);
});

test('a tag that cannot be read fails, rather than publish or skip', async (t) => {
  const api = await fakeGitHub({ status: 401 });
  t.after(api.close);
  await assert.rejects(planWith(api), /^Error: Reading the nightly tag failed \(GitHub answered: .*Bad credentials/);
});

test('a version that cannot be read fails the publish', async (t) => {
  const api = await fakeGitHub({ refs: { 'tags/nightly': commit(OTHER) } });
  t.after(api.close);
  await assert.rejects(planWith(api, { cargoToml: '[workspace]\n' }), /could not read the workspace version/);
});
