// Bazel HTTP remote cache backed by Cloudflare R2.
//
// Implements the Bazel HTTP cache protocol (GET/HEAD/PUT on /ac/<hash> and
// /cas/<hash>) over an R2 bucket, served from Cloudflare's edge, so cloud CI
// reads it at edge speed (with R2's zero egress cost). Hot /cas/ blobs are
// served straight from the per-PoP edge cache with no R2 round trip at all
// (see CAS_EDGE_TTL_S below).
//
// Security (public repo): reads are ANONYMOUS, so every PR including forks gets
// a warm cache; writes require a Bearer token that only push-to-main / nightly
// hold, so forks can never poison the cache. For local builds, point
// user.bazelrc at a LAN bazel-remote instead. See README.md.
//
// Bindings (wrangler.toml): CACHE = R2 bucket.  Secret: WRITE_TOKEN.

const KEY_RE = /^(ac|cas)\/[0-9a-fA-F]+$/;

// Touch-on-read. The bucket's lifecycle rule deletes objects by UPLOAD age,
// but Bazel never re-uploads an entry that keeps getting cache hits, so
// without a touch every object dies a fixed time after it was last BUILT, not
// last USED — the whole stable core of the action graph expires en masse once
// per lifecycle window and the next builds run cold. Re-putting a read object
// resets its lifecycle clock, turning age-based expiry into effective LRU:
// anything read within the window survives.
//
// Only objects older than TOUCH_AFTER_MS are re-put, bounding the Class A
// (write) cost to at most one write per read object per 2 days. The safety
// constraint is TOUCH_AFTER + the longest read gap of a live object < the
// lifecycle window (7 days); the nightly full build of main reads the entire
// //... action-cache set daily, so 2 days leaves wide margin. This alone only
// touches CAS blobs Bazel actually downloads -- build-without-the-bytes
// (--remote_download_outputs=toplevel) skips most intermediate/tool outputs
// whenever a hit-heavy build never needs the bytes locally, so a steady-state
// dependency's AC entry stays warm while its backing CAS content silently
// ages out underneath it (confirmed 2026-07-14: aws-lc-sys/aws-lc-rs went
// missing ahead of the rustls 0.23.42 bump despite daily green builds).
// touchReferencedCas below closes that gap by touching an AC entry's
// referenced outputs on every read of the entry itself, not just on download.
const TOUCH_AFTER_MS = 2 * 24 * 60 * 60 * 1000;

// Regex for a SHA-256 hex digest (64 hex chars; case-insensitive, matching
// KEY_RE's [0-9a-fA-F] -- REAPI digests are conventionally lowercase, but
// nothing about the wire format forbids uppercase, and an unmatched digest
// just silently keeps aging out). See touchReferencedCas below for why this
// is the touch mechanism for CAS blobs an AC hit never downloads.
const DIGEST_RE = /[0-9a-fA-F]{64}/g;

// Edge-cache TTL for /cas/ reads. CAS keys are content hashes — a key's bytes
// can never legitimately change — so serving them from Cloudflare's
// per-datacenter cache needs no invalidation story (even a touch re-put
// writes identical content). The constraint is retention interplay: an edge
// hit never reaches R2, so it cannot touch — a hot blob's R2 clock only
// advances on the origin reads between TTL expiries. Worst-case R2 age of a
// live object is TOUCH_AFTER (2d) + this TTL (1d) + the nightly read gap
// (1d) ≈ 4d, comfortably inside the 7-day lifecycle window; keep
// TOUCH_AFTER_MS + CAS_EDGE_TTL_S well below that window if either changes.
// A longer TTL would buy little anyway: CI reads are bursty (all legs of a
// run land within hours), and the next origin read re-primes the PoP.
const CAS_EDGE_TTL_S = 24 * 60 * 60;

// One immediate retry on R2 read exceptions. Transient R2 errors exist, and
// to Bazel a failed cache read becomes a rebuilt action — a second read is
// far cheaper than that.
async function r2Get(env, key) {
  try {
    return await env.CACHE.get(key);
  } catch {
    return env.CACHE.get(key);
  }
}

async function touch(env, key, seenUploadedMs) {
  // Fresh get(): the client response consumes its own body stream. put()
  // accepts the R2ObjectBody stream directly (its length is known).
  const obj = await env.CACHE.get(key);
  // Gone, or already refreshed/replaced since the GET that scheduled this
  // touch (a sibling touch from a GET burst on the same stale key, or a real
  // push-to-main PUT) -> nothing to do. Collapses duplicate touches to one
  // Class A write and keeps a touch from resurrecting a replaced entry.
  if (!obj || obj.uploaded.getTime() !== seenUploadedMs) return;
  // The conditional write closes the remaining get->put window: if anything
  // replaced the object in between, the etag no longer matches and R2 drops
  // the touch (put resolves null; fine — the object is fresh either way).
  await env.CACHE.put(key, obj.body, { onlyIf: { etagMatches: obj.etag } });
}

// Re-puts an object whose body/etag the caller already fetched (no second
// get()) -- the conditional write still closes the get->put race the same
// way touch() does.
async function touchObj(env, key, body, etag) {
  await env.CACHE.put(key, body, { onlyIf: { etagMatches: etag } });
}

// Populates the edge cache from a fresh R2 read, independent of the client's
// response stream (see the /cas/ GET path for why not res.clone()). The key is
// a content hash, so the copy is byte-identical to what the client got.
async function edgePut(env, request, key, headers) {
  const obj = await r2Get(env, key);
  if (!obj) return;
  await caches.default.put(request, new Response(obj.body, { status: 200, headers }));
}

// Bazel checks the action cache on every action, hit or not -- but an AC hit
// alone never reads (and so never touches) the CAS blobs its ActionResult
// points to: build-without-the-bytes (--remote_download_outputs=toplevel)
// skips downloading non-toplevel/tool outputs whenever nothing local needs
// the actual bytes. A steady-state dependency nothing else forces a fresh
// compile of (aws-lc-sys, cc, cmake, ...) can be "used" daily via its AC
// entry while the CAS content backing it quietly ages past the 7-day
// lifecycle window underneath it -- this is what happened ahead of the
// rustls 0.23.42 bump (2026-07-14): the AC hit was there, the bytes weren't.
//
// Close the gap without a full REAPI protobuf parser: an ActionResult's
// output Digest.hash fields are length-delimited UTF-8 strings, so they
// appear as literal 64-char hex runs in the entry's raw wire bytes. A
// lossless latin1 decode (byte N <-> char code N, no replacement) plus
// DIGEST_RE recovers them. A false match would need 64 contiguous bytes to
// each land in the 32-value [0-9a-fA-F] range -- astronomically unlikely in
// unrelated protobuf framing -- and the failure mode is benign either way:
// a bogus key just misses in r2Get below and is skipped.
async function touchReferencedCas(env, acBytes) {
  const text = new TextDecoder("latin1").decode(acBytes);
  const seen = new Set();
  for (const [rawHash] of text.matchAll(DIGEST_RE)) {
    const hash = rawHash.toLowerCase();
    if (seen.has(hash)) continue;
    seen.add(hash);
    const casKey = `cas/${hash}`;
    // r2Get already has the body + etag this touch needs -- unlike touch()
    // above (called where the caller's own obj.body is already spoken for
    // by the response stream), nothing else here reads this obj, so a
    // direct touchObj skips a second, redundant get().
    const obj = await r2Get(env, casKey);
    if (obj && Date.now() - obj.uploaded.getTime() > TOUCH_AFTER_MS) {
      await touchObj(env, casKey, obj.body, obj.etag);
    }
  }
}

export default {
  async fetch(request, env, ctx) {
    const key = new URL(request.url).pathname.replace(/^\/+/, "");

    // Health check for humans; Bazel never hits this.
    if (key === "" || key === "status") {
      return new Response("bazel-cache (cloudflare worker + r2) ok\n", { status: 200 });
    }
    if (!KEY_RE.test(key)) {
      return new Response("not a bazel cache key\n", { status: 400 });
    }

    switch (request.method) {
      case "GET": {
        // /cas/ is immutable -> edge-cacheable: a hit never reaches R2, so an
        // R2 latency spike or transient error can't stall the read. /ac/
        // entries are mutable (a re-executed action re-uploads under the same
        // key), so they always read R2.
        const edgeable = key.startsWith("cas/");
        if (edgeable) {
          const hit = await caches.default.match(request);
          if (hit) return hit;
        }
        const obj = await r2Get(env, key);
        if (!obj) return new Response(null, { status: 404 });
        const stale = Date.now() - obj.uploaded.getTime() > TOUCH_AFTER_MS;

        // /ac/ entries are small ActionResult protos (a handful of output
        // digests), unlike /cas/ blobs which can be tens of MB and stream
        // straight through -- so buffer them once and reuse it both to
        // touch the entry itself (no second get(), unlike the /cas/ path
        // below whose obj.body is already spoken for by the response
        // stream) and to scan for referenced-output digests to touch
        // (touchReferencedCas above). This is the fix for the
        // build-without-the-bytes touch gap: reading the metadata now also
        // keeps the bytes it describes alive, regardless of whether Bazel
        // itself ever downloads them.
        if (key.startsWith("ac/")) {
          const bytes = await obj.arrayBuffer();
          if (stale) ctx.waitUntil(touchObj(env, key, bytes, obj.etag));
          ctx.waitUntil(touchReferencedCas(env, bytes));
          return new Response(bytes, { status: 200, headers: { "Content-Length": String(obj.size) } });
        }

        if (stale) ctx.waitUntil(touch(env, key, obj.uploaded.getTime()));
        // Content-Length gives Bazel a sized body instead of a chunked
        // stream; Cache-Control is what makes cache.put store the response.
        const headers = { "Content-Length": String(obj.size) };
        if (edgeable) headers["Cache-Control"] = `public, max-age=${CAS_EDGE_TTL_S}`;
        // The edge write reads its own copy from R2 rather than tee-ing the
        // client's body with res.clone(). A tee makes the runtime buffer
        // whatever the slower branch hasn't read yet -- unbounded, no
        // backpressure -- and the slower branch is the client: a multi-MiB
        // test binary (rp_unit_test is ~88 MiB) read slowly by Bazel while
        // cache.put drains fast lands most of the blob in isolate memory
        // (128 MB limit), with touch() above reading it again on stale
        // keys. Two independent streams each go at their own pace; the cost
        // is one extra Class B read per edge miss. A suspect in the macOS
        // bazel-build wedge (#765), the one leg that still reads this Worker.
        if (edgeable) ctx.waitUntil(edgePut(env, request, key, headers));
        return new Response(obj.body, { status: 200, headers });
      }
      case "HEAD": {
        const obj = await env.CACHE.head(key);
        return new Response(null, { status: obj ? 200 : 404 });
      }
      case "PUT": {
        // Token-gated. The token is a Worker secret; only trusted CI events
        // (push-to-main, nightly) send it. Fork PRs get no secret -> read-only.
        if ((request.headers.get("Authorization") || "") !== `Bearer ${env.WRITE_TOKEN}`) {
          return new Response("forbidden\n", { status: 403 });
        }
        // Bazel sends Content-Length, so request.body is a fixed-length stream;
        // R2 streams it straight to storage (no buffering -> large blobs OK,
        // subject to Cloudflare's 100 MB request-body limit — see README).
        await env.CACHE.put(key, request.body);
        return new Response(null, { status: 200 });
      }
      default:
        return new Response("method not allowed\n", { status: 405 });
    }
  },
};
