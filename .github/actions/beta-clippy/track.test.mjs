import assert from 'node:assert/strict';
import { mkdirSync, mkdtempSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { test } from 'node:test';

import { capture, serve } from '../lib/testing.mjs';
import { LABEL, MAX_LINTS, PREFIX, issueBody, readCensus, track } from './track.mjs';

const RUN = 'https://github.example/o/r/actions/runs/123';
const TOOLCHAIN = 'clippy 0.1.99 (0123456789 2026-10-14)';

function json(res, status, value) {
  res.writeHead(status, { 'content-type': 'application/json' }).end(JSON.stringify(value));
}

// A fake GitHub API for repository o/r: its open issues are `open`, and it
// has the label LABEL unless `labelMissing`. A new issue is numbered 100 and
// up; every other write answers as GitHub does, with nothing the action
// reads.
async function fakeGitHub(open = [], { labelMissing = false } = {}) {
  let next = 100;
  const server = await serve((req, res) => {
    const url = new URL(req.url, server.url);
    const issue = url.pathname.match(/^\/repos\/o\/r\/issues\/(\d+)$/);
    if (req.method === 'GET' && url.pathname === `/repos/o/r/labels/${LABEL}`) {
      if (labelMissing) json(res, 404, { message: 'Not Found' });
      else json(res, 200, { name: LABEL });
    } else if (req.method === 'POST' && url.pathname === '/repos/o/r/labels') {
      json(res, 201, JSON.parse(req.body));
    } else if (req.method === 'GET' && url.pathname === '/repos/o/r/issues') {
      json(res, 200, open);
    } else if (req.method === 'POST' && url.pathname === '/repos/o/r/issues') {
      json(res, 201, { number: next++ });
    } else if (req.method === 'PATCH' && issue) {
      json(res, 200, { number: Number(issue[1]) });
    } else if (req.method === 'POST' && /^\/repos\/o\/r\/issues\/\d+\/comments$/.test(url.pathname)) {
      json(res, 201, { id: 1 });
    } else {
      json(res, 404, { message: 'Not Found' });
    }
  });
  return server;
}

const entry = (lint, count = 2, extra = {}) => ({
  lint,
  count,
  sites: Array.from({ length: count }, (_, i) => `services/x/src/lib.rs:${i + 1}:5`),
  example: `the ${lint} example`,
  docs: null,
  ...extra,
});

const census = (...lints) => ({ toolchain: TOOLCHAIN, total: 0, lints });

const issue = (number, title) => ({ number, title, html_url: `https://github.example/o/r/issues/${number}` });

// Runs track against `api` and resolves to what it returned and printed, and
// the exit code it set.
async function runTrack(api, theCensus) {
  const saved = process.exitCode;
  try {
    const { value, stdout } = await capture(() => track({ api: api.url, repo: 'o/r', token: 'tok', runUrl: RUN, census: theCensus }));
    return { value, stdout, exitCode: process.exitCode };
  } finally {
    process.exitCode = saved;
  }
}

// The requests that write, as `METHOD path` and the body sent.
const writes = (api) => api.requests.filter((r) => r.method !== 'GET').map((r) => [`${r.method} ${r.url}`, JSON.parse(r.body)]);

// --- The issue's body -----------------------------------------------------------------

test('an issue’s body names the lint, its example, its sites, the toolchain and the run', () => {
  const body = issueBody(entry('clippy::redundant_clone', 2, { docs: 'https://lints.example/#redundant_clone' }), TOOLCHAIN, RUN);
  assert.equal(
    body,
    [
      '`clippy::redundant_clone` fires **2×** under the beta toolchain.',
      '',
      '`stable / clippy` is a required gate running `-D warnings`, so',
      '`main` is silent on stable — this lint is new on the beta channel',
      'and will start failing that gate when beta becomes stable. Fixing',
      'it in current code now avoids a rushed sweep on the bump.',
      '',
      '> the clippy::redundant_clone example',
      '',
      'Lint reference: https://lints.example/#redundant_clone',
      '',
      '**Sites** (showing 2 of 2):',
      '',
      '- `services/x/src/lib.rs:1:5`',
      '- `services/x/src/lib.rs:2:5`',
      '',
      `Toolchain: \`${TOOLCHAIN}\``,
      `Last seen: ${RUN}`,
      '',
      'Opened automatically by the nightly `beta / clippy` job. This',
      'body is rewritten on every run, and the issue closes itself once',
      'the lint stops being reported.',
    ].join('\n'),
  );
});

test('an issue’s body lists 20 sites at most, and no reference for a lint with none', () => {
  const body = issueBody(entry('unused_imports', 25), TOOLCHAIN, RUN);
  assert.match(body, /^\*\*Sites\*\* \(showing 20 of 25\):$/m);
  assert.equal(body.match(/^- `/gm).length, 20);
  assert.match(body, /^- `services\/x\/src\/lib\.rs:20:5`$/m);
  assert.doesNotMatch(body, /Lint reference/);
});

// --- The census's file ------------------------------------------------------------------

test('the census is read from its file, and a file that is not one is refused', () => {
  const base = process.env.TEST_TMPDIR ?? path.join(process.cwd(), '.tmp');
  mkdirSync(base, { recursive: true });
  const file = path.join(mkdtempSync(path.join(base, 'census-')), 'census.json');
  const good = census(entry('clippy::a'));
  writeFileSync(file, JSON.stringify(good));
  assert.deepEqual(readCensus(file), good);
  for (const [bad, why] of [
    [{ lints: [] }, /no toolchain/],
    [{ toolchain: TOOLCHAIN }, /no list of lints/],
    [census({ count: 1, sites: [] }), /a lint with no name/],
    [census({ lint: 'clippy::a', sites: [] }), /clippy::a has no count/],
    [census({ lint: 'clippy::a', count: 1 }), /clippy::a has no count of sites or no list of them/],
  ]) {
    writeFileSync(file, JSON.stringify(bad));
    assert.throws(() => readCensus(file), why);
  }
});

// --- Tracking -------------------------------------------------------------------------------

test('a lint with no open issue gets one, with both labels', async (t) => {
  const api = await fakeGitHub();
  t.after(api.close);
  const lint = entry('clippy::needless_range_loop');
  const { value, stdout, exitCode } = await runTrack(api, census(lint));
  assert.equal(exitCode, undefined);
  assert.deepEqual(value, { updated: [], opened: [100], closed: [] });
  assert.deepEqual(writes(api), [
    [
      'POST /repos/o/r/issues',
      { title: `${PREFIX}clippy::needless_range_loop`, body: issueBody(lint, TOOLCHAIN, RUN), labels: [LABEL, 'check-nightly'] },
    ],
  ]);
  assert.match(stdout, /^opened #100 — beta clippy: clippy::needless_range_loop \(2 sites\)$/m);
  for (const request of api.requests) assert.equal(request.headers.authorization, 'Bearer tok');
  const list = new URL(api.requests.find((r) => r.url.startsWith('/repos/o/r/issues?')).url, api.url);
  assert.deepEqual(Object.fromEntries(list.searchParams), { state: 'open', labels: LABEL, per_page: '100' });
});

test('a lint with an open issue gets its body rewritten', async (t) => {
  const api = await fakeGitHub([issue(7, `${PREFIX}clippy::map_unwrap_or`)]);
  t.after(api.close);
  const lint = entry('clippy::map_unwrap_or', 3);
  const { value, stdout } = await runTrack(api, census(lint));
  assert.deepEqual(value, { updated: [7], opened: [], closed: [] });
  assert.deepEqual(writes(api), [['PATCH /repos/o/r/issues/7', { body: issueBody(lint, TOOLCHAIN, RUN) }]]);
  assert.match(stdout, /^updated #7 — beta clippy: clippy::map_unwrap_or \(3 sites\)$/m);
});

test('an issue whose lint is no longer reported gets a comment and is closed; others are left alone', async (t) => {
  const api = await fakeGitHub([
    issue(7, `${PREFIX}clippy::map_unwrap_or`),
    issue(8, `${PREFIX}clippy::redundant_clone`),
    issue(9, 'a beta clippy question someone asked'),
    { ...issue(10, `${PREFIX}clippy::from_a_pull_request`), pull_request: {} },
  ]);
  t.after(api.close);
  const { value, stdout } = await runTrack(api, census(entry('clippy::map_unwrap_or')));
  assert.deepEqual(value, { updated: [7], opened: [], closed: [8] });
  assert.deepEqual(writes(api).slice(1), [
    ['POST /repos/o/r/issues/8/comments', { body: `No longer reported by \`beta / clippy\` — closing.\n\nRun: ${RUN}` }],
    ['PATCH /repos/o/r/issues/8', { state: 'closed', state_reason: 'completed' }],
  ]);
  assert.match(stdout, /^closed #8 — beta clippy: clippy::redundant_clone$/m);
});

test('a census with no lints closes every issue this job opened', async (t) => {
  const api = await fakeGitHub([issue(7, `${PREFIX}clippy::a`), issue(8, `${PREFIX}clippy::b`)]);
  t.after(api.close);
  const { value } = await runTrack(api, census());
  assert.deepEqual(value, { updated: [], opened: [], closed: [7, 8] });
});

test('the label is created before any issue when the repository lacks it', async (t) => {
  const api = await fakeGitHub([], { labelMissing: true });
  t.after(api.close);
  const { value, stdout } = await runTrack(api, census(entry('clippy::a')));
  assert.deepEqual(value, { updated: [], opened: [100], closed: [] });
  assert.deepEqual(
    writes(api).map(([request]) => request),
    ['POST /repos/o/r/labels', 'POST /repos/o/r/issues'],
  );
  assert.deepEqual(writes(api)[0][1], { name: LABEL, color: 'ededed', description: 'a lint beta clippy reports that stable does not yet' });
  assert.match(stdout, /^Created the label beta-clippy$/m);
});

test(`more than ${MAX_LINTS} lints fail the step and touch no issue`, async (t) => {
  const api = await fakeGitHub([issue(7, `${PREFIX}clippy::a`)]);
  t.after(api.close);
  const lints = Array.from({ length: MAX_LINTS + 1 }, (_, i) => entry(`clippy::lint_${i}`));
  const { value, stdout, exitCode } = await runTrack(api, census(...lints));
  assert.equal(exitCode, 1);
  assert.deepEqual(value, { updated: [], opened: [], closed: [] });
  assert.match(stdout, /^::error::beta clippy reported 21 distinct lints \(cap 20\); triage the census/m);
  assert.equal(api.requests.length, 0);

  // At the cap, it tracks them.
  const { value: atCap } = await runTrack(api, census(...lints.slice(0, MAX_LINTS)));
  assert.equal(atCap.opened.length, MAX_LINTS);
});
