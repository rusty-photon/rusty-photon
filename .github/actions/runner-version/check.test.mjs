import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

import { serve } from '../lib/testing.mjs';
import { assess, compareVersions, DEADLINE_DAYS, parseGraceDays, parseVersion, releases, report } from './check.mjs';

// --- Fixtures -----------------------------------------------------------------

// The first page of actions/runner's releases, recorded with `gh api
// 'repos/actions/runner/releases?per_page=8'` and trimmed to the fields the
// check reads. v2.338.1 is a pre-release.
const RECORDED = JSON.parse(readFileSync(new URL('./testdata/releases.json', import.meta.url), 'utf8'));

function json(res, status, value) {
  res.writeHead(status, { 'content-type': 'application/json' }).end(JSON.stringify(value));
}

// A fake GitHub API that answers the releases list with `list`.
async function fakeGitHub(list) {
  return serve((req, res) => {
    const url = new URL(req.url, 'http://x');
    if (req.method === 'GET' && url.pathname === '/repos/actions/runner/releases') json(res, 200, list);
    else json(res, 404, { message: 'Not Found' });
  });
}

// The recorded releases as releases() returns them, without the fake API.
function published() {
  return RECORDED.filter((r) => !r.draft && !r.prerelease).map((r) => ({
    version: r.tag_name.slice(1),
    published: new Date(r.published_at),
  }));
}

const DAY_MS = 24 * 60 * 60 * 1000;
const V2_337_0 = new Date('2026-08-26T14:33:29Z');

// --- parseVersion and parseGraceDays ---------------------------------------------

test('a version file holds one major.minor.patch line', () => {
  assert.equal(parseVersion('2.338.2\n'), '2.338.2');
});

test('a version file with anything else is refused', () => {
  for (const text of ['v2.338.2', '2.338', '', '2.338.2\n2.338.1\n']) {
    assert.throws(() => parseVersion(text), /not one major\.minor\.patch line/, JSON.stringify(text));
  }
});

test('grace days are a whole number short of the deadline', () => {
  assert.equal(parseGraceDays('0'), 0);
  assert.equal(parseGraceDays('14'), 14);
  assert.equal(parseGraceDays(String(DEADLINE_DAYS - 1)), DEADLINE_DAYS - 1);
  for (const text of [String(DEADLINE_DAYS), '-1', '1.5', 'x', '']) {
    assert.throws(() => parseGraceDays(text), /grace-days/, JSON.stringify(text));
  }
});

// --- compareVersions -----------------------------------------------------------

test('versions compare by number, not as text', () => {
  assert.ok(compareVersions('2.338.10', '2.338.2') > 0);
  assert.ok(compareVersions('2.337.0', '2.338.0') < 0);
  assert.ok(compareVersions('3.0.0', '2.999.999') > 0);
  assert.equal(compareVersions('2.338.2', '2.338.2'), 0);
});

// --- releases ------------------------------------------------------------------

test('the releases list leaves out pre-releases and drafts', async () => {
  const draft = { tag_name: 'v2.339.0', name: 'v2.339.0', draft: true, prerelease: false, published_at: null };
  const api = await fakeGitHub([draft, ...RECORDED]);
  try {
    const list = await releases({ api: api.url, token: 't0ken' });
    assert.deepEqual(
      list.map((r) => r.version),
      ['2.338.2', '2.338.0', '2.337.0', '2.336.0', '2.335.1', '2.335.0', '2.334.0'],
    );
    assert.deepEqual(list[0].published, new Date('2026-10-26T14:45:12Z'));
    assert.equal(api.requests.length, 1);
    assert.equal(api.requests[0].url, '/repos/actions/runner/releases?per_page=30');
    assert.equal(api.requests[0].headers.authorization, 'Bearer t0ken');
  } finally {
    await api.close();
  }
});

test('a release list GitHub refuses fails the check, naming the request', async () => {
  const api = await serve((req, res) => json(res, 404, { message: 'Not Found' }));
  try {
    await assert.rejects(releases({ api: api.url }), /listing the releases of actions\/runner failed/);
  } finally {
    await api.close();
  }
});

// --- assess and report -----------------------------------------------------------

test('the newest release is current', () => {
  const result = assess({ pinned: '2.338.2', published: published(), now: new Date('2026-11-06T00:00:00Z'), graceDays: 14 });
  assert.deepEqual(result, { status: 'current', pinned: '2.338.2' });
  assert.match(report(result), /runner 2\.338\.2 is the newest release/);
});

test('the deadline runs from the first newer release, and the check waits out the grace', () => {
  const now = new Date(V2_337_0.getTime() + 13 * DAY_MS);
  const result = assess({ pinned: '2.336.0', published: published(), now, graceDays: 14 });
  assert.equal(result.status, 'behind');
  assert.equal(result.first.version, '2.337.0');
  assert.equal(result.latest.version, '2.338.2');
  assert.deepEqual(result.deadline, new Date('2026-09-25T14:33:29Z'));
  assert.equal(result.due, false);
});

test('the check is due once the grace has passed', () => {
  const now = new Date(V2_337_0.getTime() + 14 * DAY_MS);
  assert.equal(assess({ pinned: '2.336.0', published: published(), now, graceDays: 14 }).due, true);
});

test('templates left on 2.336.0 into November are long due', () => {
  const result = assess({ pinned: '2.336.0', published: published(), now: new Date('2026-11-05T08:00:00Z'), graceDays: 14 });
  assert.equal(result.due, true);
  assert.equal(
    report(result),
    "The pool templates' runner 2.336.0 is behind actions/runner 2.338.2. 2.337.0 was released on 2026-08-26, " +
      "so GitHub stops queueing jobs to the pool's runners from about 2026-09-25. Rebuild both templates with " +
      '2.338.2 and update tools/ci/runner-guest/RUNNER_VERSION (docs/skills/proxmox-runner-pool.md §Runner releases).',
  );
});

test('a pre-release newer than the templates does not start the clock', () => {
  const list = published().filter((r) => r.version !== '2.338.2');
  const result = assess({ pinned: '2.338.0', published: list, now: new Date('2026-11-30T00:00:00Z'), graceDays: 14 });
  assert.equal(result.status, 'current');
});
