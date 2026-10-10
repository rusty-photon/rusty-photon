# Bazel remote cache — Cloudflare Worker + R2

A serverless Bazel HTTP remote cache for **GitHub cloud CI**, served from
Cloudflare's edge and backed by [R2](https://developers.cloudflare.com/r2/)
(S3-compatible object storage, **zero egress fees**).

## Overview

Bazel's HTTP cache protocol is a simple keyed blob store (`GET`/`PUT` on
`/ac/<hash>` and `/cas/<hash>`). This Worker maps it onto an R2 bucket and
serves it from Cloudflare's edge, so cloud CI gets fast cache reads at zero
egress cost. Access is public-read / token-write (below), served at
`cache.rustyphoton.space`.

For **local builds**, point a (gitignored) `user.bazelrc` at a LAN
`bazel-remote` instead, which reads at LAN speed:

```
build:remote-cache --remote_cache=http://<your-lan-cache-host>:8088
```

| Consumer | Cache |
|---|---|
| GitHub cloud CI (`bazel.yml`) | this Worker + R2 (`cache.rustyphoton.space`) |
| Local builds | your own LAN `bazel-remote` (via `user.bazelrc`) |

## Security model (public repo)

- **GET/HEAD = anonymous** — every PR (incl. forks) gets a warm cache.
- **PUT = `Authorization: Bearer <WRITE_TOKEN>`** — only push-to-main / nightly
  (which hold the GitHub secret) can write, so forks can't poison the cache.

The `WRITE_TOKEN` Worker secret must equal the `BAZEL_CACHE_WRITE_TOKEN` GitHub
Actions secret (`bazel.yml` already sends `Authorization: Bearer` on push/schedule).

## Deploy (one-time)

Prereqs: a Cloudflare account with the `rustyphoton.space` zone, R2 enabled
(free), and `wrangler` (`npm i -g wrangler`, or `npx wrangler`).

```bash
cd tools/bazel-cache-worker

# 1. Create the bucket.
wrangler r2 bucket create rusty-photon-bazel-cache

# 2. Set the write token (paste the SAME value as the GitHub secret
#    BAZEL_CACHE_WRITE_TOKEN; don't echo it into shell history).
wrangler secret put WRITE_TOKEN

# 3. Make sure no other Cloudflare DNS record/route already claims
#    cache.rustyphoton.space (remove it if so), then deploy — this provisions
#    the Worker + the cache.rustyphoton.space custom domain.
wrangler deploy
```

### Retention / eviction

R2 has no built-in LRU. Growth is bounded by an **object lifecycle rule**
(Cloudflare dashboard → R2 → `rusty-photon-bazel-cache` → Settings → Object
lifecycle), currently **delete objects older than 7 days**. On its own that
rule would be an age bomb, not an LRU: R2 expires by *upload* time, and Bazel
never re-uploads an entry that keeps getting cache hits, so every object would
die a fixed time after it was last *built* — the stable core of the graph
expiring en masse once per window, followed by a cold rebuild. (The nightly
main build does not help here: an all-hit build uploads nothing.)

The Worker therefore **touches on read**: a GET of an object older than 2 days
re-puts it under the same key (in `ctx.waitUntil`, off the response path),
resetting its lifecycle clock. Net effect: age expiry becomes effective LRU —
anything read within the window survives, genuinely unused entries age out.
Cost bound: at most one Class A write per read object per 2 days (~$1–4/mo at
this repo's scale). If the lifecycle window changes, keep
`TOUCH_AFTER_MS + CAS_EDGE_TTL_S` (src/cache.js) comfortably below it — the
edge cache (next section) adds its TTL to the worst-case gap between an
object's R2 touches.

**AC-referenced CAS touch.** Touch-on-read alone only touches a `/cas/` blob
Bazel actually *downloads*. `--remote_download_outputs=toplevel`
(build-without-the-bytes) means a hit-heavy build skips downloading most
intermediate/tool outputs whenever nothing local needs the bytes — so a
steady-state dependency (a build-script action nothing else forces a fresh
compile of) stays "used" via its `/ac/` entry every day while the CAS content
backing it quietly ages past the 7-day window underneath it. This bit
2026-07-14: `aws-lc-sys`/`aws-lc-rs`'s build-script outputs went missing
("Lost inputs no longer available remotely") ahead of a routine rustls
0.23.42 bump, despite green daily builds, because nothing had forced a real
download of that corner of the graph in over a week.

The Worker closes this by touching an AC entry's *referenced* CAS digests on
every read of the entry itself, regardless of whether Bazel downloads them:
an `ActionResult`'s output `Digest.hash` fields are length-delimited UTF-8
strings, so a lossless latin1 decode of the entry's raw bytes plus a
64-hex-char regex recovers every digest it points to, without a full REAPI
protobuf parser (see `touchReferencedCas` in `src/cache.js`). `/ac/` entries
are small (a handful of output digests), so buffering one to scan it costs
nothing next to the CAS blobs' own streaming path. Residual risk: Bazel 9's
default eviction retries (rewind + rebuild) still backstop any digest this
heuristic misses — entries are regenerable either way.

### Edge cache (`/cas/` reads)

CAS keys are content hashes — a key's bytes can never legitimately change —
so the Worker serves `/cas/` GETs from Cloudflare's per-datacenter cache
(`caches.default`, `max-age` 1 day) and only falls through to R2 on a miss
(with one retry on transient R2 read errors). Its readers are the jobs on
GitHub-hosted runners: the macOS leg (push-to-main and the nightly schedule;
`bazel.yml` drops macOS from every PR), and the Linux/Windows legs and the
coverage job on the nightly schedule, on fork PRs, and whenever a pool kill
switch is off — otherwise push-to-main and same-repo PRs route those to the
self-hosted pool and its LAN cache. Legs pulling the same blobs within hours
of each other are served at edge latency, and an R2
latency spike or transient error can't stall an edge-cached read. `/ac/`
entries are mutable (a re-executed action re-uploads under the same key), so
they always read R2 and are never edge-cached.

On an edge miss the client's response streams straight from R2, and the
edge copy is written from a *second*, independent R2 read — not by tee-ing
the client's body with `response.clone()`. A tee makes the runtime buffer
whatever the slower branch hasn't consumed, with no limit and no
backpressure; the slower branch is Bazel, and the blobs that miss are the
big test binaries (~88 MiB for `rp_unit_test`) against a 128 MB isolate
memory limit. The second read costs one Class B operation per edge miss.
This was changed as a suspect in the macOS `bazel build` wedge
([#765](https://github.com/rusty-photon/rusty-photon/issues/765)) — on
push-to-main, with the pool switches on, macOS is the only leg that still
reads this Worker.

Interplay with retention: an edge hit never reaches R2, so it cannot touch —
a hot blob's R2 clock only advances on the origin reads between TTL
expiries. The 1-day TTL bounds that: worst-case R2 age of a live object is
2d (touch threshold) + 1d (edge TTL) + 1d (nightly read gap) ≈ 4d, inside
the 7-day lifecycle window with margin.

## Verify

```bash
curl -sf https://cache.rustyphoton.space/status                       # -> ok
H=0000000000000000000000000000000000000000000000000000000000000000
curl -s -o/dev/null -w '%{http_code}\n' -X PUT --data x \
  https://cache.rustyphoton.space/cas/$H                              # -> 403 (no token)
curl -s -o/dev/null -w '%{http_code}\n' -X PUT --data x \
  -H "Authorization: Bearer <WRITE_TOKEN>" \
  https://cache.rustyphoton.space/cas/$H                              # -> 200
```

On the next push-to-main, `cache.rustyphoton.space` fills from the edge and PR
reads come back fast (`bazel.yml` shows remote cache hits without long
`Downloading …` stalls).

## Cost

Effectively free. R2: 10 GB storage free, then ~$0.015/GB·mo, **$0 egress** (the
whole point — cache reads don't cost). Workers: 100k requests/day free; a very
busy CI day could tip into the $5/mo Workers plan (10M requests). Debuginfo is
stripped in `.bazelrc`, so individual CAS blobs stay well under Cloudflare's
100 MB request-body (write) limit.
