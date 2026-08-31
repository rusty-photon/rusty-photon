# Skill: Bazel Remote Cache

## When to Read This

- Diagnosing Bazel cache misses, slow `Downloading …` stalls, or `403` on upload in CI.
- Changing or redeploying the remote cache.
- Auditing the cache's security posture on this **public** repo.

## What it is

Cloud CI (`.github/workflows/bazel.yml`) uses a Bazel HTTP remote cache served
by a **Cloudflare Worker backed by R2**, at `https://cache.rustyphoton.space`.
Code, config, and deploy steps live in
[`tools/bazel-cache-worker/`](../../tools/bazel-cache-worker/README.md).
Retention is an R2 lifecycle rule (delete after 7 days) made into effective
LRU by the Worker's **touch-on-read** (a GET of an object older than 2 days
re-puts it, resetting its expiry clock) — see the Worker README's
"Retention / eviction" for the full model. `/cas/` GETs are additionally
served from Cloudflare's per-PoP **edge cache** (1-day TTL; immutable
content-addressed blobs, so no invalidation — the TTL is counted into the
retention margin, see the README's "Edge cache"). Worker changes only take
effect after `wrangler deploy` (manual; no CI deploy step).

For **local builds**, point a gitignored `user.bazelrc` at a LAN `bazel-remote`
(reads at LAN speed):

```
build:remote-cache --remote_cache=http://<your-lan-cache-host>:8088
```

## LAN pool: repository fetches through the Remote Asset API

The self-hosted pool's LAN bazel-remote (on the operator's NAS) can also
serve Bazel's repository *downloads* — crates.io archives, Rust toolchains,
BCR modules — via its experimental Remote Asset API, gated on the
`RP_LAN_ASSET_API` repo variable. When it is `on`, `bazel.yml` /
`bazel-coverage.yml` pool jobs switch their cache channel to the cache
host's gRPC endpoint and add `--experimental_remote_downloader` (Bazel
requires gRPC caching alongside a remote downloader; the asset API is a
gRPC-only protocol, so there is no HTTP variant of this). The cloud Worker
cache is unaffected — it speaks HTTP and hosted runners keep using it
exactly as described here. Semantics, enable order (NAS first, variable
second — an unreachable gRPC endpoint fails builds rather than degrading),
and the rollback flip live in
[docs/skills/proxmox-runner-pool.md](proxmox-runner-pool.md).

## Security model (public repo)

The cache stores build/test outputs of public code, so:

- **Reads (GET/HEAD) are anonymous** — every PR, including forks, gets a warm cache.
- **Writes (PUT) require `Authorization: Bearer <token>`** — the token is a
  GitHub Actions secret (`BAZEL_CACHE_WRITE_TOKEN`) exposed only to `push`-to-main
  and the nightly `schedule`, never to PRs or forks. So only trusted,
  main-derived runs can populate the cache — no poisoning path.

`bazel.yml` attaches the Bearer token only on push/schedule and adds
`--remote_upload_local_results=false` on PRs (read-only). Reads need no
credentials, so fork PRs benefit too.

## Repo wiring

- `.bazelrc` — `build:remote-cache --remote_cache=https://cache.rustyphoton.space`; debuginfo is stripped (`-Cdebuginfo=0`) to keep cached blobs small.
- `bazel.yml` — sends the Bearer token on trusted events; reads anonymously otherwise.
- The GitHub secret `BAZEL_CACHE_WRITE_TOKEN` must match the Worker's `WRITE_TOKEN`.

## Verify

```bash
curl -sf https://cache.rustyphoton.space/status                       # -> ok
H=0000000000000000000000000000000000000000000000000000000000000000
curl -s -o/dev/null -w '%{http_code}\n' -X PUT --data x \
  https://cache.rustyphoton.space/cas/$H                              # -> 403 (no token)
```

A healthy CI run shows `… processes: N remote cache hit` without long
`Downloading …` stalls.

## QHYCCD SDK (qhy-camera) — not on this cache

The QHYCCD SDK that `qhy-camera` links (`static=qhyccd`, pinned 26.06.04) does
**not** go through this cache. It is publicly downloadable from qhyccd.com, and
the GitHub-hosted ubuntu/macOS/Windows jobs install it via the author's
published `ivonnyssen/qhyccd-sdk-install@v3` action (which wraps the download —
on Linux the 26.x packaging ships no `install.sh`, so it copies the staged tree
into `/usr/local/lib`; macOS/Windows extract into the workspace — and caches it)
— no secret, no SHA pin, no internal tier. The Pi nightly pre-provisions it from
qhyccd.com via `scripts/setup-pi-runner.sh` (v3 now covers linux-arm64, but the
Pi runner is sudo-less, so it installs at provisioning time rather than per-run).
See `docs/services/qhy-camera.md` "Native dependency & build gating".

## References

- [tools/bazel-cache-worker/](../../tools/bazel-cache-worker/README.md) — Worker code + deploy runbook.
- [docs/plans/archive/bazel-migration.md](../plans/archive/bazel-migration.md) — migration status.
