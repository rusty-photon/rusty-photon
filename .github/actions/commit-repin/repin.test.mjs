import assert from 'node:assert/strict';
import { test } from 'node:test';

import { capture, serve } from '../lib/testing.mjs';
import { LOCK, blobSha, commitRepin } from './repin.mjs';

const HEAD = 'a'.repeat(40);
const COMMIT = 'c'.repeat(40);
const BRANCH = 'dependabot/cargo/libc-0.2.191';
const OLD = Buffer.from('{"lockFileVersion": 24, "old": true}\n');
const NEW = Buffer.from('{"lockFileVersion": 24, "new": true}\n');
const BOT = { name: 'github-actions[bot]', email: '41898282+github-actions[bot]@users.noreply.github.com' };

function json(res, status, value) {
  res.writeHead(status, { 'content-type': 'application/json' }).end(JSON.stringify(value));
}

// A fake GitHub API for repository o/r, whose commit HEAD has the tree
// `tree0`, which holds `lock` as LOCK (none with `noLock`) beside Cargo.lock.
// A blob is named as git names it, unless `wrongBlob`; a new tree is `tree1`
// and a new commit COMMIT. `fail` maps a request, as `METHOD path`, to the
// statuses its first answers take before it succeeds; the ref's PATCH
// answers 422 as GitHub does for a branch that moved, with `moved`.
async function fakeGitHub({ lock = OLD, noLock = false, wrongBlob = false, fail = {}, moved = false } = {}) {
  const seen = {};
  const server = await serve((req, res) => {
    const url = new URL(req.url, server.url);
    const key = `${req.method} ${url.pathname}`;
    seen[key] = (seen[key] ?? 0) + 1;
    const status = fail[key]?.[seen[key] - 1];
    if (status !== undefined) {
      json(res, status, { message: 'Server Error' });
    } else if (key === `GET /repos/o/r/git/commits/${HEAD}`) {
      json(res, 200, { sha: HEAD, tree: { sha: 'tree0' } });
    } else if (key === 'GET /repos/o/r/git/trees/tree0') {
      const entries = [{ path: 'Cargo.lock', mode: '100644', type: 'blob', sha: blobSha(Buffer.from('cargo')) }];
      if (!noLock) entries.push({ path: LOCK, mode: '100644', type: 'blob', sha: blobSha(lock) });
      json(res, 200, { sha: 'tree0', tree: entries, truncated: false });
    } else if (key === 'POST /repos/o/r/git/blobs') {
      const { content } = JSON.parse(req.body);
      json(res, 201, { sha: wrongBlob ? 'b'.repeat(40) : blobSha(Buffer.from(content, 'base64')) });
    } else if (key === 'POST /repos/o/r/git/trees') {
      json(res, 201, { sha: 'tree1' });
    } else if (key === 'POST /repos/o/r/git/commits') {
      json(res, 201, { sha: COMMIT });
    } else if (req.method === 'PATCH' && url.pathname.startsWith('/repos/o/r/git/refs/heads/')) {
      if (moved) json(res, 422, { message: 'Update is not a fast forward' });
      else json(res, 200, { ref: url.pathname.slice('/repos/o/r/git/'.length), object: { sha: COMMIT } });
    } else {
      json(res, 404, { message: 'Not Found' });
    }
  });
  return server;
}

function repin(api, options = {}) {
  return capture(() =>
    commitRepin({ api: api.url, repo: 'o/r', token: 'tok', branch: BRANCH, head: HEAD, lock: NEW, delayMs: 1, ...options }),
  );
}

// Each request as `METHOD path`, with the body sent, when one was.
const requests = (api) => api.requests.map((r) => [`${r.method} ${r.url}`, ...(r.body.length ? [JSON.parse(r.body)] : [])]);

// --- The blob's name ------------------------------------------------------------------

test('a blob is named as git names it', () => {
  // `printf 'hello\n' | git hash-object --stdin`, and the empty blob's.
  assert.equal(blobSha(Buffer.from('hello\n')), 'ce013625030ba8dba906f756967f9e9ca394464a');
  assert.equal(blobSha(Buffer.alloc(0)), 'e69de29bb2d1d6434b8b29ae775ad8c2e48c5391');
});

// --- The commit ---------------------------------------------------------------------------

test('a lock the branch lacks is committed on the checked-out commit, and the branch fast-forwarded to it', async (t) => {
  const api = await fakeGitHub();
  t.after(api.close);
  const { value, stdout } = await repin(api);
  assert.deepEqual(value, { outcome: 'committed', sha: COMMIT });
  assert.deepEqual(requests(api), [
    [`GET /repos/o/r/git/commits/${HEAD}`],
    ['GET /repos/o/r/git/trees/tree0'],
    ['POST /repos/o/r/git/blobs', { content: NEW.toString('base64'), encoding: 'base64' }],
    ['POST /repos/o/r/git/trees', { base_tree: 'tree0', tree: [{ path: LOCK, mode: '100644', type: 'blob', sha: blobSha(NEW) }] }],
    [
      'POST /repos/o/r/git/commits',
      { message: `chore(bazel): repin MODULE.bazel.lock for ${BRANCH}`, tree: 'tree1', parents: [HEAD], author: BOT, committer: BOT },
    ],
    [`PATCH /repos/o/r/git/refs/heads/${BRANCH}`, { sha: COMMIT, force: false }],
  ]);
  for (const request of api.requests) assert.equal(request.headers.authorization, 'Bearer tok');
  assert.match(stdout, new RegExp(`^Committed ${COMMIT} on ${HEAD}, and moved ${BRANCH} to it$`, 'm'));
});

test('a lock the branch already holds is not committed', async (t) => {
  const api = await fakeGitHub({ lock: NEW });
  t.after(api.close);
  const { value, stdout } = await repin(api);
  assert.deepEqual(value, { outcome: 'current' });
  assert.deepEqual(
    requests(api).map(([request]) => request),
    [`GET /repos/o/r/git/commits/${HEAD}`, 'GET /repos/o/r/git/trees/tree0'],
  );
  assert.match(stdout, /^MODULE\.bazel\.lock already in sync; nothing to push\.$/m);
});

test('a branch that moved since the checkout is not moved, and the request is not repeated', async (t) => {
  const api = await fakeGitHub({ moved: true });
  t.after(api.close);
  await assert.rejects(repin(api), (err) => {
    assert.equal(err.message, `Moving ${BRANCH} to ${COMMIT} failed (GitHub answered: {"message":"Update is not a fast forward"})`);
    return true;
  });
  assert.equal(api.requests.filter((r) => r.method === 'PATCH').length, 1);
});

test('a branch is moved once, even when GitHub gave no answer', async (t) => {
  const api = await fakeGitHub({ fail: { [`PATCH /repos/o/r/git/refs/heads/${BRANCH}`]: [502] } });
  t.after(api.close);
  await assert.rejects(repin(api), /^Error: Moving .* failed/);
  assert.equal(api.requests.filter((r) => r.method === 'PATCH').length, 1);
});

test('the blob, the tree and the commit are written again after a 5xx', async (t) => {
  const api = await fakeGitHub({
    fail: { 'POST /repos/o/r/git/blobs': [502], 'POST /repos/o/r/git/trees': [500], 'POST /repos/o/r/git/commits': [503] },
  });
  t.after(api.close);
  const { value } = await repin(api);
  assert.deepEqual(value, { outcome: 'committed', sha: COMMIT });
  assert.deepEqual(
    requests(api).map(([request]) => request).filter((request) => request.startsWith('POST')),
    ['POST /repos/o/r/git/blobs', 'POST /repos/o/r/git/blobs', 'POST /repos/o/r/git/trees', 'POST /repos/o/r/git/trees', 'POST /repos/o/r/git/commits', 'POST /repos/o/r/git/commits'],
  );
});

test('a blob GitHub names otherwise than its bytes hash to stops the commit', async (t) => {
  const api = await fakeGitHub({ wrongBlob: true });
  t.after(api.close);
  await assert.rejects(repin(api), new RegExp(`^Error: GitHub stored MODULE\\.bazel\\.lock as the blob b{40}, not ${blobSha(NEW)}, which its bytes hash to$`));
  assert.equal(api.requests.at(-1).url, '/repos/o/r/git/blobs');
});

test('a checked-out commit with no lock is refused', async (t) => {
  const api = await fakeGitHub({ noLock: true });
  t.after(api.close);
  await assert.rejects(repin(api), new RegExp(`^Error: ${HEAD} has no MODULE\\.bazel\\.lock$`));
  assert.equal(api.requests.length, 2);
});

test('a branch name is sent as the path of its ref, each part escaped', async (t) => {
  const api = await fakeGitHub();
  t.after(api.close);
  await repin(api, { branch: 'dependabot/cargo/a#b?c' });
  assert.equal(api.requests.at(-1).url, '/repos/o/r/git/refs/heads/dependabot/cargo/a%23b%3Fc');
});

test('a checked-out commit that is not a SHA, or no branch, is refused before any request', async (t) => {
  const api = await fakeGitHub();
  t.after(api.close);
  await assert.rejects(repin(api, { head: 'HEAD' }), /^Error: the checked-out commit, "HEAD", is not a SHA-1 in lowercase hex$/);
  await assert.rejects(repin(api, { head: `${HEAD}\n` }), /is not a SHA-1/);
  await assert.rejects(repin(api, { branch: '' }), /^Error: no branch to commit on$/);
  assert.equal(api.requests.length, 0);
});
