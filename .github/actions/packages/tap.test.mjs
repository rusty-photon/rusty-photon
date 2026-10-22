import assert from 'node:assert/strict';
import { mkdirSync, mkdtempSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { test } from 'node:test';

import { capture, serve } from '../lib/testing.mjs';
import { BRANCH, FORMULA, TAP, belongsTo, commitFormulas, readFormulas } from './tap.mjs';

function tempDir() {
  return mkdtempSync(path.join(process.env.TEST_TMPDIR ?? tmpdir(), 'tap-'));
}

const HEAD = 'a'.repeat(40);
const COMMIT = 'c'.repeat(40);
const BOT = { name: 'github-actions[bot]', email: 'github-actions[bot]@users.noreply.github.com' };

// The tap's Formula directory as it is: the .gitkeep, the stable channel's
// formulas, and the nightly's.
const PRESENT = [
  '.gitkeep',
  'filemonitor.rb',
  'rusty-photon-rp.rb',
  'rusty-photon-nightly.rb',
  'rusty-photon-rp-nightly.rb',
  'rusty-photon-dropped-nightly.rb',
];

function json(res, status, value) {
  res.writeHead(status, { 'content-type': 'application/json' }).end(JSON.stringify(value));
}

// A fake GitHub API for the tap, whose branch's tip HEAD has the tree
// `tree0`, holding README.md and the Formula directory `formula0` with
// `present` in it (none: no Formula directory). A new tree is `tree1`, or,
// with `unchanged`, `tree0` itself; a new commit is COMMIT. `fail` maps a
// request, as `METHOD path`, to the statuses its first answers take; the
// ref's PATCH answers 422 as GitHub does for a branch that moved, with `moved`.
async function fakeTap({ present = PRESENT, unchanged = false, fail = {}, moved = false } = {}) {
  const repos = `/repos/${TAP}`;
  const seen = {};
  const server = await serve((req, res) => {
    const key = `${req.method} ${new URL(req.url, server.url).pathname}`;
    seen[key] = (seen[key] ?? 0) + 1;
    const status = fail[key]?.[seen[key] - 1];
    if (status !== undefined) {
      json(res, status, { message: 'Server Error' });
    } else if (key === `GET ${repos}/git/ref/heads/${BRANCH}`) {
      json(res, 200, { ref: `refs/heads/${BRANCH}`, object: { sha: HEAD, type: 'commit' } });
    } else if (key === `GET ${repos}/git/commits/${HEAD}`) {
      json(res, 200, { sha: HEAD, tree: { sha: 'tree0' } });
    } else if (key === `GET ${repos}/git/trees/tree0`) {
      const tree = [{ path: 'README.md', mode: '100644', type: 'blob', sha: 'r'.repeat(40) }];
      if (present) tree.push({ path: FORMULA, mode: '040000', type: 'tree', sha: 'formula0' });
      json(res, 200, { sha: 'tree0', tree, truncated: false });
    } else if (key === `GET ${repos}/git/trees/formula0`) {
      json(res, 200, { sha: 'formula0', tree: present.map((name) => ({ path: name, mode: '100644', type: 'blob', sha: 'f'.repeat(40) })) });
    } else if (key === `POST ${repos}/git/trees`) {
      json(res, 201, { sha: unchanged ? 'tree0' : 'tree1' });
    } else if (key === `POST ${repos}/git/commits`) {
      json(res, 201, { sha: COMMIT });
    } else if (key === `PATCH ${repos}/git/refs/heads/${BRANCH}`) {
      if (moved) json(res, 422, { message: 'Update is not a fast forward' });
      else json(res, 200, { ref: `refs/heads/${BRANCH}`, object: { sha: COMMIT } });
    } else {
      json(res, 404, { message: 'Not Found' });
    }
  });
  return server;
}

const formula = (name) => ({ name, content: `class ${name.replace(/\W/g, '')} < Formula\nend\n` });
const NIGHTLY = [formula('rusty-photon-nightly.rb'), formula('rusty-photon-rp-nightly.rb'), formula('rusty-photon-new-nightly.rb')];
const STABLE = [formula('rusty-photon.rb'), formula('rusty-photon-rp.rb')];

function commit(api, options = {}) {
  return capture(() =>
    commitFormulas({ api: api.url, token: 'tap-token', channel: 'nightly', version: '0.1.0+nightly.202607120507.gba09dc9', formulas: NIGHTLY, delayMs: 1, ...options }),
  );
}

const calls = (api) => api.requests.map((r) => `${r.method} ${r.url}`);
const bodyOf = (api, call) => JSON.parse(api.requests.find((r) => `${r.method} ${r.url}` === call).body);
const repos = `/repos/${TAP}`;

// --- Whose formula ------------------------------------------------------------------

test('a channel owns its own .rb files and nothing else', () => {
  assert.equal(belongsTo('nightly', 'rusty-photon-rp-nightly.rb'), true);
  assert.equal(belongsTo('nightly', 'rusty-photon-rp.rb'), false);
  assert.equal(belongsTo('stable', 'rusty-photon-rp.rb'), true);
  assert.equal(belongsTo('stable', 'filemonitor.rb'), true);
  assert.equal(belongsTo('stable', 'rusty-photon-rp-nightly.rb'), false);
  for (const channel of ['nightly', 'stable']) {
    assert.equal(belongsTo(channel, '.gitkeep'), false);
    assert.equal(belongsTo(channel, 'README.md'), false);
  }
});

test('the formulas are the .rb files the generator wrote, by name', async () => {
  const dir = tempDir();
  for (const name of ['b-nightly.rb', 'a-nightly.rb', 'notes.txt']) writeFileSync(path.join(dir, name), name);
  mkdirSync(path.join(dir, 'c-nightly.rb'));
  assert.deepEqual(await readFormulas(dir), [
    { name: 'a-nightly.rb', content: 'a-nightly.rb' },
    { name: 'b-nightly.rb', content: 'b-nightly.rb' },
  ]);
});

// --- The commit ---------------------------------------------------------------------

test('the nightly formulas replace the nightly channel’s, in one commit fast-forwarded onto the tap’s main', async (t) => {
  const api = await fakeTap();
  t.after(api.close);
  const { value, stdout } = await commit(api);
  assert.deepEqual(value, { outcome: 'committed', sha: COMMIT });
  assert.deepEqual(calls(api), [
    `GET ${repos}/git/ref/heads/${BRANCH}`,
    `GET ${repos}/git/commits/${HEAD}`,
    `GET ${repos}/git/trees/tree0`,
    `GET ${repos}/git/trees/formula0`,
    `POST ${repos}/git/trees`,
    `POST ${repos}/git/commits`,
    `PATCH ${repos}/git/refs/heads/${BRANCH}`,
  ]);
  assert.ok(api.requests.every((r) => r.headers.authorization === 'Bearer tap-token'));
  // The stable formulas and the .gitkeep are not touched; the nightly
  // formula the generator no longer renders goes.
  assert.deepEqual(bodyOf(api, `POST ${repos}/git/trees`), {
    base_tree: 'tree0',
    tree: [
      { path: 'Formula/rusty-photon-dropped-nightly.rb', mode: '100644', type: 'blob', sha: null },
      ...NIGHTLY.map(({ name, content }) => ({ path: `Formula/${name}`, mode: '100644', type: 'blob', content })),
    ],
  });
  assert.deepEqual(bodyOf(api, `POST ${repos}/git/commits`), {
    message: 'nightly: 0.1.0+nightly.202607120507.gba09dc9',
    tree: 'tree1',
    parents: [HEAD],
    author: BOT,
    committer: BOT,
  });
  assert.deepEqual(bodyOf(api, `PATCH ${repos}/git/refs/heads/${BRANCH}`), { sha: COMMIT, force: false });
  assert.equal(stdout, `Committed 3 nightly formula(s) to ${TAP} as ${COMMIT}, deleting 1.\n`);
});

test('the stable formulas replace the stable channel’s, the old filemonitor.rb among them, and leave the nightly’s', async (t) => {
  const api = await fakeTap();
  t.after(api.close);
  await commit(api, { channel: 'stable', version: '0.2.0', formulas: STABLE });
  assert.deepEqual(bodyOf(api, `POST ${repos}/git/trees`).tree, [
    { path: 'Formula/filemonitor.rb', mode: '100644', type: 'blob', sha: null },
    ...STABLE.map(({ name, content }) => ({ path: `Formula/${name}`, mode: '100644', type: 'blob', content })),
  ]);
  assert.equal(bodyOf(api, `POST ${repos}/git/commits`).message, 'release: 0.2.0');
});

test('a tap that already holds the formulas gets no commit', async (t) => {
  const api = await fakeTap({ unchanged: true });
  t.after(api.close);
  const { value, stdout } = await commit(api);
  assert.deepEqual(value, { outcome: 'current' });
  assert.equal(calls(api).at(-1), `POST ${repos}/git/trees`);
  assert.equal(stdout, `${TAP} already holds these nightly formulas; nothing to commit.\n`);
});

test('a check writes the tree, which takes the right to write the tap, and commits nothing', async (t) => {
  for (const unchanged of [false, true]) {
    const api = await fakeTap({ unchanged });
    t.after(api.close);
    const { value, stdout } = await commit(api, { check: true });
    assert.deepEqual(value, { outcome: 'checked' });
    assert.equal(calls(api).at(-1), `POST ${repos}/git/trees`);
    assert.match(stdout, /^The token can write rusty-photon\/homebrew-rusty-photon: it wrote the tree of the nightly formulas, tree[01]\. Nothing is committed yet\.\n$/);
  }
});

test('a check with a token that cannot write the tap fails', async (t) => {
  const api = await fakeTap({ fail: { [`POST ${repos}/git/trees`]: [403] } });
  t.after(api.close);
  await assert.rejects(commit(api, { check: true }), /^Error: Writing the tree with the nightly formulas failed/);
});

test('a tap with no Formula directory gets one, with nothing to delete', async (t) => {
  const api = await fakeTap({ present: null });
  t.after(api.close);
  await commit(api);
  assert.equal(calls(api).includes(`GET ${repos}/git/trees/formula0`), false);
  assert.ok(bodyOf(api, `POST ${repos}/git/trees`).tree.every(({ sha }) => sha !== null));
});

test('a branch that moved meanwhile fails, and its move is sent once', async (t) => {
  const api = await fakeTap({ moved: true });
  t.after(api.close);
  await assert.rejects(commit(api), /^Error: Moving rusty-photon\/homebrew-rusty-photon's main to c+ failed \(GitHub answered: .*not a fast forward/);
  assert.equal(calls(api).filter((c) => c.startsWith('PATCH ')).length, 1);
});

test('the tree and the commit are sent again after a 5xx', async (t) => {
  const api = await fakeTap({ fail: { [`POST ${repos}/git/trees`]: [502], [`POST ${repos}/git/commits`]: [503] } });
  t.after(api.close);
  assert.equal((await commit(api)).value.outcome, 'committed');
  assert.equal(calls(api).filter((c) => c.startsWith('POST ')).length, 4);
});

test('a formula of the other channel, an unknown channel or no formula is refused before any request', async (t) => {
  const api = await fakeTap();
  t.after(api.close);
  await assert.rejects(commit(api, { formulas: [...NIGHTLY, formula('rusty-photon-rp.rb')] }), /^Error: rusty-photon-rp\.rb would not be the nightly channel's in the tap$/);
  await assert.rejects(commit(api, { channel: 'stable', formulas: NIGHTLY.slice(0, 1) }), /^Error: rusty-photon-nightly\.rb would not be the stable channel's in the tap$/);
  await assert.rejects(commit(api, { channel: 'beta' }), /^Error: no channel "beta"; it is nightly or stable$/);
  await assert.rejects(commit(api, { formulas: [] }), /^Error: no nightly formula to commit$/);
  assert.equal(api.requests.length, 0);
});
