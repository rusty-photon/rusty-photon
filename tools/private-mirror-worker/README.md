# Private CI file mirror — Cloudflare Worker + R2

A private, token-gated file host for **vendor blobs CI needs but cannot
fetch from the vendor directly** — downloads sitting behind a
captcha/consent gate with no scriptable URL. First occupant: SVBony's
Windows camera SDK (see
[ADR-018](../../docs/decisions/018-svbony-sdk-no-license-payload-policy.md)),
whose Linux/macOS blobs CI fetches from indi-3rdparty's public mirror but
whose Windows zip is captcha-gated on svbony.com.

Served at `https://private.rustyphoton.space/<key>`.

## What this is NOT

- **Not publication.** Nothing here is redistributed: every request —
  reads included — requires the bearer token, so the contents are reachable
  only by our own CI and operators. Published artifacts (packages, MSI,
  releases) still never contain these files (ADR-018).
- **Not writable from the network.** The Worker has no PUT path at all;
  uploads happen out-of-band via an operator's authenticated `wrangler`
  session. A leaked read token cannot mutate or poison the mirror.
- **Not a cache.** No lifecycle rule; objects persist until deleted by
  hand. Nothing expires, nothing needs re-seeding, no token rotation is
  scheduled (the token guards read access to blobs any operator could
  download from the vendor by hand — rotate opportunistically if it leaks).

## Security model (public repo)

- **GET/HEAD = `Authorization: Bearer <READ_TOKEN>`** — required for every
  request; unauthenticated callers get 403 before any key lookup, so key
  existence never leaks.
- The `READ_TOKEN` Worker secret must equal the `PRIVATE_MIRROR_TOKEN`
  GitHub Actions secret. Fork PRs and dependabot runs don't receive repo
  secrets, so any CI step using the mirror must degrade gracefully when the
  secret is absent (e.g. fall back to `SVBONY_SKIP_NATIVE_LINK=1`).

## Layout

Keys are `<vendor>/<original-upstream-filename>`, keeping the vendor's own
name-embedded version so a listing is self-describing:

```
svbony/windows-SVBCameraSDK-v1.13.4.zip
svbony/SVBONY-Driver-DS-V1.13.4-20250205.exe
```

Consumers pin each object's sha256 (e.g. `WINDOWS_SDK_SHA256`, beside
`WINDOWS_SDK_VERSION`, in `.github/actions/install-svbony-sdk/svbony-sdk.mjs`)
— the mirror is trusted for availability, not integrity.

## Deploy (one-time)

Prereqs: a Cloudflare account with the `rustyphoton.space` zone, R2
enabled, and `wrangler`.

```bash
cd tools/private-mirror-worker

# 1. Create the bucket.
wrangler r2 bucket create rusty-photon-private-mirror

# 2. Set the read token (paste the SAME value as the GitHub secret
#    PRIVATE_MIRROR_TOKEN; don't echo it into shell history).
wrangler secret put READ_TOKEN

# 3. Make sure no other Cloudflare DNS record/route already claims
#    private.rustyphoton.space, then deploy — this provisions the Worker +
#    the private.rustyphoton.space custom domain.
wrangler deploy
```

## Uploading files

From an authenticated `wrangler` session:

```bash
wrangler r2 object put \
  rusty-photon-private-mirror/svbony/windows-SVBCameraSDK-v1.13.4.zip \
  --file ./windows-SVBCameraSDK-v1.13.4.zip --remote
```

Record the file's sha256 wherever CI consumes it.

## Fetching (CI / manual)

```bash
curl -fsSL -H "Authorization: Bearer $PRIVATE_MIRROR_TOKEN" \
  https://private.rustyphoton.space/svbony/windows-SVBCameraSDK-v1.13.4.zip \
  -o sdk.zip
sha256sum sdk.zip   # verify against the pinned hash
```
