#!/bin/sh
# push-packages-repo.sh — replace the published package-repo tree in the
# R2 bucket behind pkg.rustyphoton.space with the freshly built-and-
# verified SITE (docs/plans/archive/nightly-releases.md, phase N5). Two bucket
# bookkeeping objects make the replacement safe without ever listing
# the bucket — a sweep driven only by keys this script recorded can
# never touch objects placed by other means: manifest.txt — every key
# assumed live, always a superset of reality — and manifest-prev.txt —
# the previous publish's exact tree, which is retained for one extra
# generation so metadata a client just read keeps resolving:
#
#   1. read manifest.txt (the live-key superset) and manifest-prev.txt
#      (the retained previous generation); only a genuine missing-object
#      error may stand in for either being absent,
#   2. pre-write manifest.txt as the UNION of the old listing and the
#      tree about to upload: every key this run might create is on
#      record before it exists, so an interruption at any later point
#      leaves nothing unlisted — a future run can always sweep it,
#   3. upload every content object (pool debs, rpms, indices and their
#      by-hash copies, repodata blobs, pubkey.asc) — purely additive:
#      the old metadata keeps serving the old, complete tree throughout,
#   4. upload the metadata entry points last, as the flip, detached
#      signature before the file it covers (Release.gpg → Release →
#      InRelease for apt, repomd.xml.asc → repomd.xml for dnf): a client
#      that reads the NEW signed file always finds its signature already
#      published. Detached-signature pairs replaced in place cannot be
#      made fully atomic in either order — a client that read the
#      OUTGOING signed file inside the sub-second gap between the two
#      puts pairs it with the new signature and must retry; the reverse
#      order would instead mismatch every client between the puts.
#      Modern apt is immune outright (InRelease is clearsigned,
#      one object, atomic); dnf and legacy-apt clients clear the
#      transient on their next retry,
#   5. delete stale objects: previous listing minus the new tree MINUS
#      the retained previous generation. Unique-name objects (pool
#      files, by-hash indices, repodata blobs) therefore live exactly
#      two publishes, so a client that read the outgoing InRelease /
#      repomd.xml still fetches every object it names for a full
#      day — with the apt by-hash indices (build-apt-repo.sh) that
#      closes the index replacement race completely; only the
#      canonical stable-named Packages(.gz) paths, used by
#      by-hash-unaware clients, keep a retry-sized window,
#   6. write manifest-prev.txt = the exact new tree (the next run's
#      grace generation),
#   7. rewrite manifest.txt = the new tree plus what step 5 retained.
#
# Interruption anywhere self-heals: manifest.txt only ever grows ahead
# of reality (step 2) and shrinks after deletes (step 7), so no key is
# ever live-but-unlisted; the worst case of a mid-run death is a
# shortened grace window for the just-outgoing generation, swept by the
# next run.
#
# Auth: R2's S3-compatible endpoint via the aws CLI, using the S3
# credentials of a bucket-scoped "Object Read & Write" R2 API token
# (AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY — the
# PACKAGES_R2_ACCESS_KEY_ID / PACKAGES_R2_SECRET_ACCESS_KEY secrets)
# plus CLOUDFLARE_ACCOUNT_ID to form the endpoint URL. The S3 API is
# the ONLY API such a token can authenticate against: the Cloudflare
# REST API (what `wrangler r2 object` drives) rejects object-level
# tokens outright and accepts only account-wide Admin R2 tokens —
# far too much blast radius for a nightly publisher.
#
# Usage: scripts/push-packages-repo.sh [--site DIR] [--bucket NAME]
#   --site DIR     the verified tree to publish (default: site)
#   --bucket NAME  R2 bucket name (default: rusty-photon-packages)

set -eu

# One collation order for every sort and comm below — a locale mismatch
# between them makes comm reject its input mid-run.
LC_ALL=C
export LC_ALL

die() { echo "push-packages-repo: $*" >&2; exit 1; }

usage() {
    sed -n '/^# Usage:/,/^$/{s/^# \{0,1\}//p}' "$0"
}

SITE="site"
BUCKET="rusty-photon-packages"
while [ $# -gt 0 ]; do
    case "$1" in
        --site)
            shift
            [ $# -gt 0 ] || die "--site needs a directory"
            SITE="$1"
            ;;
        --bucket)
            shift
            [ $# -gt 0 ] || die "--bucket needs a name"
            BUCKET="$1"
            ;;
        -h|--help) usage; exit 0 ;;
        *) usage >&2; die "unknown option: $1" ;;
    esac
    shift
done

command -v aws > /dev/null 2>&1 || die "aws CLI not found"
[ -n "${AWS_ACCESS_KEY_ID:-}" ] || die "AWS_ACCESS_KEY_ID is not set"
[ -n "${AWS_SECRET_ACCESS_KEY:-}" ] || die "AWS_SECRET_ACCESS_KEY is not set"
[ -n "${CLOUDFLARE_ACCOUNT_ID:-}" ] || die "CLOUDFLARE_ACCOUNT_ID is not set"
[ -f "$SITE/pubkey.asc" ] || die "$SITE/pubkey.asc missing — build + verify the tree first"
SITE_ABS=$(cd "$SITE" && pwd)

ENDPOINT="https://${CLOUDFLARE_ACCOUNT_ID}.r2.cloudflarestorage.com"
# R2 wants region "auto"; aws CLI refuses to run with no region at all.
AWS_DEFAULT_REGION="${AWS_DEFAULT_REGION:-auto}"
export AWS_DEFAULT_REGION
# aws CLI ≥ 2.23 adds CRC integrity headers by default; R2 rejects
# them. when_required restores plain SigV4 (older CLIs ignore these).
AWS_REQUEST_CHECKSUM_CALCULATION=when_required
AWS_RESPONSE_CHECKSUM_VALIDATION=when_required
export AWS_REQUEST_CHECKSUM_CALCULATION AWS_RESPONSE_CHECKSUM_VALIDATION

TMPD=$(mktemp -d)
trap 'rm -rf "$TMPD"' EXIT INT TERM

put() {
    # no-store: Cloudflare's default edge cache covers .gz, and a stale
    # cached Packages.gz against a freshly flipped InRelease is a client
    # hash-mismatch. Every read origin-pulls from R2 instead — free egress,
    # and this channel's traffic is a handful of rigs; revisit only if
    # that ever changes. aws prints an ETag blob on stdout — discarded
    # by the redirect; stderr passes through so a failure shows its
    # real cause in the CI log.
    aws s3api put-object --endpoint-url "$ENDPOINT" --bucket "$BUCKET" \
        --key "$1" --body "$2" --cache-control no-store > /dev/null \
        || die "upload failed: $1"
    echo "  put $1"
}

# fetch_listing KEY OUTFILE — 0 = fetched, 1 = object genuinely missing
# (NoSuchKey); any other failure (auth, network) dies after retries,
# because continuing with an empty listing would rewrite the on-bucket
# manifest without the previous keys and orphan them forever.
fetch_listing() {
    : > "$2"
    for attempt in 1 2 3; do
        if aws s3api get-object --endpoint-url "$ENDPOINT" \
            --bucket "$BUCKET" --key "$1" "$2" \
            > /dev/null 2> "$TMPD/get-err.txt"; then
            return 0
        fi
        if grep -q "NoSuchKey" "$TMPD/get-err.txt"; then
            return 1
        fi
        sleep "$attempt"
    done
    cat "$TMPD/get-err.txt" >&2
    die "reading $1 failed and not with a missing-object error (above) — refusing to continue"
}

is_flip() {
    case "$1" in
        deb/dists/*/InRelease | deb/dists/*/Release | deb/dists/*/Release.gpg) return 0 ;;
        rpm/*/repodata/repomd.xml | rpm/*/repodata/repomd.xml.asc) return 0 ;;
        *) return 1 ;;
    esac
}

# 1. The live-key superset and the retained previous generation.
if fetch_listing manifest.txt "$TMPD/old.txt"; then
    echo "Previous manifest: $(wc -l < "$TMPD/old.txt" | tr -d ' ') objects"
else
    echo "No previous manifest.txt on the bucket (first publish)"
fi
if fetch_listing manifest-prev.txt "$TMPD/prev.txt"; then
    echo "Retained generation: $(wc -l < "$TMPD/prev.txt" | tr -d ' ') objects"
else
    echo "No manifest-prev.txt on the bucket (nothing retained yet)"
fi
sort -u "$TMPD/old.txt" > "$TMPD/old.sorted"
sort -u "$TMPD/prev.txt" > "$TMPD/prev.sorted"

(cd "$SITE_ABS" && find . -type f ! -name manifest.txt ! -name manifest-prev.txt | sed 's|^\./||' | sort) > "$TMPD/new.txt"
[ -s "$TMPD/new.txt" ] || die "no files under $SITE"

# 2. Pre-write the union manifest, so every key this run might upload is
# already on record for future sweeps if we die partway.
sort -u "$TMPD/old.sorted" "$TMPD/new.txt" > "$TMPD/union.txt"
put manifest.txt "$TMPD/union.txt"

# 3. Content next — additive while the old metadata is still live.
echo "Uploading content objects..."
while IFS= read -r k; do
    if ! is_flip "$k"; then
        put "$k" "$SITE_ABS/$k"
    fi
done < "$TMPD/new.txt"

# 4. The flip, signature-before-signed within each pair/trio.
echo "Flipping metadata..."
for name in Release.gpg Release InRelease repomd.xml.asc repomd.xml; do
    while IFS= read -r k; do
        if [ "$(basename "$k")" = "$name" ] && is_flip "$k"; then
            put "$k" "$SITE_ABS/$k"
        fi
    done < "$TMPD/new.txt"
done

# 5. Sweep everything except the new tree and the retained generation.
comm -23 "$TMPD/old.sorted" "$TMPD/new.txt" > "$TMPD/not-new.txt"
comm -23 "$TMPD/not-new.txt" "$TMPD/prev.sorted" > "$TMPD/stale.txt"
if [ -s "$TMPD/stale.txt" ]; then
    echo "Deleting $(wc -l < "$TMPD/stale.txt" | tr -d ' ') stale objects..."
    while IFS= read -r k; do
        [ -n "$k" ] || continue
        if aws s3api delete-object --endpoint-url "$ENDPOINT" \
            --bucket "$BUCKET" --key "$k" > /dev/null 2>&1; then
            echo "  deleted $k"
        else
            echo "push-packages-repo: warning: could not delete stale $k (already gone?)" >&2
        fi
    done < "$TMPD/stale.txt"
else
    echo "No stale objects to delete"
fi

# 6. The new tree becomes the next run's retained generation.
put manifest-prev.txt "$TMPD/new.txt"

# 7. The live listing: the new tree plus what step 5 retained.
comm -12 "$TMPD/old.sorted" "$TMPD/prev.sorted" > "$TMPD/retained.txt"
sort -u "$TMPD/new.txt" "$TMPD/retained.txt" > "$TMPD/final.txt"
put manifest.txt "$TMPD/final.txt"

echo ""
echo "push-packages-repo: OK ($(wc -l < "$TMPD/new.txt" | tr -d ' ') objects published, $(wc -l < "$TMPD/final.txt" | tr -d ' ') live in $BUCKET)"
