# shellcheck shell=bash
# Fill REMOTE_FLAGS with the remote-cache routing for this job's venue and
# credentials. SOURCED, not executed, by every Bazel step of bazel.yml and
# bazel-coverage.yml:
#
#   source tools/ci/bazel-remote-flags.sh
#   bazel build --config=ci "${REMOTE_FLAGS[@]}" //...
#
# Sourcing keeps the array, and the write credential it may carry, in the
# step's own shell: nothing is written to the runner's disk, which on the pool
# is readable by whatever job runs there next. One copy rather than one per
# step, because a step that drifted from the others would quietly read from,
# or upload to, a different cache.
#
# Reads the job's environment: RP_LAN_CACHE_GRPC_URL (derived by the mask
# step when the LAN Remote Asset API is on), RP_LAN_CACHE_URL (the pool
# runner's .env), LAN_CACHE_WRITE_AUTH and CACHE_WRITE_TOKEN (each non-empty
# only for trusted events; see the workflows' env blocks).

# shellcheck disable=SC2034  # read by the step that sources this file
REMOTE_FLAGS=(--config=remote-cache)
if [[ -n "${RP_LAN_CACHE_GRPC_URL:-}" ]]; then
  # Pool venue with the remote downloader on (RP_LAN_ASSET_API; endpoint
  # derived by the mask step): the LAN bazel-remote serves the cache AND the
  # Remote Asset API on one gRPC endpoint. The cache channel must be gRPC too:
  # Bazel refuses a remote downloader next to an http(s) --remote_cache.
  # Bazel routes EVERY repository download through the asset Fetch RPC, and
  # bazel-remote gates that RPC behind write auth even for a pure cache hit —
  # so only jobs holding the write credential (push-to-main) actually fetch
  # via the LAN and populate it; that includes the pinned nightly coverage
  # toolchain, the most expensive cold fetch a clone can hit after a pin bump.
  # An anonymous (PR) job gets UNAUTHENTICATED on every downloader fetch, and
  # the local fallback turns that into the direct WAN download it would have
  # performed anyway — one failed LAN round-trip and a log warning, no
  # behavior change. The flags stay on anonymous jobs even so: both venues
  # run one code path, and a bazel-remote that learns to serve anonymous
  # checksummed hits from CAS would make PR fetches LAN hits with no workflow
  # change.
  REMOTE_FLAGS+=(--remote_cache="$RP_LAN_CACHE_GRPC_URL")
  REMOTE_FLAGS+=(--experimental_remote_downloader="$RP_LAN_CACHE_GRPC_URL")
  REMOTE_FLAGS+=(--experimental_remote_downloader_local_fallback)
  if [[ -n "${LAN_CACHE_WRITE_AUTH:-}" ]]; then
    REMOTE_FLAGS+=(--remote_header="Authorization=$LAN_CACHE_WRITE_AUTH")
    REMOTE_FLAGS+=(--remote_downloader_header="Authorization=$LAN_CACHE_WRITE_AUTH")
  else
    REMOTE_FLAGS+=(--remote_upload_local_results=false)
  fi
elif [[ -n "${RP_LAN_CACHE_URL:-}" ]]; then
  # Pool venue: --remote_cache after --config=remote-cache overrides the
  # cloud URL (last single-valued flag wins) while keeping the config's
  # timeout/keep_going settings.
  REMOTE_FLAGS+=(--remote_cache="$RP_LAN_CACHE_URL")
  if [[ -n "${LAN_CACHE_WRITE_AUTH:-}" ]]; then
    REMOTE_FLAGS+=(--remote_header="Authorization=$LAN_CACHE_WRITE_AUTH")
  else
    REMOTE_FLAGS+=(--remote_upload_local_results=false)
  fi
elif [[ -n "${CACHE_WRITE_TOKEN:-}" ]]; then
  REMOTE_FLAGS+=(--remote_header="Authorization=Bearer $CACHE_WRITE_TOKEN")
else
  REMOTE_FLAGS+=(--remote_upload_local_results=false)
fi
