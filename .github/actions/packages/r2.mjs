// nightly-packages.yml's publish job, last of the channels: the site
// repo.mjs rendered and verify-repo.mjs installed from replaces the one in
// the R2 bucket behind pkg.rustyphoton.space (docs/plans/archive/
// nightly-releases.md, N5). Conventions: docs/skills/ci-logic.md.
//
// Two objects of the bucket's own make the replacement safe without ever
// listing the bucket, so that nothing removes an object put there by other
// means: manifest.txt, every key that may be live, always a superset of
// what is; and manifest-prev.txt, the previous publish's tree, which is kept
// one more generation so that metadata a client has just read still
// resolves. In order:
//
//   1. read manifest.txt and manifest-prev.txt; only an answer that the
//      object does not exist stands for either being absent;
//   2. write manifest.txt as the union of what it listed and the tree about
//      to go up, so that every key this run may make is on record before it
//      exists, and a run that dies at any later point leaves nothing a
//      later run cannot sweep;
//   3. upload every object but the metadata entry points: the pool's debs,
//      the rpms, the indices and their by-hash copies, repodata, pubkey.asc.
//      This only adds, so the old metadata serves the old, whole tree
//      throughout;
//   4. upload the entry points last, as the flip, each detached signature
//      before the file it signs (Release.gpg, Release, InRelease for apt;
//      repomd.xml.asc, repomd.xml for dnf), so that a client reading a new
//      signed file finds its signature there already. A pair replaced in
//      place cannot be atomic in either order: a client that read the
//      outgoing file within the moment between the two puts pairs it with
//      the new signature and retries, where the other order would fail
//      every client between them. Current apt reads InRelease, clearsigned
//      in one object, and is never caught; dnf and older apt get past it on
//      a retry;
//   5. delete what manifest.txt listed that is neither in the new tree nor
//      in the generation kept. An object with a unique name (a pool file, a
//      by-hash index, a repodata blob) lives two publishes, so a client
//      that read the outgoing InRelease or repomd.xml finds everything it
//      names for a day; with apt's by-hash indices (repo.mjs) apt is never
//      caught at all, and only a client that reads the stable-named
//      Packages(.gz) keeps a window a retry closes;
//   6. write manifest-prev.txt as the new tree, the next run's generation
//      to keep;
//   7. write manifest.txt as the new tree, what step 5 kept, and what it
//      failed to delete, which the next run tries again.
//
// A run that dies anywhere heals: manifest.txt only grows ahead of the
// bucket (step 2) and shrinks after the deletes (step 7), so no key is ever
// live and unlisted. The worst a death does is shorten the outgoing
// generation's grace, and the next run sweeps it.
//
// Every object goes up with Cache-Control: no-store. Cloudflare's edge
// caches .gz by default, and a stale cached Packages.gz against a fresh
// InRelease is a hash mismatch for the client. Every read goes to R2, whose
// egress is free, for a channel a handful of rigs read.
//
// Auth: R2's S3 API, with the S3 credentials of a bucket-scoped "Object
// Read & Write" API token, each request signed with AWS Signature Version 4.
// The S3 API is the only one such a token can use: Cloudflare's REST API,
// which wrangler drives, takes account-wide Admin R2 tokens alone, far too
// much for a nightly publisher.

import { createHash, createHmac } from 'node:crypto';
import { existsSync } from 'node:fs';
import { readFile } from 'node:fs/promises';
import path from 'node:path';

import { fetchWithRetry, HttpError, io, warning } from '../lib/helpers.mjs';
import { filesUnder } from './repo.mjs';

/** The bucket's bookkeeping objects. */
export const MANIFEST = 'manifest.txt';
export const MANIFEST_PREV = 'manifest-prev.txt';

/** The metadata entry points, in the order step 4 uploads them. */
export const FLIP = ['Release.gpg', 'Release', 'InRelease', 'repomd.xml.asc', 'repomd.xml'];

/** Whether `key` is a metadata entry point, which goes up last. */
export function isFlip(key) {
  const name = path.posix.basename(key);
  if (['Release.gpg', 'Release', 'InRelease'].includes(name)) return /^deb\/dists\/[^/]+\/[^/]+$/.test(key);
  if (['repomd.xml', 'repomd.xml.asc'].includes(name)) return /^rpm\/[^/]+\/repodata\/[^/]+$/.test(key);
  return false;
}

// --- Signature Version 4 --------------------------------------------------------

/** `text` encoded as SigV4 asks: everything but RFC 3986's unreserved characters. */
export function uriEncode(text) {
  return encodeURIComponent(text).replace(/[!'()*]/g, (c) => `%${c.charCodeAt(0).toString(16).toUpperCase()}`);
}

/** The path of an object `key` in `bucket`, each segment encoded. */
export function objectPath(bucket, key) {
  return `/${uriEncode(bucket)}/${key.split('/').map(uriEncode).join('/')}`;
}

/** `date` as SigV4 stamps it: 20150830T123600Z. */
export function amzDate(date) {
  return date.toISOString().replace(/[-:]/g, '').replace(/\.\d{3}/, '');
}

function sha256Hex(bytes) {
  return createHash('sha256').update(bytes).digest('hex');
}

function hmac(key, text) {
  return createHmac('sha256', key).update(text).digest();
}

/**
 * The Authorization header that signs a request with SigV4: `method` on the
 * already-encoded `path`, with no query, with `headers` (lowercase names,
 * every one signed, `host` and `x-amz-date` among them) and a payload of
 * SHA-256 `payloadHash`, for `region` and `service`, with the key
 * `accessKeyId` and `secretAccessKey`.
 */
export function authorization({ method, path: canonicalPath, headers, payloadHash, region, service, accessKeyId, secretAccessKey }) {
  const names = Object.keys(headers).sort();
  const canonicalHeaders = names.map((name) => `${name}:${String(headers[name]).trim().replace(/\s+/g, ' ')}\n`).join('');
  const signedHeaders = names.join(';');
  const canonical = [method, canonicalPath, '', canonicalHeaders, signedHeaders, payloadHash].join('\n');
  const stamp = headers['x-amz-date'];
  const scope = `${stamp.slice(0, 8)}/${region}/${service}/aws4_request`;
  const toSign = ['AWS4-HMAC-SHA256', stamp, scope, sha256Hex(canonical)].join('\n');
  let key = hmac(`AWS4${secretAccessKey}`, stamp.slice(0, 8));
  for (const part of [region, service, 'aws4_request']) key = hmac(key, part);
  const signature = createHmac('sha256', key).update(toSign).digest('hex');
  return `AWS4-HMAC-SHA256 Credential=${accessKeyId}/${scope}, SignedHeaders=${signedHeaders}, Signature=${signature}`;
}

// --- The bucket -------------------------------------------------------------------

// How long one request may take, its body included: an rpm is a few MB.
const TIMEOUT_MS = 120_000;

/**
 * R2's S3 API on `bucket` at `endpoint`, path-style, signed with the token's
 * `accessKeyId` and `secretAccessKey`. `get` resolves to an object's bytes,
 * or null where R2 says it does not exist; `put` and `remove` to nothing. A
 * request failing on what may pass is retried (fetchWithRetry); any other
 * answer outside 2xx rejects.
 */
export function bucketStore({ endpoint, bucket, accessKeyId, secretAccessKey, now = () => new Date(), delayMs }) {
  const request = async (method, key, { body, headers: extra = {} } = {}) => {
    const url = new URL(objectPath(bucket, key), endpoint);
    const payloadHash = sha256Hex(body ?? '');
    const headers = { 'x-amz-content-sha256': payloadHash, 'x-amz-date': amzDate(now()), ...extra };
    const signed = { ...headers, host: url.host };
    headers.authorization = authorization({
      method,
      path: url.pathname,
      headers: signed,
      payloadHash,
      region: 'auto',
      service: 's3',
      accessKeyId,
      secretAccessKey,
    });
    return fetchWithRetry(url, { method, headers, body, timeoutMs: TIMEOUT_MS, delayMs });
  };
  return {
    async get(key) {
      try {
        return Buffer.from(await (await request('GET', key)).arrayBuffer());
      } catch (err) {
        // A missing bucket answers 404 too, as NoSuchBucket: that is no
        // first publish, and must not read as one.
        if (err instanceof HttpError && err.status === 404 && err.body?.includes('NoSuchKey')) return null;
        throw err;
      }
    },
    async put(key, body) {
      await request('PUT', key, { body, headers: { 'cache-control': 'no-store' } });
    },
    async remove(key) {
      await request('DELETE', key);
    },
  };
}

// --- Publishing -------------------------------------------------------------------

// A manifest's keys.
function keys(text) {
  return new Set(text.split('\n').filter(Boolean));
}

// A manifest of `set`'s keys, sorted, a line each.
function manifest(set) {
  const sorted = [...set].sort();
  return Buffer.from(sorted.length ? `${sorted.join('\n')}\n` : '');
}

/**
 * Replaces the tree `store` holds with the files under `site`, in the order
 * this module's header gives. Resolves to the counts of objects published
 * and live.
 */
export async function publishSite({ site, store }) {
  // A tree without the public key is not a rendered site: published, it
  // would replace the live repositories, and the next run would sweep them.
  if (!existsSync(path.join(site, 'pubkey.asc'))) throw new Error(`${site}/pubkey.asc missing: render the site first`);
  const put = async (key, body) => {
    await store.put(key, body);
    io.stdout(`  put ${key}\n`);
  };
  const read = async (key) => store.get(key).then((bytes) => (bytes ? keys(bytes.toString('utf8')) : null));

  // 1.
  const old = await read(MANIFEST);
  io.stdout(old ? `Previous manifest: ${old.size} objects\n` : `No previous ${MANIFEST} on the bucket (first publish)\n`);
  const prev = await read(MANIFEST_PREV);
  io.stdout(prev ? `Retained generation: ${prev.size} objects\n` : `No ${MANIFEST_PREV} on the bucket (nothing retained yet)\n`);
  const listed = old ?? new Set();
  const kept = prev ?? new Set();
  const tree = (await filesUnder(site)).filter((key) => ![MANIFEST, MANIFEST_PREV].includes(path.posix.basename(key)));
  const fresh = new Set(tree);

  // 2.
  await put(MANIFEST, manifest(new Set([...listed, ...fresh])));
  // 3.
  io.stdout('Uploading content objects...\n');
  for (const key of tree.filter((key) => !isFlip(key))) await put(key, await readFile(path.join(site, key)));
  // 4.
  io.stdout('Flipping metadata...\n');
  for (const name of FLIP) {
    for (const key of tree.filter((key) => isFlip(key) && path.posix.basename(key) === name)) {
      await put(key, await readFile(path.join(site, key)));
    }
  }
  // 5.
  const stale = [...listed].filter((key) => !fresh.has(key) && !kept.has(key)).sort();
  io.stdout(stale.length ? `Deleting ${stale.length} stale objects...\n` : 'No stale objects to delete\n');
  const undeleted = [];
  for (const key of stale) {
    try {
      await store.remove(key);
      io.stdout(`  deleted ${key}\n`);
    } catch (err) {
      // Listed still, so that the next run deletes it.
      warning(`could not delete stale ${key}: ${err.message}`);
      undeleted.push(key);
    }
  }
  // 6.
  await put(MANIFEST_PREV, manifest(fresh));
  // 7.
  const live = new Set([...fresh, ...[...listed].filter((key) => kept.has(key)), ...undeleted]);
  await put(MANIFEST, manifest(live));
  io.stdout(`\npublish-repo: OK (${fresh.size} objects published, ${live.size} live)\n`);
  return { published: fresh.size, live: live.size };
}
