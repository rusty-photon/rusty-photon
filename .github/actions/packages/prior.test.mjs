import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { existsSync, mkdtempSync, readFileSync, readdirSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { test } from 'node:test';

import { capture, serve } from '../lib/testing.mjs';
import { fetchPriorMsi } from './prior.mjs';

function tempDir() {
  return mkdtempSync(path.join(process.env.TEST_TMPDIR ?? tmpdir(), 'prior-'));
}

const MSI = Buffer.from('last night’s MSI\n');
const OTHER_MSI = Buffer.from('another MSI\n');
const digest = (bytes) => `sha256:${createHash('sha256').update(bytes).digest('hex')}`;

// The release's assets as GitHub lists them: a Linux package and the sums
// before the MSI, and a second MSI after it.
const ASSETS = [
  { id: 11, name: 'rusty-photon-rp_0.1.0+nightly.202610211001.ga49e79d_amd64.deb', digest: digest(Buffer.from('deb')) },
  { id: 12, name: 'SHA256SUMS.txt', digest: digest(Buffer.from('sums')) },
  { id: 13, name: 'rusty-photon-0.1.0+nightly.202610211001.ga49e79d-x64.msi', digest: digest(MSI) },
  { id: 14, name: 'rusty-photon-0.1.0+nightly.202610201001.gc12b06c-x64.msi', digest: digest(OTHER_MSI) },
];

function json(res, status, value) {
  res.writeHead(status, { 'content-type': 'application/json' }).end(JSON.stringify(value));
}

// A fake GitHub API for repository o/r, whose nightly release holds `assets`
// (none: no release), and a fake storage server it redirects each asset's
// download to, as GitHub's API redirects to its release-assets host. Asset
// 13 is MSI and 14 OTHER_MSI; `status`, when given, answers the release.
async function fakeGitHub(t, { assets = ASSETS, status } = {}) {
  const storage = await serve((req, res) => res.end(req.url === '/14' ? OTHER_MSI : MSI));
  t.after(storage.close);
  const api = await serve((req, res) => {
    const id = req.url.match(/^\/repos\/o\/r\/releases\/assets\/(\d+)$/)?.[1];
    if (status !== undefined) {
      json(res, status, { message: 'Bad credentials' });
    } else if (req.url === '/repos/o/r/releases/tags/nightly' && assets) {
      json(res, 200, { id: 1, tag_name: 'nightly', prerelease: true, assets });
    } else if (id !== undefined) {
      res.writeHead(302, { location: `${storage.url}/${id}` }).end();
    } else {
      json(res, 404, { message: 'Not Found' });
    }
  });
  t.after(api.close);
  return { api, storage };
}

function fetchWith(api, dir, options = {}) {
  return capture(() => fetchPriorMsi({ api: api.url, repo: 'o/r', token: 'tok', dir, download: { delayMs: 1 }, delayMs: 1, ...options }));
}

test('the first MSI the release carries is downloaded through the API, checked, and its path resolved', async (t) => {
  const { api, storage } = await fakeGitHub(t);
  const dir = path.join(tempDir(), 'prior-nightly');
  const { value, stdout } = await fetchWith(api, dir);
  assert.equal(value, path.join(dir, ASSETS[2].name));
  assert.deepEqual(readFileSync(value), MSI);
  assert.deepEqual(readdirSync(dir), [ASSETS[2].name]);
  assert.deepEqual(
    api.requests.map((r) => r.url),
    ['/repos/o/r/releases/tags/nightly', '/repos/o/r/releases/assets/13'],
  );
  // The asset is asked for as its bytes, with the token, which goes to the
  // API alone and not on to where it redirects.
  const [read, asset] = api.requests;
  assert.equal(read.headers.authorization, 'Bearer tok');
  assert.equal(asset.headers.authorization, 'Bearer tok');
  assert.equal(asset.headers.accept, 'application/octet-stream');
  assert.deepEqual(storage.requests.map((r) => [r.url, r.headers.authorization]), [['/13', undefined]]);
  assert.match(stdout, /: SHA-256 [0-9a-f]{64} matches its pin\n/);
  assert.match(stdout, /Upgrade seed: rusty-photon-0\.1\.0\+nightly\.202610211001\.ga49e79d-x64\.msi\n$/);
});

test('an MSI is found whatever the case of its extension', async (t) => {
  const assets = [ASSETS[0], { ...ASSETS[2], name: 'RUSTY-PHOTON-X64.MSI' }];
  const { api } = await fakeGitHub(t, { assets });
  const dir = tempDir();
  assert.equal((await fetchWith(api, dir)).value, path.join(dir, 'RUSTY-PHOTON-X64.MSI'));
});

test('no nightly release yet is no upgrade seed', async (t) => {
  const { api } = await fakeGitHub(t, { assets: null });
  const dir = path.join(tempDir(), 'prior-nightly');
  const { value, stdout } = await fetchWith(api, dir);
  assert.equal(value, undefined);
  assert.equal(stdout, 'There is no nightly release yet; skipping the upgrade seed.\n');
  assert.equal(existsSync(dir), false);
});

test('a nightly release with no MSI yet is no upgrade seed', async (t) => {
  const { api } = await fakeGitHub(t, { assets: ASSETS.slice(0, 2) });
  const dir = path.join(tempDir(), 'prior-nightly');
  const { value, stdout } = await fetchWith(api, dir);
  assert.equal(value, undefined);
  assert.equal(stdout, 'The nightly release has no MSI yet; skipping the upgrade seed.\n');
  assert.equal(api.requests.length, 1);
});

test('a release that cannot be read fails, rather than drop the upgrade proof', async (t) => {
  const { api } = await fakeGitHub(t, { status: 403 });
  await assert.rejects(fetchWith(api, tempDir()), /^Error: Reading the nightly release failed \(GitHub answered: .*Bad credentials/);
});

test('an MSI that fails its SHA-256 fails, and leaves nothing behind', async (t) => {
  const { api } = await fakeGitHub(t, { assets: [{ ...ASSETS[2], digest: digest(OTHER_MSI) }] });
  const dir = path.join(tempDir(), 'prior-nightly');
  await assert.rejects(fetchWith(api, dir), /SHA-256 mismatch/);
  assert.deepEqual(readdirSync(dir), []);
});

test('an MSI GitHub gives no SHA-256 for fails, and nothing is downloaded', async (t) => {
  for (const missing of [undefined, null, 'sha512:00', `sha256:${'A'.repeat(64)}`]) {
    const { api } = await fakeGitHub(t, { assets: [{ ...ASSETS[2], digest: missing }] });
    await assert.rejects(
      fetchWith(api, tempDir()),
      new RegExp(`^Error: The nightly release's ${ASSETS[2].name.replace(/[.+]/g, '\\$&')} has no SHA-256 from GitHub to check it against \\(its digest is `),
    );
    assert.equal(api.requests.length, 1);
  }
});
