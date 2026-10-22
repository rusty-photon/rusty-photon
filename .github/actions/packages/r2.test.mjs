import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { mkdirSync, mkdtempSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { test } from 'node:test';

import { capture, serve } from '../lib/testing.mjs';
import { FLIP, MANIFEST, MANIFEST_PREV, amzDate, authorization, bucketStore, isFlip, objectPath, publishSite, uriEncode } from './r2.mjs';

function tempDir() {
  return mkdtempSync(path.join(process.env.TEST_TMPDIR ?? tmpdir(), 'r2-'));
}

// The key pair of the AWS Signature Version 4 test suite, as botocore's
// tests/unit/auth/test_sigv4.py gives it.
const KEY = { accessKeyId: 'AKIDEXAMPLE', secretAccessKey: 'wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY' };

test('signs as the SigV4 test suite does', () => {
  // botocore's tests/unit/auth/aws4_testsuite/get-vanilla.
  assert.equal(
    authorization({
      method: 'GET',
      path: '/',
      headers: { host: 'example.amazonaws.com', 'x-amz-date': '20150830T123600Z' },
      payloadHash: 'e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855',
      region: 'us-east-1',
      service: 'service',
      ...KEY,
    }),
    'AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=host;x-amz-date, Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31',
  );
});

test('signs an object upload as the aws CLI does', () => {
  // `aws s3api put-object --endpoint-url http://127.0.0.1:18765 --bucket
  // rusty-photon-packages --key <the deb below> --body <BODY> --cache-control
  // no-store` with the suite's key pair and region auto (aws-cli 2.37.0),
  // as a loopback server received it.
  const body = 'Package: rusty-photon-sentinel\n';
  const hash = createHash('sha256').update(body).digest('hex');
  const key = 'deb/pool/main/rusty-photon-sentinel_0.1.0+nightly.202610221006.g8c9e489_amd64.deb';
  const path = objectPath('rusty-photon-packages', key);
  assert.equal(path, '/rusty-photon-packages/deb/pool/main/rusty-photon-sentinel_0.1.0%2Bnightly.202610221006.g8c9e489_amd64.deb');
  assert.equal(hash, 'b0860b77822aa9cfcae0e1546d9ce758b730a3b872f850d521b95c0e1d0ba86c');
  assert.equal(
    authorization({
      method: 'PUT',
      path,
      headers: { 'cache-control': 'no-store', host: '127.0.0.1:18765', 'x-amz-content-sha256': hash, 'x-amz-date': '20261022T111303Z' },
      payloadHash: hash,
      region: 'auto',
      service: 's3',
      ...KEY,
    }),
    'AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20261022/auto/s3/aws4_request, SignedHeaders=cache-control;host;x-amz-content-sha256;x-amz-date, Signature=3aebfdc65b7fb58dab49ef11eccfc2fb13503abb8b7a7769bcad6539e821d101',
  );
});

test("signs a header's value with its outer spaces trimmed and inner runs of them one", () => {
  const sign = (value) =>
    authorization({
      method: 'GET',
      path: '/',
      headers: { host: 'example.amazonaws.com', 'x-amz-date': '20150830T123600Z', 'x-amz-meta': value },
      payloadHash: 'e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855',
      region: 'us-east-1',
      service: 'service',
      ...KEY,
    });
  assert.equal(sign('  a   b \t c  '), sign('a b c'));
  assert.notEqual(sign('a b c'), sign('a  b c'.replace('  ', '_')));
});

test('encodes keys and stamps dates as SigV4 asks', () => {
  assert.equal(uriEncode("a b+c^d~e_f-g.h!i'j(k)l*m/n"), 'a%20b%2Bc%5Ed~e_f-g.h%21i%27j%28k%29l%2Am%2Fn');
  assert.equal(objectPath('bucket', 'rpm/x86_64/repodata/repomd.xml'), '/bucket/rpm/x86_64/repodata/repomd.xml');
  assert.equal(amzDate(new Date('2026-10-22T11:13:03.456Z')), '20261022T111303Z');
});

test('knows the metadata entry points from the content', () => {
  for (const key of [
    'deb/dists/nightly/InRelease',
    'deb/dists/nightly/Release',
    'deb/dists/nightly/Release.gpg',
    'rpm/x86_64/repodata/repomd.xml',
    'rpm/aarch64/repodata/repomd.xml.asc',
  ]) {
    assert.ok(isFlip(key), key);
  }
  for (const key of [
    'deb/dists/nightly/main/binary-amd64/Packages',
    'deb/dists/nightly/main/binary-amd64/Release',
    'deb/pool/main/Release',
    'rpm/x86_64/repomd.xml',
    'rpm/x86_64/repodata/primary.xml.gz',
    'pubkey.asc',
  ]) {
    assert.ok(!isFlip(key), key);
  }
  assert.deepEqual(FLIP, ['Release.gpg', 'Release', 'InRelease', 'repomd.xml.asc', 'repomd.xml']);
});

// --- The bucket ---------------------------------------------------------------

// A fake R2 bucket `bucket`: objects in `objects`, by key. `fail` maps a
// request, as `METHOD key`, to the statuses its first answers take.
async function fakeBucket({ bucket = 'packages', objects = new Map(), fail = {} } = {}) {
  const seen = {};
  const server = await serve((req, res) => {
    const url = new URL(req.url, server.url);
    const prefix = `/${bucket}/`;
    if (!url.pathname.startsWith(prefix)) {
      res.writeHead(404).end('<Error><Code>NoSuchBucket</Code></Error>');
      return;
    }
    const key = url.pathname.slice(prefix.length).split('/').map(decodeURIComponent).join('/');
    const what = `${req.method} ${key}`;
    seen[what] = (seen[what] ?? 0) + 1;
    const status = fail[what]?.[seen[what] - 1];
    if (status !== undefined) {
      res.writeHead(status).end('<Error><Code>InternalError</Code></Error>');
    } else if (req.method === 'PUT') {
      objects.set(key, req.body);
      res.writeHead(200).end();
    } else if (req.method === 'DELETE') {
      objects.delete(key);
      res.writeHead(204).end();
    } else if (objects.has(key)) {
      res.writeHead(200).end(objects.get(key));
    } else {
      res.writeHead(404).end('<Error><Code>NoSuchKey</Code></Error>');
    }
  });
  return { ...server, objects };
}

test('puts, gets and removes objects with signed requests R2 can check', async () => {
  const bucket = await fakeBucket();
  try {
    const now = () => new Date('2026-10-22T11:13:03Z');
    const store = bucketStore({ endpoint: bucket.url, bucket: 'packages', ...KEY, now });
    await store.put('deb/pool/main/a+b.deb', Buffer.from('deb'));
    assert.deepEqual(await store.get('deb/pool/main/a+b.deb'), Buffer.from('deb'));
    assert.equal(await store.get('missing'), null);
    await store.remove('deb/pool/main/a+b.deb');
    assert.equal(bucket.objects.size, 0);
    const [put, get, , remove] = bucket.requests;
    assert.equal(put.url, '/packages/deb/pool/main/a%2Bb.deb');
    assert.equal(put.headers['cache-control'], 'no-store');
    assert.equal(get.headers['cache-control'], undefined);
    for (const [request, body] of [
      [put, 'deb'],
      [get, ''],
      [remove, ''],
    ]) {
      const hash = createHash('sha256').update(body).digest('hex');
      assert.equal(request.headers['x-amz-content-sha256'], hash);
      assert.equal(request.headers['x-amz-date'], '20261022T111303Z');
      // What R2 computes from the request it received.
      const signed = request.headers.authorization.match(/SignedHeaders=([^,]+)/)[1].split(';');
      assert.deepEqual(signed, request.method === 'PUT' ? ['cache-control', 'host', 'x-amz-content-sha256', 'x-amz-date'] : ['host', 'x-amz-content-sha256', 'x-amz-date']);
      const headers = Object.fromEntries(signed.map((name) => [name, request.headers[name]]));
      assert.equal(
        request.headers.authorization,
        authorization({ method: request.method, path: request.url, headers, payloadHash: hash, region: 'auto', service: 's3', ...KEY }),
      );
    }
  } finally {
    await bucket.close();
  }
});

test('reads a missing bucket as an error, not as a first publish, and retries what may pass', async () => {
  const bucket = await fakeBucket({ fail: { 'PUT k': [500, 503] } });
  try {
    const store = (name) => bucketStore({ endpoint: bucket.url, bucket: name, ...KEY, delayMs: 1 });
    await assert.rejects(store('other').get(MANIFEST), { name: 'HttpError', status: 404 });
    await capture(() => store('packages').put('k', Buffer.from('v')));
    assert.deepEqual(bucket.objects.get('k'), Buffer.from('v'));
    assert.equal(bucket.requests.filter(({ method }) => method === 'PUT').length, 3);
  } finally {
    await bucket.close();
  }
});

// --- Publishing ---------------------------------------------------------------

// A site holding `files`, each holding its own name.
function site(files) {
  const root = path.join(tempDir(), 'site');
  for (const file of files) {
    mkdirSync(path.dirname(path.join(root, file)), { recursive: true });
    writeFileSync(path.join(root, file), file);
  }
  return root;
}

const TREE = [
  'deb/dists/nightly/InRelease',
  'deb/dists/nightly/Release',
  'deb/dists/nightly/Release.gpg',
  'deb/dists/nightly/main/binary-amd64/Packages',
  'deb/pool/main/new.deb',
  'pubkey.asc',
  'rpm/x86_64/new.rpm',
  'rpm/x86_64/repodata/repomd.xml',
  'rpm/x86_64/repodata/repomd.xml.asc',
];

// A store in memory that records each operation as `put key`, `get key` or
// `remove key`; `failRemove` lists keys whose removal fails.
function memoryStore(objects = {}, { failRemove = [], failGet = false } = {}) {
  const ops = [];
  const held = new Map(Object.entries(objects).map(([key, text]) => [key, Buffer.from(text)]));
  return {
    ops,
    held,
    async get(key) {
      ops.push(`get ${key}`);
      if (failGet) throw new Error('GET https://bucket/manifest.txt: 403 Forbidden');
      return held.get(key) ?? null;
    },
    async put(key, body) {
      ops.push(`put ${key}`);
      held.set(key, Buffer.from(body));
    },
    async remove(key) {
      ops.push(`remove ${key}`);
      if (failRemove.includes(key)) throw new Error(`DELETE ${key}: 500 Internal Server Error`);
      held.delete(key);
    },
  };
}

const lines = (...keys) => `${keys.sort().join('\n')}\n`;

test('publishes a first tree: manifest, content, the signed metadata last, then the generation to keep', async () => {
  const root = site([...TREE, 'deb/manifest.txt']);
  const store = memoryStore();
  const { value, stdout } = await capture(() => publishSite({ site: root, store }));
  assert.deepEqual(value, { published: TREE.length, live: TREE.length });
  assert.deepEqual(store.ops, [
    `get ${MANIFEST}`,
    `get ${MANIFEST_PREV}`,
    `put ${MANIFEST}`,
    'put deb/dists/nightly/main/binary-amd64/Packages',
    'put deb/pool/main/new.deb',
    'put pubkey.asc',
    'put rpm/x86_64/new.rpm',
    'put deb/dists/nightly/Release.gpg',
    'put deb/dists/nightly/Release',
    'put deb/dists/nightly/InRelease',
    'put rpm/x86_64/repodata/repomd.xml.asc',
    'put rpm/x86_64/repodata/repomd.xml',
    `put ${MANIFEST_PREV}`,
    `put ${MANIFEST}`,
  ]);
  assert.equal(store.held.get(MANIFEST).toString(), lines(...TREE));
  assert.equal(store.held.get(MANIFEST_PREV).toString(), lines(...TREE));
  assert.equal(store.held.get('deb/dists/nightly/InRelease').toString(), 'deb/dists/nightly/InRelease');
  assert.match(stdout, /No previous manifest\.txt on the bucket \(first publish\)/);
});

test('keeps the previous generation one more publish, and deletes what is older', async () => {
  const root = site(TREE);
  // Live: the generation before last (old.deb), the last (prev.deb), and
  // what both share (pubkey.asc). Kept: the last one.
  const store = memoryStore({
    [MANIFEST]: lines('deb/pool/main/old.deb', 'deb/pool/main/prev.deb', 'pubkey.asc', 'rpm/x86_64/old.rpm'),
    [MANIFEST_PREV]: lines('deb/pool/main/prev.deb', 'pubkey.asc'),
  });
  const { value } = await capture(() => publishSite({ site: root, store }));
  // The union goes up first, so a run that dies leaves nothing unlisted.
  assert.equal(store.ops[2], `put ${MANIFEST}`);
  assert.deepEqual(
    store.ops.filter((op) => op.startsWith('remove')),
    ['remove deb/pool/main/old.deb', 'remove rpm/x86_64/old.rpm'],
  );
  assert.equal(store.held.get(MANIFEST_PREV).toString(), lines(...TREE));
  assert.equal(store.held.get(MANIFEST).toString(), lines(...TREE, 'deb/pool/main/prev.deb'));
  assert.deepEqual(value, { published: TREE.length, live: TREE.length + 1 });
});

test('the union it writes first lists every key the run may make', async () => {
  const root = site(TREE);
  const store = memoryStore({ [MANIFEST]: lines('deb/pool/main/old.deb') });
  let union;
  const put = store.put;
  store.put = async (key, body) => {
    union ??= key === MANIFEST ? Buffer.from(body).toString() : undefined;
    return put(key, body);
  };
  await capture(() => publishSite({ site: root, store }));
  assert.equal(union, lines(...TREE, 'deb/pool/main/old.deb'));
});

test('a stale object it fails to delete stays listed, so the next run deletes it', async () => {
  const root = site(TREE);
  const store = memoryStore(
    { [MANIFEST]: lines('deb/pool/main/old.deb', 'deb/pool/main/older.deb') },
    { failRemove: ['deb/pool/main/old.deb'] },
  );
  const { stdout } = await capture(() => publishSite({ site: root, store }));
  assert.match(stdout, /::warning::could not delete stale deb\/pool\/main\/old\.deb: DELETE deb\/pool\/main\/old\.deb: 500/);
  assert.equal(store.held.get(MANIFEST).toString(), lines(...TREE, 'deb/pool/main/old.deb'));
  assert.ok(!store.held.has('deb/pool/main/older.deb'));
});

test('publishes nothing when it cannot read what the bucket lists', async () => {
  const store = memoryStore({}, { failGet: true });
  await assert.rejects(capture(() => publishSite({ site: site(TREE), store })), { message: /403 Forbidden/ });
  assert.deepEqual(store.ops, [`get ${MANIFEST}`]);
});

test('publishes no tree without the public key, which every rendered site has', async () => {
  for (const files of [TREE.filter((key) => key !== 'pubkey.asc'), [MANIFEST, MANIFEST_PREV]]) {
    const store = memoryStore({ [MANIFEST]: lines(...TREE) });
    const root = site(files);
    await assert.rejects(capture(() => publishSite({ site: root, store })), { message: `${root}/pubkey.asc missing: render the site first` });
    assert.deepEqual(store.ops, []);
  }
});
