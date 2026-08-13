#!/bin/bash
# Ephemeral GitHub Actions runner pool for a Proxmox VE host.
#
# Maintains one warm runner clone per POOL SLOT:
#   linked-clone the template -> boot -> mint a single-use JIT runner config
#   via the GitHub API -> inject it through the QEMU guest agent -> the
#   in-guest one-job runner runs exactly one job and powers off -> the
#   clone is destroyed and its runner registration deleted -> repeat.
#
# Each slot runs that loop independently and concurrently, so the pool can
# serve several queued jobs at once. Slots are declared in SLOTS below; two
# slots sharing a label set are interchangeable, which is how the Linux pool
# serves bazel.yml and bazel-coverage.yml (both fire on the same PR event)
# without one queueing behind the other.
#
# While a slot waits for its clone to finish, it also watches that clone's
# runner through the GitHub API, so a guest that wedges is reclaimed rather
# than holding the slot forever. See the wait loop in slot_loop for why the
# check is on the runner's liveness and never on elapsed time.
#
# Deployment (on the Proxmox host, as root — see
# docs/skills/proxmox-runner-pool.md):
#   install -m 755 rp-runner-pool.sh /usr/local/sbin/
#   put a fine-grained PAT in /etc/rp-runner/github-token (chmod 600); resource
#   owner: the rusty-photon org, sole permission "Self-hosted runners: Read
#   and write" (organization permission) — runner registration and nothing
#   else, which is why runners register at org level rather than repo level
#   (repo-level registration would require the far broader Administration
#   permission)
#   run under a systemd unit with Restart=always
#
# Security properties this loop preserves:
#   * the PAT lives only on the hypervisor, never inside any VM;
#   * each VM receives exactly one single-use JIT config: a compromised job
#     cannot mint further registrations;
#   * every job runs on a fresh linked clone; nothing persists between jobs
#     except the shared remote build cache, whose writes are separately
#     credential-gated.
# pipefail so the injection check below really does gate on BOTH `qm guest
# exec` and the in-guest exitcode it reports: the exit status of a pipeline is
# otherwise its last command's, so a `qm` failure that still emitted
# `exitcode: 0` JSON would read as a successful injection and wedge the slot.
# The script's other pipelines feed emptiness checks or string comparisons, so
# this changes nothing for them.
set -u -o pipefail

ORG=rusty-photon
TOKEN_FILE=/etc/rp-runner/github-token

# Per-clone "this one received its config" markers, used to tell an in-flight
# job from an orphan when this service restarts. tmpfs on purpose: a host
# reboot clears it, and a host reboot also takes the clones with it.
STATE_DIR=/run/rp-runner-pool
mkdir -p "$STATE_DIR"

# Slot health check (see the wait loop at the end of slot_loop). That loop
# polls every 10 seconds; probing the runner's GitHub-side state is an API
# call rather than a local one, so it runs on every Nth poll — once a minute.
#
# A slot reclaims its clone once HEALTH_STRIKES probes have come back `gone`
# with no `online` between them. That is deliberately NOT "consecutive polls":
# a probe that could not reach the API returns `unknown`, which neither
# increments nor resets the count, so an API outage stretches the grace rather
# than counting toward it. The values below therefore set a floor of ten
# minutes, not a fixed duration — which is why the reclaim log reports the
# time actually elapsed. That number is what separates a clean wedge from ten
# strikes dribbled out across an hour of flaky API.
HEALTH_PROBE_EVERY=6
HEALTH_STRIKES=10

# Pool slots: name|template VMID|clone VMID|guest OS|labels
#
# Clone VMIDs must be unique and must not collide with any other VM on the
# host. Guest OS selects the jitconfig injection path (the guests differ in
# shell and runner directory, nothing else). Sizing note: every slot keeps one
# clone powered on at all times, so the host must hold the sum of their
# memory — see the capacity section of docs/plans/proxmox-pr-routing.md.
#
# The name is cosmetic and safe to change: it names the clone VM, prefixes
# this slot's log lines, and forms the GitHub runner name ("<name>-<epoch>").
# Nothing keys on it — the reconcile below matches clones by VMID and marker
# file — so a rename takes effect as each slot next recreates its clone,
# leaving no stale state behind. What must NOT change casually are the labels,
# which workflows select on.
# Templates live on cipool (the 4 TB NVMe), not the root mirror: clone disks
# are the write-heavy, disposable part of the workload and the mirror collapses
# under concurrency (see docs/skills/proxmox-runner-pool.md, storage layout).
# 920 = Linux, 910 = Windows, both 16 GB / 6 vCPU — resized 2026-08 after the
# oversubscription flake wave (5 slots × 12 vCPU on a 20-thread host bred
# timing flakes across nine suites; 5 × 6 keeps worst-case load ~1.5×).
# 920/910 replace 919/909: byte-identical builds with RP_LAN_CACHE_URL
# repointed after the runner VLAN's renumbering to a /16 (the cache endpoint
# moved with it; the address itself is deliberately not recorded in this
# public repo). 920 inherits the one-time `bazel coverage //...` warmup
# introduced in 918, so its Bazel output base already holds the nightly
# toolchain + instrumented externals — that keeps the pooled `bazel coverage`
# leg zero-WAN instead of re-fetching the nightly toolchain on every ephemeral
# clone. (Lineage: 919/909 repointed RP_LAN_CACHE_URL at the NAS-hosted cache
# on the 25GbE fabric; 918 added the coverage warmup; 917 replaced the first
# cipool Linux template 907, which shipped a populated /etc/machine-id and so
# handed every clone the same DHCP identity and IP; all are built with
# machine-id wiped.)
# Both templates carry firewall=1 on their NIC, so clones inherit it and the
# per-clone policy written in slot_loop takes effect (clone-to-clone isolation).
#
# Two Windows slots share template 910: GitHub dispatches a Windows pool job to
# whichever clone is free, so a second slot removes the cross-branch queue that
# a single Windows runner created. The shared-autologon-credential concern this
# raised (both clones hold the same local admin password) is mitigated by the
# NIC isolation below — a compromised clone cannot reach a peer's SMB/RDP/WinRM.
SLOTS=(
  "runner-linux1|920|9100|linux|[\"self-hosted\",\"Linux\",\"X64\",\"proxmox-ephemeral\"]"
  "runner-linux2|920|9101|linux|[\"self-hosted\",\"Linux\",\"X64\",\"proxmox-ephemeral\"]"
  "runner-linux3|920|9102|linux|[\"self-hosted\",\"Linux\",\"X64\",\"proxmox-ephemeral\"]"
  "runner-win|910|9200|windows|[\"self-hosted\",\"Windows\",\"X64\",\"proxmox-ephemeral-windows\"]"
  "runner-win2|910|9201|windows|[\"self-hosted\",\"Windows\",\"X64\",\"proxmox-ephemeral-windows\"]"
)

# Free-plan orgs have exactly one (default) runner group, but resolve its id
# rather than assuming 1 so a plan change can't silently break registration.
GROUP_ID=$(curl -fsS --connect-timeout 5 --max-time 30 \
  -H @<(printf 'Authorization: Bearer %s' "$(cat $TOKEN_FILE)") \
  -H "Accept: application/vnd.github+json" \
  "https://api.github.com/orgs/$ORG/actions/runner-groups" \
  | python3 -c 'import json,sys; gs=json.load(sys.stdin)["runner_groups"]; print(next(g["id"] for g in gs if g["default"]))')
if [ -z "${GROUP_ID:-}" ]; then
  echo "cannot resolve the default runner group — token invalid or lacking the org Self-hosted runners permission" >&2
  exit 1
fi

log() { echo "$(date -Is) [$1] ${*:2}"; }

# Report whether the GitHub-side runner a clone was registered as is currently
# connected: "online", "gone", or "unknown".
#
# This is the signal that tells an IDLE clone from a WEDGED one, which no
# timer can do — see the wait loop. `online` covers both idle-and-waiting and
# busy-running-a-job; either way the clone is doing its job and must not be
# touched.
#
# "unknown" means the API could not be asked (transport error, 5xx, rate
# limit) and is deliberately NOT a verdict — the same discipline the wait loop
# applies to an unreadable `qm status`, and the reason this uses -sS rather
# than -f: a 404 must be readable as a result, not collapsed into a transport
# failure.
#
# "gone" folds together three states that cannot be told apart from outside
# and need not be, because they mean the same thing to a slot: never
# connected, died, or finished. A JIT runner exists from the moment its config
# is minted and reads `offline` until it connects; when its job ends GitHub
# deletes some registrations outright (404) and leaves others sitting
# `offline` — in this pool the Windows clones leak entries that way and the
# Linux ones do not. That asymmetry is harmless here: a slot only ever asks
# about the id it minted for the clone it is watching, and each of those
# states means that clone is not working a job.
runner_state() {
  local id=$1 body code
  body=$(mktemp) || { echo unknown; return; }
  # Bounded, because this runs inline in the slot's wait loop: an unbounded
  # curl against a black-holed api.github.com would stop the slot observing
  # `qm status` at all, including its clone's clean poweroff. A timeout prints
  # http_code 000 and so degrades to "unknown", which is exactly right.
  code=$(curl -sS --connect-timeout 5 --max-time 15 -o "$body" -w '%{http_code}' \
    -H @<(printf 'Authorization: Bearer %s' "$(cat "$TOKEN_FILE")") \
    -H "Accept: application/vnd.github+json" \
    "https://api.github.com/orgs/$ORG/actions/runners/$id" 2>/dev/null)
  case "$code" in
    # Both verdicts require positive evidence; anything else is "unknown".
    # Mapping "not explicitly online" to "gone" would make a schema change or
    # a truncated 200 body read as a dead runner and reclaim a healthy clone
    # ten minutes later, which is the one outcome this function must never
    # produce by accident.
    200) python3 -c 'import json,sys; s=json.load(sys.stdin).get("status"); print("online" if s == "online" else "gone" if s == "offline" else "unknown")' \
      <"$body" 2>/dev/null || echo unknown ;;
    404) echo gone ;;
    *) echo unknown ;;
  esac
  rm -f "$body"
}

# Delete a JIT runner's org registration once its clone is torn down. A
# single-use JIT runner is meant to deregister itself when its job ends, and
# the Linux guest's `systemctl poweroff` gives it the SIGTERM to do so — but
# the Windows guest ends with `Stop-Computer -Force`, which cuts it off first,
# and a clone reclaimed as wedged cannot deregister at all. Either way the
# entry lingers `offline` in the org's single runner list forever, cluttering
# the one list an operator reads to ask "is the pool alive?".
#
# Doing it host-side from the id in the injection marker is authoritative and
# stateless: it does not depend on the guest managing a clean exit and covers a
# wedge reclaim exactly as well as a clean finish. Bounded and best-effort like
# every other call here — a lingering registration is inert and teardown must
# proceed regardless — but it returns the HTTP code so the caller can tell a
# real failure, worth a log line since a systematic one would let the leak
# creep back unnoticed, from the benign 404 of a runner that already
# deregistered itself, which is the Linux happy path every time. -sS not -f so
# that 404 stays readable as a code rather than collapsing into a transport
# error, the same discipline runner_state uses.
deregister_runner() {
  local id=$1
  curl -sS --connect-timeout 5 --max-time 15 -X DELETE -o /dev/null -w '%{http_code}' \
    -H @<(printf 'Authorization: Bearer %s' "$(cat "$TOKEN_FILE")") \
    -H "Accept: application/vnd.github+json" \
    "https://api.github.com/orgs/$ORG/actions/runners/$id" 2>/dev/null
}

# Write the JIT config into the guest. Both variants write a temp file and
# rename it, because the in-guest runner polls for a NON-EMPTY .jitconfig and
# must never read a partial write. A JIT config is base64, so single-quoting
# it in the PowerShell variant is safe.
inject_jitconfig() {
  local vmid=$1 os=$2 jit=$3
  case "$os" in
    linux)
      qm guest exec "$vmid" -- /bin/bash -c "printf %s \"$jit\" > /home/ci/actions-runner/.jitconfig.tmp && chown ci:ci /home/ci/actions-runner/.jitconfig.tmp && mv /home/ci/actions-runner/.jitconfig.tmp /home/ci/actions-runner/.jitconfig"
      ;;
    windows)
      # PowerShell exits 0 even when a cmdlet raises a non-terminating error,
      # so the caller's exitcode check alone would accept a failed write and
      # then deadlock waiting for a job that can never start. Force errors to
      # terminate, confirm the landed file is non-empty, and exit explicitly.
      qm guest exec "$vmid" -- powershell.exe -NoProfile -NonInteractive -Command "\$ErrorActionPreference='Stop'; try { Set-Content -Path 'C:\\actions-runner\\.jitconfig.tmp' -Value '$jit' -NoNewline -Encoding ascii; Move-Item -Force 'C:\\actions-runner\\.jitconfig.tmp' 'C:\\actions-runner\\.jitconfig'; if ((Get-Item 'C:\\actions-runner\\.jitconfig').Length -le 0) { exit 1 }; exit 0 } catch { exit 1 }"
      ;;
    *)
      echo "unknown guest os '$os'" >&2
      return 1
      ;;
  esac
}

# Per-clone network isolation. Every pool clone talks only to GitHub and the
# LAN cache (both off-subnet, reached through the gateway) and never to a peer,
# so the firewall drops all inbound and permits all outbound. `dhcp: 1` keeps
# the clone's own lease working; established/related return traffic (the cache,
# GitHub) is allowed automatically. The template NIC carries firewall=1 so the
# clone inherits it — this file supplies the policy. It matters most for the
# two Windows slots, which share one local-admin password: without it a
# compromised clone could reach a peer's SMB/RDP/WinRM (ICMP echo is still
# permitted by the Proxmox default and carries no such risk).
FW_DIR=/etc/pve/firewall
write_clone_firewall() {
  local f="$FW_DIR/$1.fw" rc
  cat > "$f" <<'FW'
[OPTIONS]
enable: 1
policy_in: DROP
policy_out: ACCEPT
dhcp: 1
FW
  rc=$?
  # The clone is only isolated if BOTH the firewall is enabled and the inbound
  # policy is DROP — `policy_in: DROP` under a missing `enable: 1` is inert. The
  # script does not run under `set -e`, so verify the write here: cat returns 0
  # only when every byte was written (a truncated/ENOSPC write returns
  # non-zero), and both key directives must be present, so a partial write or a
  # stale file can never pass as isolated. The caller treats failure as fatal.
  [ "$rc" -eq 0 ] \
    && grep -q '^enable: 1$' "$f" \
    && grep -q '^policy_in: DROP$' "$f"
}

# Tear a clone down: stop it, drop its marker, deregister its runner, destroy
# the VM. Takes the runner id explicitly when the caller has just minted it but
# the marker was not written yet (a mint that succeeded then failed to inject);
# otherwise the id comes from the injection marker, which is where every other
# teardown path — clean finish, wedge reclaim — carries it. An orphan the
# reconcile destroys has neither, because it never received a config and so no
# runner was ever registered for it.
destroy_clone() {
  local vmid=$1 rid=${2:-} code
  qm stop "$vmid" >/dev/null 2>&1
  [ -z "$rid" ] && rid=$(cat "$STATE_DIR/$vmid.injected" 2>/dev/null)
  rm -f "$STATE_DIR/$vmid.injected"
  # Deregister before destroying the VM so the org runner list does not
  # accumulate one offline entry per Windows job (see deregister_runner). An
  # empty id means nothing was ever minted for this clone.
  if [ -n "$rid" ]; then
    case "$rid" in
      *[!0-9]*)
        # The id comes from a marker file; a non-numeric value means that file
        # is corrupt. Surface it rather than build a malformed URL from it.
        log "$vmid" "injection marker for $vmid holds a non-numeric runner id ('$rid'); skipping deregistration" ;;
      *)
        # 204 (deleted) and 404 (already gone — the Linux happy path) are
        # success. 000 is curl's "no HTTP status": a transport failure, so it
        # is reported as unreachable rather than as a status code. Any other
        # code is a real HTTP problem. Either failure is worth a line so a
        # systematic one shows up rather than the leak quietly returning.
        code=$(deregister_runner "$rid")
        case "$code" in
          204 | 404) : ;;
          000) log "$vmid" "runner $rid deregistration could not reach the API" ;;
          *) log "$vmid" "runner $rid deregistration returned HTTP $code" ;;
        esac ;;
    esac
  fi
  # Drop the isolation policy only when the destroy actually removed the VM.
  # Keying cleanup off `qm destroy` succeeding — not a `qm status` probe, which
  # can fail transiently while the clone still exists — keeps a still-present
  # clone's inbound DROP in place; the caller retries the destroy. A recreated
  # VMID rewrites its .fw before boot, so a briefly-orphaned file is harmless.
  qm destroy "$vmid" --purge >/dev/null 2>&1 && rm -f "$FW_DIR/$vmid.fw"
}

slot_loop() {
  local name=$1 template=$2 vmid=$3 os=$4 labels=$5

  while true; do
    # Establish the invariant the rest of the iteration depends on: either the
    # VM does not exist, or it exists AND holds a real job. A clone with no
    # injection marker was created but never configured, so the health check
    # below cannot watch it — it has no runner id to watch — and the guest's
    # own no-config timeout is then the only thing that would end it, half an
    # hour of a slot held for nothing. Two ways to reach that state: this
    # service restarting mid-window, and a destroy that did not take (a
    # Proxmox lock, say), which is why the check runs every iteration rather
    # than once at startup. A clone WITH a marker is left alone: an
    # orchestrator restart must never abort an in-flight job.
    if qm status "$vmid" >/dev/null 2>&1 && [ ! -e "$STATE_DIR/$vmid.injected" ]; then
      log "$name" "clone $vmid exists but never received a config; destroying"
      destroy_clone "$vmid"
      # If the destroy did not take, do not fall through into the poweroff
      # wait — retry the reconcile instead, so a transient lock resolves.
      if qm status "$vmid" >/dev/null 2>&1; then
        log "$name" "clone $vmid still present after destroy; retrying"
        sleep 30
        continue
      fi
    fi

    if ! qm status "$vmid" >/dev/null 2>&1; then
      qm clone "$template" "$vmid" --name "$name" >/dev/null || { sleep 30; continue; }
      # Write the isolation policy before the clone boots, so the first packet
      # it sends is already filtered — the clone inherits firewall=1 from the
      # template NIC and this supplies the rules. If the policy fails to land,
      # booting anyway would leave the clone unisolated (peer SMB/RDP/WinRM
      # reachable), so treat it as fatal: destroy and retry rather than start.
      if ! write_clone_firewall "$vmid"; then
        log "$name" "firewall policy for $vmid did not land; destroying"
        destroy_clone "$vmid"
        sleep 30
        continue
      fi
      # A marker can outlive the clone it described: killing this service
      # between the destroy and its `rm` leaves one behind, and the next clone
      # of the same VMID would then look already-configured to the reconcile
      # above and never be recovered. Clearing here binds the marker to THIS
      # clone instance.
      rm -f "$STATE_DIR/$vmid.injected"
      qm start "$vmid" >/dev/null

      # Windows clones take appreciably longer than Linux to reach a
      # responding guest agent, so the wait is generous rather than tuned.
      booted=0
      for _ in $(seq 1 60); do
        qm agent "$vmid" ping >/dev/null 2>&1 && { booted=1; break; }
        sleep 5
      done
      if [ $booted != 1 ]; then
        log "$name" "clone $vmid never reached the guest agent; destroying"
        destroy_clone "$vmid"
        sleep 30
        continue
      fi

      # The auth header arrives via process substitution (bash printf is a
      # builtin), so the PAT never appears on any process command line.
      #
      # Bounded like the other calls, with one tradeoff worth naming: this is
      # the only POST, so a timeout that fires after GitHub created the runner
      # but before the response arrived leaks a registration. A stalled slot
      # is the worse failure — it serves no jobs at all — and a leaked
      # registration is inert.
      mint=$(curl -fsS --connect-timeout 5 --max-time 30 -X POST \
        -H @<(printf 'Authorization: Bearer %s' "$(cat $TOKEN_FILE)") \
        -H "Accept: application/vnd.github+json" \
        "https://api.github.com/orgs/$ORG/actions/runners/generate-jitconfig" \
        -d "{\"name\":\"$name-$(date +%s)\",\"runner_group_id\":$GROUP_ID,\"labels\":$labels,\"work_folder\":\"_work\"}")
      JIT=$(printf '%s' "$mint" | python3 -c 'import json,sys; print(json.load(sys.stdin)["encoded_jit_config"])' 2>/dev/null)
      # The mint response also names the runner it created. That id is how the
      # health check identifies THIS clone's runner among the pool's, so it is
      # as essential as the config itself: without it the slot has no way to
      # tell an idle clone from a wedged one.
      RUNNER_ID=$(printf '%s' "$mint" | python3 -c 'import json,sys; print(json.load(sys.stdin)["runner"]["id"])' 2>/dev/null)
      if [ -z "${JIT:-}" ] || [ -z "${RUNNER_ID:-}" ]; then
        log "$name" "jitconfig mint failed; destroying $vmid"
        destroy_clone "$vmid"
        sleep 60
        continue
      fi

      # An unverified injection would deadlock this loop — the guest waits for
      # a config that never arrives while this loop waits for a poweroff that
      # never comes — so check both qm's own exit and the in-guest exitcode.
      if ! inject_jitconfig "$vmid" "$os" "$JIT" \
          | python3 -c 'import json,sys; sys.exit(0 if json.load(sys.stdin).get("exitcode") == 0 else 1)'; then
        log "$name" "jitconfig injection into $vmid failed; destroying"
        # RUNNER_ID is minted but the marker is not written until injection
        # succeeds, so pass it explicitly — otherwise this teardown would leak
        # the registration, with no marker left to recover its id.
        destroy_clone "$vmid" "$RUNNER_ID"
        sleep 30
        continue
      fi
      # Only now is the clone recoverable across a restart of this service.
      #
      # Ordering is deliberate, and this is the safe direction. Dying in the
      # sliver between a successful injection and this line loses a clone that
      # was about to run a job — one aborted job, and the pool immediately
      # rebuilds the slot. Writing the marker FIRST would instead lose a clone
      # that never got a config, which nothing recovers: the guest waits
      # forever for a config that will not come and this loop waits forever
      # for its poweroff. A transient, self-healing failure beats a permanent
      # one. Note also that the guest deletes .jitconfig the moment it reads
      # it (~2 s), so the file's presence cannot be used to detect a running
      # job — proving liveness needs the runner's state from the GitHub API.
      # That is also why the marker carries the runner id rather than being
      # empty: it is what lets the health check below survive a restart of
      # this service and keep watching a clone it did not itself register.
      printf '%s\n' "$RUNNER_ID" > "$STATE_DIR/$vmid.injected"
      log "$name" "runner clone $vmid up and registered"
    fi

    # The GitHub runner id of whatever clone occupies this slot — the one just
    # injected above, or one inherited from before a restart of this service.
    # An empty marker (one written by an older version of this script) simply
    # means no health check for that clone; the wait then behaves as it did
    # before, which is the right way to degrade.
    runner_id=$(cat "$STATE_DIR/$vmid.injected" 2>/dev/null)

    # The clone powers itself off after its single job.
    #
    # This wait is NOT bounded by wall clock. A registered clone with no job
    # yet assigned is not stalled, it is WARM — sitting here ready to be
    # picked is the whole point of the pool — so a time limit would
    # periodically destroy healthy idle runners and put cold-start latency
    # back on every job. What it is bounded by is the runner's own liveness:
    # a guest that wedges (hung shutdown, BSOD, a runner process that dies
    # without ever powering off) would otherwise hold its slot forever, and
    # with a single Windows slot that is the whole venue lost, silently.
    #
    # `runner_state` is what separates those two cases; a timer cannot. An
    # idle clone is `online`, a busy one is `online`, and only a clone that is
    # no longer working its job reads otherwise. The strike count exists
    # because "not online" is briefly TRUE on the happy path too: the runner
    # stops reading `online` the instant its job ends, a few seconds before
    # the guest finishes powering off. Normal shutdowns lose that race by
    # minutes, so
    # the grace period never fires on them — but a shutdown that hangs past it
    # is exactly the wedge this is here to catch, and reclaiming it then is
    # correct.
    #
    # Deliberately not part of the verdict: `qm agent ping`. It cannot tell a
    # healthy idle guest from one whose runner died under a live kernel, and
    # OR-ing it in would reclaim busy clones whose agent merely timed out
    # under build load. The GitHub state already covers every wedge it would.
    #
    # Only a confirmed `stopped` ends the wait normally. An unreadable status
    # is NOT "stopped": stderr is suppressed here, so a transient failure
    # (host under load, a lock) yields an empty string, and treating that as
    # stopped would destroy a VM still running a job. Unknown therefore means
    # keep waiting — but not forever, since a VM removed out of band would
    # never report anything again; after a few minutes of silence give up on
    # the wait and let the next iteration's reconcile decide.
    unknown=0
    abandoned=0
    offline=0
    tick=0
    reason=finished
    while true; do
      state=$(qm status "$vmid" 2>/dev/null | awk '{print $2}')
      [ "$state" = stopped ] && break
      if [ -z "$state" ]; then
        unknown=$((unknown + 1))
        if [ "$unknown" -ge 30 ]; then
          log "$name" "status of $vmid unreadable for 5 minutes; re-reconciling"
          abandoned=1
          break
        fi
      else
        unknown=0
      fi

      # Probe only while the VM is confirmed running: an unreadable status
      # already has its own handling above, and a clone that is mid-shutdown
      # should be judged by `stopped` arriving, not by its runner going away.
      # `running` rather than "anything but stopped" is deliberate — it leaves
      # a `paused`/`suspended` clone alone, on the grounds that a VM in that
      # state was put there by an operator who is probably looking at it.
      # Skipping tick 0 is what keeps the ten-minute floor honest: N probes
      # starting at t=0 span only N-1 intervals, so the earliest possible
      # reclaim would land a minute short of it. Starting at t=60s also drops
      # a probe that carried no information — a clone whose config landed
      # seconds ago cannot have connected yet, so that reading is `gone` on
      # the happy path every time.
      if [ -n "$runner_id" ] && [ "$state" = running ] && [ "$tick" -gt 0 ] &&
        [ $((tick % HEALTH_PROBE_EVERY)) -eq 0 ]; then
        case "$(runner_state "$runner_id")" in
          online) offline=0 ;;
          gone) offline=$((offline + 1)) ;;
          unknown) : ;; # could not ask — hold the count, do not judge
        esac
        if [ "$offline" -ge "$HEALTH_STRIKES" ]; then
          # Both numbers, because they differ whenever the API was flaky: the
          # strike count is fixed, the elapsed time is not. A reclaim at the
          # ten-minute floor is a clean wedge; one at forty minutes means the
          # probes spent most of that time unable to reach GitHub, which is a
          # different problem wearing the same log line.
          reason="wedged (runner $runner_id not connected on $HEALTH_STRIKES probes over $((tick * 10 / 60))m while the VM stayed up)"
          break
        fi
      fi

      sleep 10
      tick=$((tick + 1))
    done
    # Abandoning the wait means "I no longer know what this VM is doing", which
    # is not the same as "it finished" — destroying here would kill a job that
    # is merely unobservable while qm is failing. Go back to the reconcile,
    # which acts on a readable status or keeps retrying until there is one.
    if [ "$abandoned" = 1 ]; then
      sleep 30
      continue
    fi
    # Logged with its reason either way, so a recurring wedge shows up in the
    # journal as a pattern rather than as capacity quietly going missing.
    log "$name" "runner clone $vmid $reason; destroying"
    destroy_clone "$vmid"
  done
}

# One background loop per slot. Killing the service kills the loops but not
# the clones; each slot reconciles its own leftover on the next start —
# waiting on one that was already running a job, destroying one that never got
# a config (see slot_loop).
for slot in "${SLOTS[@]}"; do
  IFS='|' read -r s_name s_template s_vmid s_os s_labels <<<"$slot"
  log "$s_name" "starting slot (template $s_template, clone $s_vmid, $s_os)"
  slot_loop "$s_name" "$s_template" "$s_vmid" "$s_os" "$s_labels" &
done
wait
