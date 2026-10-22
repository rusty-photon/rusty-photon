import assert from 'node:assert/strict';
import { mkdtempSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { test } from 'node:test';

import { capture, serve } from '../lib/testing.mjs';
import { SUMS, writeSums } from './assets.mjs';
import { TAG, TITLE, moveTag, publishNightly, publishRelease, releaseBody } from './publish.mjs';

function tempDir() {
  return mkdtempSync(path.join(process.env.TEST_TMPDIR ?? tmpdir(), 'publish-'));
}

const SHA = 'ba09dc9'.padEnd(40, 'a');
const OLD = 'c'.repeat(40);
const VERSION = '0.1.0+nightly.202607120507.gba09dc9';

function json(res, status, value) {
  res.writeHead(status, { 'content-type': 'application/json' }).end(JSON.stringify(value));
}

// A fake GitHub API for repository o/r, with its uploads host on the same
// server under /uploads. `tag` is the commit the nightly tag names (none: no
// tag), and `assets` the names of the nightly release's assets (none: no
// release), listed `perPage` to a page. `fail` maps a request, as `METHOD
// path` without the query, to the statuses its first answers take; an upload
// named in `lost` is stored before its first answer fails with a 502, as when
// the answer to a stored upload is lost, and one named in `limited` is first
// refused as GitHub refuses a request over its secondary rate limit, asking
// for a wait of a second. With `renames`, an upload is stored under its name
// with each `+` as `.`.
async function fakeGitHub({ tag = OLD, assets, perPage = 100, fail = {}, lost = [], limited = [], renames = false } = {}) {
  const state = { tag, release: assets && { id: 1, assets: assets.map((name, i) => ({ id: 100 + i, name })) }, uploaded: [] };
  const seen = {};
  let next = 500;
  const release = () => ({
    id: 1,
    tag_name: TAG,
    html_url: 'https://github.example/o/r/releases/tag/nightly',
    upload_url: `${server.url}/uploads/repos/o/r/releases/1/assets{?name,label}`,
  });
  const server = await serve((req, res) => {
    const url = new URL(req.url, server.url);
    const key = `${req.method} ${url.pathname}`;
    seen[key] = (seen[key] ?? 0) + 1;
    const status = fail[key]?.[seen[key] - 1];
    const asset = url.pathname.match(/^\/repos\/o\/r\/releases\/assets\/(\d+)$/)?.[1];
    if (status !== undefined) {
      json(res, status, { message: 'Server Error' });
    } else if (key === `GET /repos/o/r/git/ref/tags/${TAG}`) {
      if (state.tag) json(res, 200, { ref: `refs/tags/${TAG}`, object: { sha: state.tag, type: 'commit' } });
      else json(res, 404, { message: 'Not Found' });
    } else if (key === `PATCH /repos/o/r/git/refs/tags/${TAG}`) {
      state.tag = JSON.parse(req.body).sha;
      json(res, 200, { ref: `refs/tags/${TAG}`, object: { sha: state.tag, type: 'commit' } });
    } else if (key === 'POST /repos/o/r/git/refs') {
      state.tag = JSON.parse(req.body).sha;
      json(res, 201, { ref: `refs/tags/${TAG}`, object: { sha: state.tag, type: 'commit' } });
    } else if (key === `GET /repos/o/r/releases/tags/${TAG}`) {
      if (state.release) json(res, 200, { ...release(), assets: state.release.assets });
      else json(res, 404, { message: 'Not Found' });
    } else if (key === 'POST /repos/o/r/releases') {
      state.release = { id: 1, assets: [] };
      json(res, 201, { ...release(), assets: [] });
    } else if (key === 'PATCH /repos/o/r/releases/1') {
      json(res, 200, { ...release(), assets: state.release.assets });
    } else if (key === 'GET /repos/o/r/releases/1/assets') {
      const page = Number(url.searchParams.get('page') ?? 1);
      const items = state.release.assets.slice((page - 1) * perPage, page * perPage);
      const headers = { 'content-type': 'application/json' };
      if (page * perPage < state.release.assets.length) {
        headers.link = `<${server.url}/repos/o/r/releases/1/assets?per_page=100&page=${page + 1}>; rel="next"`;
      }
      res.writeHead(200, headers).end(JSON.stringify(items));
    } else if (req.method === 'DELETE' && asset !== undefined) {
      const before = state.release.assets.length;
      state.release.assets = state.release.assets.filter(({ id }) => id !== Number(asset));
      if (state.release.assets.length === before) json(res, 404, { message: 'Not Found' });
      else res.writeHead(204).end();
    } else if (key === 'POST /uploads/repos/o/r/releases/1/assets') {
      const requested = url.searchParams.get('name');
      if (limited.includes(requested) && seen[`limited ${requested}`] === undefined) {
        seen[`limited ${requested}`] = 1;
        res.writeHead(403, { 'content-type': 'application/json', 'retry-after': '1' }).end(JSON.stringify({ message: 'You have exceeded a secondary rate limit' }));
        return;
      }
      const name = renames ? requested.replaceAll('+', '.') : requested;
      const stored = { id: next++, name };
      if (state.release.assets.some((a) => a.name === name)) {
        json(res, 422, { message: 'Validation Failed', errors: [{ code: 'already_exists' }] });
        return;
      }
      state.release.assets.push(stored);
      state.uploaded.push({ name, type: req.headers['content-type'], body: req.body.toString('utf8'), auth: req.headers.authorization });
      if (lost.includes(name) && seen[`lost ${name}`] === undefined) {
        seen[`lost ${name}`] = 1;
        json(res, 502, { message: 'Bad Gateway' });
      } else {
        json(res, 201, stored);
      }
    } else {
      json(res, 404, { message: 'Not Found' });
    }
  });
  server.state = state;
  return server;
}

// Each request as `METHOD path?query`.
const calls = (api) => api.requests.map((r) => `${r.method} ${r.url}`);
const bodyOf = (api, call) => JSON.parse(api.requests.find((r) => `${r.method} ${r.url}` === call).body);

// --- The notes --------------------------------------------------------------------------

test('the notes are what the shell step wrote: the links, the versions and a row per asset', () => {
  const assets = [
    { name: 'rusty-photon-rp_0.1.0+nightly.202607120507.gba09dc9_amd64.deb', sha256: 'a'.repeat(64) },
    { name: 'rusty-photon-rp-0.1.0.202607120507.gba09dc9-1.x86_64.rpm', sha256: 'b'.repeat(64) },
  ];
  // The shell step's own output for these inputs.
  const docs = `https://github.com/o/r/blob/${SHA}/docs`;
  const expected = `Rolling nightly build of \`main\`. Assets are replaced on
every publish — no dated history; fetch \`SHA256SUMS.txt\`
from this release for the current filenames. Install and
channel semantics: [docs/packaging.md](${docs}/packaging.md#nightly-channel)
(Windows: [docs/packaging-windows.md](${docs}/packaging-windows.md#nightly-channel),
macOS: [docs/packaging-macos.md](${docs}/packaging-macos.md#nightly-channel)).

- **Source**: ${SHA}
- **Built**: 20260712 (UTC)
- **Nightly version**: \`0.1.0+nightly.202607120507.gba09dc9\` (Debian version, MSI filename, and macOS tarball/formula version; the MSI's Programs & Features version renders as \`0.1.0.<YYDDD>\`)
- **RPM version**: \`0.1.0^202607120507.gba09dc9\` (\`rpm -q\` shows this; the asset filenames render the \`^\` as \`.\` because GitHub rewrites it at upload)
- **macOS**: don't download the tarballs by hand — \`brew tap rusty-photon/rusty-photon && brew install rusty-photon-nightly\` (the formulas point here)

| Asset | SHA-256 |
|-------|---------|
| \`rusty-photon-rp_0.1.0+nightly.202607120507.gba09dc9_amd64.deb\` | \`${'a'.repeat(64)}\` |
| \`rusty-photon-rp-0.1.0.202607120507.gba09dc9-1.x86_64.rpm\` | \`${'b'.repeat(64)}\` |
`;
  assert.equal(releaseBody({ repo: 'o/r', sha: SHA, date: '20260712', version: VERSION, assets }), expected);
});

// --- The tag ------------------------------------------------------------------------

test('a nightly tag elsewhere is moved to the commit by force', async (t) => {
  const api = await fakeGitHub();
  t.after(api.close);
  const { stdout } = await capture(() => moveTag({ api: api.url, repo: 'o/r', token: 'tok', sha: SHA }));
  assert.deepEqual(calls(api), [`GET /repos/o/r/git/ref/tags/${TAG}`, `PATCH /repos/o/r/git/refs/tags/${TAG}`]);
  assert.deepEqual(bodyOf(api, `PATCH /repos/o/r/git/refs/tags/${TAG}`), { sha: SHA, force: true });
  assert.equal(api.requests[1].headers.authorization, 'Bearer tok');
  assert.equal(api.state.tag, SHA);
  assert.equal(stdout, `The nightly tag is at ${SHA}.\n`);
});

test('no nightly tag is made, lightweight, at the commit', async (t) => {
  const api = await fakeGitHub({ tag: null });
  t.after(api.close);
  await capture(() => moveTag({ api: api.url, repo: 'o/r', token: 'tok', sha: SHA }));
  assert.deepEqual(calls(api), [`GET /repos/o/r/git/ref/tags/${TAG}`, 'POST /repos/o/r/git/refs']);
  assert.deepEqual(bodyOf(api, 'POST /repos/o/r/git/refs'), { ref: `refs/tags/${TAG}`, sha: SHA });
});

test('moving the tag is sent again after a 5xx, and making it is not', async (t) => {
  const moved = await fakeGitHub({ fail: { [`PATCH /repos/o/r/git/refs/tags/${TAG}`]: [502] } });
  t.after(moved.close);
  await capture(() => moveTag({ api: moved.url, repo: 'o/r', token: 'tok', sha: SHA, delayMs: 1 }));
  assert.equal(moved.state.tag, SHA);
  const made = await fakeGitHub({ tag: null, fail: { 'POST /repos/o/r/git/refs': [502] } });
  t.after(made.close);
  await assert.rejects(capture(() => moveTag({ api: made.url, repo: 'o/r', token: 'tok', sha: SHA, delayMs: 1 })), /^Error: Making the nightly tag at \w+ failed/);
  assert.equal(calls(made).filter((c) => c === 'POST /repos/o/r/git/refs').length, 1);
});

// --- The release --------------------------------------------------------------------

// Files to upload, in a fresh directory.
function files(names) {
  const dir = tempDir();
  return names.map((name) => {
    writeFileSync(path.join(dir, name), `bytes of ${name}`);
    return { name, file: path.join(dir, name) };
  });
}

const NEW = ['new.deb', 'new.x86_64.rpm', 'new.msi', 'new.tar.gz', SUMS];

function publish(api, upload, options = {}) {
  return capture(() => publishRelease({ api: api.url, repo: 'o/r', token: 'tok', body: 'the notes', files: upload, delayMs: 1, ...options }));
}

test('the release loses every old asset, every page of them, then gets its notes and the new assets', async (t) => {
  const api = await fakeGitHub({ assets: ['old-a.deb', 'old-b.rpm', 'old-c.msi', 'dropped.deb', 'SHA256SUMS.txt'], perPage: 2 });
  t.after(api.close);
  const { stdout } = await publish(api, files(NEW));
  const made = calls(api);
  // Every delete comes before the rewrite, and every upload after it.
  const rewrite = made.indexOf('PATCH /repos/o/r/releases/1');
  const deletes = made.flatMap((c, i) => (c.startsWith('DELETE ') ? [i] : []));
  const uploads = made.flatMap((c, i) => (c.startsWith('POST /uploads/') ? [i] : []));
  assert.equal(deletes.length, 5);
  assert.ok(Math.max(...deletes) < rewrite && rewrite < Math.min(...uploads));
  assert.equal(made.filter((c) => c.startsWith('GET /repos/o/r/releases/1/assets')).length, 3);
  assert.deepEqual(bodyOf(api, 'PATCH /repos/o/r/releases/1'), { name: TITLE, body: 'the notes', prerelease: true });
  assert.deepEqual(api.state.release.assets.map(({ name }) => name).sort(), [...NEW].sort());
  assert.deepEqual(
    api.state.uploaded.map(({ name, type, body, auth }) => [name, type, body, auth]).sort(),
    [
      ['SHA256SUMS.txt', 'text/plain; charset=utf-8', 'bytes of SHA256SUMS.txt', 'Bearer tok'],
      ['new.deb', 'application/x-debian-package', 'bytes of new.deb', 'Bearer tok'],
      ['new.msi', 'application/x-msi', 'bytes of new.msi', 'Bearer tok'],
      ['new.tar.gz', 'application/x-gtar', 'bytes of new.tar.gz', 'Bearer tok'],
      ['new.x86_64.rpm', 'application/x-rpm', 'bytes of new.x86_64.rpm', 'Bearer tok'],
    ],
  );
  assert.match(stdout, /^Deleted the nightly release's 5 asset\(s\)\.\n/);
  assert.match(stdout, /Uploaded 5 asset\(s\) to https:\/\/github\.example\/o\/r\/releases\/tag\/nightly\n$/);
});

test('no release yet makes one on the tag, as a prerelease, and deletes nothing', async (t) => {
  const api = await fakeGitHub();
  t.after(api.close);
  await publish(api, files(NEW));
  assert.deepEqual(bodyOf(api, 'POST /repos/o/r/releases'), { tag_name: TAG, name: TITLE, body: 'the notes', prerelease: true });
  assert.equal(calls(api).some((c) => c.startsWith('DELETE ') || c.startsWith('PATCH ')), false);
  assert.equal(api.state.release.assets.length, NEW.length);
});

test('making the release is sent once, and rewriting it again after a 5xx', async (t) => {
  const made = await fakeGitHub({ fail: { 'POST /repos/o/r/releases': [502] } });
  t.after(made.close);
  await assert.rejects(publish(made, files(NEW)), /^Error: Making the nightly release failed/);
  assert.equal(calls(made).filter((c) => c === 'POST /repos/o/r/releases').length, 1);
  const rewritten = await fakeGitHub({ assets: [], fail: { 'PATCH /repos/o/r/releases/1': [503] } });
  t.after(rewritten.close);
  await publish(rewritten, files(NEW));
  assert.equal(calls(rewritten).filter((c) => c === 'PATCH /repos/o/r/releases/1').length, 2);
});

test('an upload whose answer was lost is retried, once what it left under its name is deleted', async (t) => {
  const api = await fakeGitHub({ assets: [], lost: ['new.msi'] });
  t.after(api.close);
  const { stdout } = await publish(api, files(NEW));
  assert.deepEqual(api.state.release.assets.map(({ name }) => name).sort(), [...NEW].sort());
  assert.equal(api.state.uploaded.filter(({ name }) => name === 'new.msi').length, 2);
  assert.equal(calls(api).filter((c) => c.startsWith('DELETE ')).length, 1);
  assert.match(stdout, /upload new\.msi: attempt 1 of 3 failed/);
});

test('an upload over the rate limit waits as long as GitHub asks, then is retried', async (t) => {
  const api = await fakeGitHub({ assets: [], limited: ['new.deb'] });
  t.after(api.close);
  const { stdout } = await publish(api, files(['new.deb']));
  assert.deepEqual(api.state.release.assets.map(({ name }) => name), ['new.deb']);
  assert.match(stdout, /upload new\.deb: attempt 1 of 3 failed \(.*\); retrying in 1000 ms\n/);
});

test('an upload GitHub refuses is not retried, and fails the publish', async (t) => {
  const api = await fakeGitHub({ assets: ['new.deb'], fail: { 'POST /uploads/repos/o/r/releases/1/assets': [422] } });
  t.after(api.close);
  await assert.rejects(publish(api, files(['new.deb'])), /^Error: Uploading new\.deb failed \(GitHub answered: .*Server Error/);
  assert.equal(calls(api).filter((c) => c.startsWith('POST /uploads/')).length, 1);
});

test('a failed upload starts no further upload', async (t) => {
  const api = await fakeGitHub({ assets: [], fail: { 'POST /uploads/repos/o/r/releases/1/assets': [422] } });
  t.after(api.close);
  const many = files(Array.from({ length: 12 }, (_, i) => `asset-${String(i).padStart(2, '0')}.deb`));
  await assert.rejects(publish(api, many), /^Error: Uploading asset-\d+\.deb failed/);
  // The five under way at once end; none of the other seven starts.
  assert.ok(calls(api).filter((c) => c.startsWith('POST /uploads/')).length < many.length);
});

test('an upload GitHub stores under another name fails, since the sums would not list it', async (t) => {
  const api = await fakeGitHub({ assets: [], renames: true });
  t.after(api.close);
  await assert.rejects(
    publish(api, files(['a+b.deb'])),
    /^Error: GitHub named the upload of a\+b\.deb a\.b\.deb, which the sums do not list$/,
  );
});

test('an old asset already gone when its delete arrives is no error', async (t) => {
  const api = await fakeGitHub({ assets: ['old.deb'], fail: { 'DELETE /repos/o/r/releases/assets/100': [404] } });
  t.after(api.close);
  await publish(api, files(['new.deb']));
  assert.equal(calls(api).filter((c) => c.startsWith('DELETE ')).length, 1);
  assert.ok(api.state.uploaded.some(({ name }) => name === 'new.deb'));
});

// --- The whole publish ---------------------------------------------------------------

test('the publish moves the tag, then gives the release the summed assets, the sums and notes listing them', async (t) => {
  const api = await fakeGitHub({ assets: ['old.deb'] });
  t.after(api.close);
  const dir = tempDir();
  for (const name of ['a.deb', 'a.x86_64.rpm', 'a.msi', 'a.tar.gz', 'stray.txt']) writeFileSync(path.join(dir, name), name);
  await capture(() => writeSums(dir));
  await capture(() => publishNightly({ api: api.url, repo: 'o/r', token: 'tok', sha: SHA, date: '20260712', version: VERSION, dir, delayMs: 1 }));
  assert.equal(api.state.tag, SHA);
  assert.ok(calls(api).indexOf(`PATCH /repos/o/r/git/refs/tags/${TAG}`) < calls(api).indexOf(`GET /repos/o/r/releases/tags/${TAG}`));
  assert.deepEqual(api.state.release.assets.map(({ name }) => name).sort(), ['SHA256SUMS.txt', 'a.deb', 'a.msi', 'a.tar.gz', 'a.x86_64.rpm']);
  const { body } = bodyOf(api, 'PATCH /repos/o/r/releases/1');
  assert.match(body, /^- \*\*Source\*\*: ba09dc9a{33}$/m);
  assert.match(body, /^\| `a\.msi` \| `[0-9a-f]{64}` \|$/m);
  assert.doesNotMatch(body, /stray|SHA256SUMS\.txt` \|/);
});

test('a commit that is no full SHA is refused before anything is published', async (t) => {
  const api = await fakeGitHub();
  t.after(api.close);
  await assert.rejects(
    publishNightly({ api: api.url, repo: 'o/r', token: 'tok', sha: 'ba09dc9', date: '20260712', version: VERSION, dir: tempDir() }),
    /^Error: the packaged commit, "ba09dc9", is not a SHA-1 in lowercase hex$/,
  );
  assert.equal(api.requests.length, 0);
});
