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
#   run under the systemd unit shipped next to this script
#   (rp-runner-pool.service — install -m 644 into /etc/systemd/system/); the
#   unit is checked in rather than described so its ordering cannot drift
#   from what a deployment actually runs, and its own comments explain each
#   directive. The zfs.target ordering is
#   load-bearing: on startup the reconcile destroys stale clones, and a
#   `qm destroy` that runs before the ZFS pool backing the templates is
#   imported — early in boot the on-demand import the destroy triggers fails
#   too, because the device links are not up yet — removes the VM config but
#   leaves the volumes behind. A leftover
#   cloudinit volume wedges its slot on "dataset already exists" (cloudinit
#   volumes are allocated under a fixed name, so every retry collides);
#   leftover disk volumes collide with nothing — clones take the next free
#   index — and instead leak silently, pinning the template's base snapshot.
#   destroy_clone therefore refuses to destroy while a storage backing a
#   VM's volumes is inactive (see storage_gate), so on the teardown path a
#   misordered start costs a deferral instead of leaked volumes; the create
#   path is not gated — a failed clone or start is loud, logged, and
#   retried, though what qm's own rollback leaves behind on a half-imported
#   pool is qm's to get right. Apply the ordering and the cachefile
#   registration anyway: they close the window instead of waiting it out,
#   and they protect qm invocations outside this script.
#   zfs.target waits only for pools in the import cachefile, and a pool PVE
#   imported on demand has cachefile=none and is not in it — `zpool get
#   cachefile <pool>` reading `none` means at risk; register it with
#   `zpool set cachefile=/etc/zfs/zpool.cache <pool>` and confirm with
#   `zdb -C <pool>` (after the set, `zpool get cachefile` reads back `-`,
#   the default, not the path). A manual export re-runs PVE's on-demand
#   import next time the storage is touched and drops the pool from the
#   cachefile again — re-register after one.
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
# reboot clears it, and a host reboot also kills the running clones — their
# configs and volumes persist, which is what the startup reconcile removes.
STATE_DIR=/run/rp-runner-pool
mkdir -p "$STATE_DIR"

# Root of the PVE config filesystem. Overridable for one reason: the checks
# that license sweep_orphan_volumes to delete storage are read out of here, and
# they are the paths a test has to be able to lie about. Getting either wrong
# frees a live clone's disks, so they must be exercised rather than reasoned
# about. Production passes nothing and gets /etc/pve.
PVE_CONF_ROOT=${RP_PVE_CONF_ROOT:-/etc/pve}

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

# Leak recovery (see the sweep at the end of destroy_clone). `pvesm free` does
# not report whether the volume actually went, so the sweep confirms removal
# by re-listing and retries a bounded number of times. What it recovers from
# is storage-lock contention, which is transient: once the competing task
# releases the lock a later attempt takes. Kept small on purpose — this runs
# inline in the slot's teardown, and a volume still present after three spaced
# attempts is no longer waiting on a lock, it is a case for the runbook.
FREE_ATTEMPTS=3
FREE_RETRY_SLEEP=5

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
# 926 = Linux, 911 = Windows, both 16 GB / 6 vCPU — resized 2026-08 after the
# oversubscription flake wave (5 slots × 12 vCPU on a 20-thread host bred
# timing flakes across nine suites; 5 × 6 keeps worst-case load ~1.5×). The
# guest-wide freezes behind most of that wave turned out to be storage-side
# sync-write queueing, which relax_clone_sync below removes at the source.
# Current templates: 926 (Linux), 911 (Windows).
#
# 911 was cloned from the previous Windows template 910 (full clone) with the
# current tools/ci/runner-guest/one-job.ps1 copied in — the version that empties
# the job account's %TEMP% at logon, so a clone no longer inherits the warm-up
# BDD debris (live rp session registries among it) that a job could restore
# from a colliding temp path. The only on-disk differences from 910 are that
# updated script and an emptied %TEMP%, so 911 is functionally identical to 910
# for build/test. 910 is retained only for rollback, until 911's clones are
# proven and 910's own clones have all recycled.
#
# 926 gives every clone a unique hostname and closes an ownership hole the
# SDK installers had left. It descends from 920 through five intermediate
# captures (921-925) that were superseded before deployment and have
# since been destroyed; 920 remains, and is the rollback.
#
# The hostname half: a hostname is a DHCP identity (option 12), not just a
# label, and 920's clones all came up as `ci-bench` — pinned by the cloud-init
# user snippet, not by the image — so three concurrent Linux slots presented
# one identity to the router and collided on a lease. Enlarging the subnet
# cannot fix that, because the collision is in identity space rather than
# address space. 926 carries an `rp-hostname.service` running
# `rp-set-hostname`, which derives `runner-<last 6 of the NIC MAC>` from the
# address Proxmox regenerates per clone. It is ordered ahead of
# systemd-networkd, since a unit that runs after it is too late: the first
# DHCP request already carried the template's name, and that is the request
# that takes the lease. Its cloud-init snippet is a copy carrying
# `preserve_hostname: true` rather than a pinned name, so cloud-init no longer
# stamps the template's hostname back over it; the copy leaves 920's snippet
# untouched so 920 stays a clean rollback.
#
# The ownership half: the QHY SDK installer had left /etc, /usr, /usr/sbin,
# /usr/lib, /usr/share and their udev and firmware subdirectories owned by
# `ci` at mode 775, and every template through 921 inherited it — 920 among
# them, so the hazard is live until this roll lands. Directory
# write permission governs unlink and create regardless of who owns the files
# inside, so the unprivileged job account could swap out binaries under
# /usr/sbin, libraries under /usr/lib, and udev rules that root executes: a
# local escalation to root inside the clone. All are root:root 755 in 926.
#
# /usr/local needs the same care for a subtler reason, and blanket-excluding
# it as "the SDK's tree" is wrong: rp-set-hostname lives in /usr/local/sbin
# and systemd executes it as root at early boot. Its own file and directory
# were already root-owned, but /usr/local itself was ci-writable — and write
# permission on the parent is what governs renaming `sbin` and substituting a
# directory, so the file's ownership did not protect it. /usr/local,
# /usr/local/sbin and /usr/local/bin are therefore root:root 755 in 926. The
# SDK's own subtrees below them (include, lib, testapp, udev, fx3load, doc,
# riffa_linux_driver, cmake_modules) stay ci-owned, so jobs that write there
# are unaffected.
#
# /tmp and /var/tmp are empty at capture. That buys no correctness on its own,
# because tmpfiles.d ships `D /tmp` and systemd-tmpfiles empties /tmp on every
# boot regardless; it saves each clone the I/O of clearing a populated
# directory during boot. /var/tmp gets no such treatment — its tmpfiles line
# is commented out — so anything left there would persist for the life of the
# template.
#
# 920 is the Linux half of the earlier 920/910 generation,
# which were byte-identical rebuilds of 919/909 with RP_LAN_CACHE_URL repointed
# after the runner VLAN's renumbering to a /16 (the cache endpoint moved with
# it; the address itself is deliberately not recorded in this public repo).
# 911 inherits that repoint from 910 unchanged. 920 also carries the one-time
# `bazel coverage //...` warmup
# introduced in 918, so its Bazel output base already holds the nightly
# toolchain + instrumented externals — that keeps the pooled `bazel coverage`
# leg zero-WAN instead of re-fetching the nightly toolchain on every ephemeral
# clone. (Earlier lineage: 919/909 repointed RP_LAN_CACHE_URL at the NAS-hosted cache
# on the 25GbE fabric; 918 added the coverage warmup; 917 replaced the first
# cipool Linux template 907, which shipped a populated /etc/machine-id and so
# handed every clone the same DHCP identity and IP; all are built with
# machine-id wiped.)
# Both templates carry firewall=1 on their NIC, so clones inherit it and the
# per-clone policy written in slot_loop takes effect (clone-to-clone isolation).
#
# Two Windows slots share template 911: GitHub dispatches a Windows pool job to
# whichever clone is free, so a second slot removes the cross-branch queue that
# a single Windows runner created. The shared-autologon-credential concern this
# raised (both clones hold the same local admin password) is mitigated by the
# NIC isolation below — a compromised clone cannot reach a peer's SMB/RDP/WinRM.
SLOTS=(
  "runner-linux1|926|9100|linux|[\"self-hosted\",\"Linux\",\"X64\",\"proxmox-ephemeral\"]"
  "runner-linux2|926|9101|linux|[\"self-hosted\",\"Linux\",\"X64\",\"proxmox-ephemeral\"]"
  "runner-linux3|926|9102|linux|[\"self-hosted\",\"Linux\",\"X64\",\"proxmox-ephemeral\"]"
  "runner-win|911|9200|windows|[\"self-hosted\",\"Windows\",\"X64\",\"proxmox-ephemeral-windows\"]"
  "runner-win2|911|9201|windows|[\"self-hosted\",\"Windows\",\"X64\",\"proxmox-ephemeral-windows\"]"
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

# Filter: the storage tokens named by volume lines of a qm config dump on
# stdin (a disk line reads "scsi0: cipool:base-920-disk-0/vm-9100-disk-0,..."
# and names storage `cipool`).
#
# Deliberately NOT a device-key allowlist (scsi/ide/efidisk/...): for a
# safety gate, an unrecognised key fails in the unsafe direction — the
# unlisted volume's storage is silently never probed and the destroy gets a
# green light while the journal shows the gate working. Instead any
# "<key>: <token>:..." line counts, with the token anchored to the PVE
# storage-id charset — which is also what keeps a by-path passthrough
# ("scsi1: /dev/disk/by-path/pci-0000:00:17...") from injecting a bogus
# token, since a path starts with `/`. `qm destroy` skips absolute-path
# volids entirely, so such a line must never gate anything. "ide2:
# none,media=cdrom" has no second colon and never matches; a free-text value
# that happens to fit ("description: todo: rebalance") is dropped by the
# caller's validation against the storages that actually exist — one that
# names a real storage merely gates on it, the safe direction. ISO-backed
# CD-ROMs ("local:iso/foo.iso") are excluded by their iso/ volume path: `qm
# destroy` does not remove an ISO, so its storage being down must not defer
# a teardown. The exclusion is on the path, NOT on media=cdrom — the
# cloudinit volume is also media=cdrom, and it is the one volume whose leak
# wedges the slot. Known limit, stated so this does not read as
# authoritative: `qm config` output omits snapshot sections, so a snapshot's
# vmstate volume on some other storage is invisible here — pool clones never
# carry snapshots.
volume_storage_tokens() {
  sed -n -E -e '/^[a-z]+[0-9]*: [^:]+:iso\//d' \
      -e 's/^[a-z]+[0-9]*: ([A-Za-z][A-Za-z0-9_.-]*):.*/\1/p' \
    | sort -u
}

# Filter: the volume ids ("<storage>:<volname>") named by volume lines of a
# qm config dump on stdin — the same lines volume_storage_tokens keys on,
# whole rather than truncated to the storage. The volname charset excludes
# whitespace, so a free-text value with a colon in it ("description: todo:
# rebalance") does not parse as a volume the way it would with `[^,]+`.
volume_ids() {
  sed -n -E -e '/^[a-z]+[0-9]*: [^:]+:iso\//d' \
      -e 's/^[a-z]+[0-9]*: ([A-Za-z][A-Za-z0-9_.-]*:[A-Za-z0-9_./-]+).*/\1/p' \
    | sort -u
}

# Run a clone's ZFS-backed volumes with sync=disabled, before it first boots.
#
# Why: the pool's clone disks are zvols on one NVMe, and every flush the
# guest issues (ext4/NTFS journal commits, fsync from Bazel, the runner, the
# services under test writing configs, FITS frames and OmniSim profiles)
# becomes a synchronous ZFS write that queues behind the vdev's small
# sync-write slot count. With several busy clones the queue wait for those
# writes was measured in seconds and, at the tail, tens of seconds — during
# which a guest's whole filesystem freezes: every process needing a journal
# handle blocks while timers keep firing, so any wall-clock-bounded step whose
# work touches disk (a harness PUT the service answers only after a probe
# write, a 10 s wait for a config file to appear) times out. Coverage runs
# suffered most because instrumented suites write the most and run longest,
# but the freeze is host-wide, not per-suite.
#
# sync=disabled makes the guest's flushes complete immediately (no ZIL write;
# the data still reaches disk with the next transaction group). The cost is
# that a host crash can lose the last few seconds of a clone's writes and
# leave its filesystem inconsistent — for a clone that is destroyed after its
# single job and re-cloned from the template, that is no loss at all. Set per
# clone rather than on the pool root on purpose: the templates keep the
# default and stay durable through a rebuild.
#
# Volumes are resolved through `pvesm path`, which for zfspool storage names
# the zvol without activating anything; a path outside /dev/zvol/ (a clone on
# some other storage type) is simply skipped. The dataset must then carry
# this VMID's own name (`vm-<vmid>-*`, the same guard destroy_clone's leak
# sweep applies): volume_ids keys on line shape, not on a device-key
# allowlist, so a free-text value that happens to look like a volid
# ("description: cipool:base-920-disk-0") resolves like one — and unlike
# the destroy gate, where an unrecognised line failing *closed* is the safe
# direction, here acting on it would relax a dataset that is not this
# clone's, the template's included. The guard makes that impossible
# regardless of how the config parses. This is a performance property, not
# a safety one, so a volume that cannot be relaxed is reported and the clone
# boots anyway — a slot serving jobs slowly beats a slot serving none.
# Returns non-zero when any volume was left at its default; stdout carries
# the per-volume reasons and the count that did take, for the caller's log.
relax_clone_sync() {
  local vmid=$1 cfg vol path ds relaxed=0 failed=0
  if ! cfg=$(qm config "$vmid" 2>/dev/null) || [ -z "$cfg" ]; then
    echo "the VM config is unreadable"
    return 1
  fi
  for vol in $(volume_ids <<<"$cfg"); do
    if ! path=$(timeout -k 5 30 pvesm path "$vol" 2>/dev/null); then
      echo "$vol: pvesm path failed"
      failed=1
      continue
    fi
    case "$path" in
      /dev/zvol/*) ds=${path#/dev/zvol/} ;;
      *) continue ;;
    esac
    case "$ds" in
      */vm-"$vmid"-*) ;;
      *)
        echo "$vol: resolves to $ds, which is not this clone's own dataset; left alone"
        failed=1
        continue ;;
    esac
    # Read back rather than trust the exit code: the readback is what proves
    # the guest's flushes will be cheap from its first boot. Every call here
    # is bounded (30 s each: pvesm path above, zfs set, zfs get), so a
    # wedged pool costs this slot up to ~1.5 minutes per volume — minutes
    # for a clone, never forever.
    if timeout -k 5 30 zfs set sync=disabled "$ds" >/dev/null 2>&1 \
       && [ "$(timeout -k 5 30 zfs get -H -o value sync "$ds" 2>/dev/null)" = disabled ]; then
      relaxed=$((relaxed + 1))
    else
      echo "$ds: sync=disabled did not take"
      failed=1
    fi
  done
  echo "sync=disabled on $relaxed zvol(s)"
  [ "$failed" -eq 0 ]
}

# Decide whether a VM is safe to destroy, and with what storage inventory.
# On exit 0 the gate passed and stdout carries the validated storages backing
# the VM's volumes — possibly none: a readable config that references no
# volumes cannot leak anything and MUST pass, or a clone killed inside `qm
# clone`'s config-first window (the temp config exists, no disks yet) would
# defer forever and park its slot on a state the pre-gate script recovered
# from. On exit 1 stdout carries the refusal reason instead; the caller logs
# it. Reasons are specific because they need different operator responses:
# "cannot import" is a storage problem, "timed out" may be a faulted vdev
# with a D-state import, "storage.cfg unreadable" is pmxcfs down.
#
# The probe is `pvesm list <storage> --vmid <vmid>`, not `pvesm status
# --storage`: status filters its OUTPUT to one storage but activates every
# enabled storage first, so a down NFS ISO store would block teardowns of
# VMs living entirely on a healthy pool. list scopes activation to the one
# storage asked about, exits non-zero when that activation fails, and still
# triggers the on-demand import this probe heals by — a ZFS pool that is
# importable but was not imported at boot comes back imported. Validation
# reads /etc/pve/storage.cfg directly because, unlike pvesm, the read
# triggers no activation; its section headers are "<type>: <id>". timeout -k
# matters: a TERM-immune activation (D-state zfs import) ignores the polite
# signal, and without the follow-up KILL the command substitution would hold
# this slot hostage on the open pipe — the exact stall the bound exists to
# prevent.
storage_gate() {
  local vmid=$1 cfg tokens defined storages st err rc
  if ! cfg=$(qm config "$vmid" 2>/dev/null) || [ -z "$cfg" ]; then
    echo "the VM config is unreadable"
    return 1
  fi
  tokens=$(volume_storage_tokens <<<"$cfg")
  [ -z "$tokens" ] && return 0
  # Whitespace-tolerant on purpose: PVE writes "<type>: <id>" with one
  # space, but a hand-edited file must not shrink the defined list — a
  # missed definition makes that storage's volume tokens drop out as
  # not-storages, which UN-gates them, the unsafe direction.
  # awk's status counts for the same reason the comment above gives: a read
  # that emits one section name and then fails shrinks the defined list, and a
  # shrunken list is precisely the un-gating direction described there. Non-
  # empty output is not a completed read.
  # $2 is awk's field reference, not a shell variable — wrapping awk in
  # `timeout` is what stops shellcheck recognising the command and its quoting.
  # shellcheck disable=SC2016
  if ! defined=$(timeout -k 5 30 awk '/^[a-z]+:[ \t]+[A-Za-z][A-Za-z0-9_.-]*[ \t]*$/ {print $2}' "$PVE_CONF_ROOT/storage.cfg" 2>/dev/null) ||
    [ -z "$defined" ]; then
    echo "no storages readable from /etc/pve/storage.cfg (pmxcfs down?)"
    return 1
  fi
  # A token that names no defined storage is not a volume reference (free
  # text, or a storage since removed from the cluster — qm destroy could not
  # free such a volume either, so refusing on it would park the slot for
  # nothing).
  storages=$(grep -Fx -f <(printf '%s\n' "$defined") <<<"$tokens")
  for st in $storages; do
    err=$(timeout -k 5 30 pvesm list "$st" --vmid "$vmid" 2>&1 >/dev/null)
    rc=$?
    if [ "$rc" -ne 0 ]; then
      if [ "$rc" -eq 124 ]; then
        echo "storage '$st' probe timed out after 30s"
      else
        echo "storage '$st' is not active${err:+: ${err//$'\n'/'; '}}"
      fi
      return 1
    fi
  done
  printf '%s\n' "$storages"
}

# Tear a clone down: stop it, drop its marker, deregister its runner, destroy
# the VM. Takes the runner id explicitly when the caller has just minted it but
# the marker was not written yet (a mint that succeeded then failed to inject);
# otherwise the id comes from the injection marker, which is where every other
# teardown path — clean finish, wedge reclaim — carries it. An orphan the
# reconcile destroys usually has neither, because it never received a config
# and so no runner was ever registered for it. "Usually" is load-bearing: a
# clone caught between a successful injection and its marker write did get a
# config and does have a registration, and nothing survives to name it. Do not
# read the no-id reconcile path as proof that no runner can exist -- see the
# ordering note at the marker write.
#
# Returns non-zero when the VM was not destroyed (storage inactive, destroy
# failed). Callers need no special handling: every path converges on the
# reconcile, which finds the marker-less VM still present and retries the
# teardown until it takes — at 30 seconds, backing off toward five minutes
# while it keeps deferring (see defer_sleep in slot_loop).

# Free one leaked volume, and decide the outcome from the storage rather than
# from an exit status.
#
# `pvesm free` exits 0 even when the imgdel task it starts fails: under
# storage-lock contention it returns success while the task ends with
# "can't lock file ... got timeout" and the volume is still there. Branching
# on that status logs a recovery that did not happen, which is worse than
# leaking silently — the failure branch never fires, so nothing sends anyone
# to the runbook, and the next clone of this VMID then wedges on "dataset
# already exists" while the journal positively asserts the volume was freed.
#
# So the verdict comes from re-listing, exactly the way the leak was detected
# a few lines above; an exit status that does not track the outcome cannot
# decide it. The retry is the secondary half: contention passes, so a later
# attempt usually takes — but a retry that still trusted the exit status would
# report unearned success just as readily, which is why the check comes first
# and the retry second.
#
# A listing that cannot be read is NOT evidence of removal, so it never yields
# the success verdict. It does not yield the "still there" verdict either,
# though: claiming a volume is still listed when nothing could be listed would
# repeat, in the failure branch, the same sin this function exists to remove
# from the success branch. The two are reported separately.
#
# Exit status: 0 = confirmed gone, 1 = confirmed still present, 2 = could not
# confirm because the storage would not list, 3 = could not confirm because the
# matcher itself failed. The last two are separated because they send an
# operator to different places -- a storage that will not answer is the
# immediate fault in one, and is working fine in the other -- and a message
# naming the wrong cause is the failure this whole function is about.
#
# Two more mean the run was STOPPED rather than finished, and they are kept
# apart for the same reason 2 and 3 are: they send an operator to different
# places. 4 is a VM config existing again for this VMID -- somebody created a
# VM, and the volume may now be its. 5 is the config filesystem no longer
# answering, so ownership cannot be established either way. Both differ from
# the three above in that the volume's fate was not established and the cause
# is neither the storage nor the matcher, but the licence itself expiring.
free_leaked_volume() {
  local vmid=$1 vol=$2 attempt=1 listing listed=0 matched=1 cfg
  # Separate `local`: an assignment cannot read a name bound earlier in the
  # same `local`, so folding this in would read an outer `vol` instead.
  local st=${vol%%:*}
  while :; do
    # Ownership is re-established before EVERY attempt, not once by the caller.
    # Each attempt can spend a bounded 30s freeing and another listing, so a
    # full run covers a couple of minutes of repeated deletion, and the
    # dangerous sequence in there is not exotic: a free that actually worked, a
    # listing that was merely flaky, and the now-free name reissued to a VM
    # recreated in the meantime. The next attempt would delete that VM's
    # volume. Both callers hold the same invariant on entry -- this volume
    # belongs to a VMID with no VM config -- so checking it here is what keeps
    # it true for the whole run instead of only at the start of it.
    vm_config_state "$vmid"
    cfg=$?
    if [ "$cfg" -eq 0 ]; then
      return 4
    fi
    if [ "$cfg" -ne 1 ]; then
      return 5
    fi
    timeout -k 5 30 pvesm free "$vol" >/dev/null 2>&1
    if listing=$(timeout -k 5 30 pvesm list "$st" --vmid "$vmid" 2>/dev/null); then
      listed=1
      matched=1
      # The match runs INSIDE awk on purpose. Piping into `grep -q` looks
      # equivalent and is not: grep exits the moment it matches, awk takes
      # SIGPIPE writing the next row, and under `set -o pipefail` the pipeline
      # then reports 141 — which the enclosing `!` would read as "not found"
      # and turn a present volume into a confirmed removal. That is precisely
      # the false success this function was written to eliminate, so it must
      # not be reintroduced by the check itself. awk consumes all input and
      # decides in one process, leaving nothing for a signal to distort.
      #
      # Status is read exactly, not as true/false. awk exits 0 for a match and
      # 1 for a clean no-match, but any other value means awk itself failed --
      # and lumping that in with "no match" would return "confirmed gone"
      # because the matcher broke, which is the same false success by a third
      # route. Only a 1 is removal; anything else fails closed.
      awk -v want="$vol" '$1 == want { found = 1 } END { exit !found }' \
        <<<"$listing"
      case $? in
        0) : ;;         # listed — fall through to the retry
        1) return 0 ;;  # genuinely absent
        *) matched=0 ;; # the matcher failed; nothing was established
      esac
    else
      listed=0
    fi
    if [ "$attempt" -ge "$FREE_ATTEMPTS" ]; then
      [ "$listed" -eq 0 ] && return 2
      [ "$matched" -eq 0 ] && return 3
      return 1
    fi
    attempt=$((attempt + 1))
    sleep "$FREE_RETRY_SLEEP"
  done
}

# Read a marker file, accepting only a complete record.
#
# `read` succeeds only on a newline-terminated line, and that is the proof
# that whatever wrote the file finished writing it. A marker that lost its
# write holds a PREFIX of a runner id, and a prefix is not a smaller id — it
# names a DIFFERENT runner, which is worse than naming none. The health check
# would then watch a stranger's runner and reclaim this clone on its state,
# and the teardown would deregister that stranger while this clone's own
# registration leaked.
#
# Markers published by rename cannot be short, but two sources can be: the
# unpublished .tmp, and an .injected left by an older build of this script,
# which wrote it in place. Markers live in /run, so they survive the service
# restart that a script upgrade is.
#
# Bash assigns the partial data even when read fails, so it is cleared rather
# than trusted not to have been set. Redirection order is load-bearing: bash
# applies them left to right, so stderr must be redirected BEFORE the input
# redirect, or a missing file — the ordinary case for .tmp — prints an
# unattributed error into the journal.
read_marker() {
  local value=""
  IFS= read -r value 2>/dev/null <"$1" || value=""
  printf '%s' "$value"
}

destroy_clone() {
  local vmid=$1 rid=${2:-} code out rc
  qm stop "$vmid" >/dev/null 2>&1
  [ -z "$rid" ] && rid=$(read_marker "$STATE_DIR/$vmid.injected")
  # The unpublished marker counts too. Write-then-rename is atomic at the
  # rename, not across the pair, so a service killed between them leaves the id
  # in .tmp and nothing in .injected — a clone that has a registration and no
  # published way to name it, which is the leak this reads back.
  [ -z "$rid" ] && rid=$(read_marker "$STATE_DIR/$vmid.injected.tmp")
  # Best-effort, but not silently so: the reconcile reads a surviving marker as
  # proof this clone holds a live job, so a marker that outlives its clone
  # suppresses the very recovery that would clean up after this teardown.
  rm -f "$STATE_DIR/$vmid.injected" "$STATE_DIR/$vmid.injected.tmp" 2>/dev/null ||
    log "$vmid" "could not clear the injection marker in $STATE_DIR; until it goes the reconcile will read this clone as holding a live job"
  # Deregister before destroying the VM so the org runner list does not
  # accumulate one offline entry per Windows job (see deregister_runner). An
  # empty id usually means nothing was ever minted for this clone — but not
  # on every path. A clone torn down by the reconcile between a successful
  # injection and its marker write does have a registration, and no marker
  # left to name it from, so that one is deregistered from nothing and the
  # runner outlives its clone (see the ordering note at the marker write).
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
  # `qm destroy` on a VM whose backing storage is not active removes the
  # config but leaves the volumes behind — the boot-race wedge described in
  # the deployment notes, reachable again on any mid-life restart because
  # zfs-import-cache is wanted, not required, by zfs-import.target, so a
  # failed import does not hold zfs.target back. Defer instead of destroying
  # blind: the probe's own activation attempt is often what brings the
  # storage back, and the reconcile retries the teardown until it takes.
  # `storages` doubles as the leak-sweep inventory below — it must be taken
  # BEFORE the destroy, since afterwards there is no config to read. The
  # deferral line carries the runner id because the marker is already
  # consumed and the deregistration above ran its one attempt: if that
  # attempt failed, this line is the only place the id survives for an
  # operator to clean up by hand. (Deliberately not a second marker file —
  # marker lifecycles against hypervisor state have no race-free fixed
  # point, and a leaked registration is inert.)
  local storages
  if ! storages=$(storage_gate "$vmid"); then
    log "$vmid" "deferring destroy (runner id ${rid:-none}): ${storages:-no reason reported}"
    return 1
  fi
  out=$(qm destroy "$vmid" --purge 2>&1)
  rc=$?
  out=${out//$'\n'/'; '}
  if [ "$rc" -ne 0 ]; then
    log "$vmid" "destroy failed: $out"
    return 1
  fi
  # qm destroy exits 0 even when it could not remove a volume — it warns and
  # carries on — and that leak is exactly what wedges the next clone of this
  # VMID on "dataset already exists". Detect it structurally rather than by
  # qm's wording (which no test pins and a PVE upgrade may reword): list
  # what this VMID still owns on the storages gated above. Anything found is
  # an orphan by construction — the config destroy just succeeded, so
  # nothing references it — which is the strongest license to free a volume
  # this script will ever hold; freeing now is what turns the would-be wedge
  # into self-healing. The name guard is a belt on top of that construction:
  # never touch anything that is not this VMID's own volume, base images
  # most of all. A free that fails is left for the recovery runbook.
  local leaked="" vol st sweep_out sweep_rc
  for st in $storages; do
    sweep_out=$(timeout -k 5 30 pvesm list "$st" --vmid "$vmid" 2>/dev/null)
    sweep_rc=$?
    if [ "$sweep_rc" -ne 0 ]; then
      # The gate passed moments ago, so a sweep failing here means the
      # storage went away mid-teardown. Say so rather than skipping
      # silently — a missed sweep is a possible unlogged leak, and silence
      # here is what this block exists to end. (stderr is dropped from the
      # capture on purpose: a warning line mixed into stdout would parse as
      # a volume name.)
      log "$vmid" "leak sweep of storage '$st' failed (rc $sweep_rc); a leaked volume may remain — the recovery runbook applies if the next clone wedges"
      continue
    fi
    leaked+="$(printf '%s\n' "$sweep_out" | awk '$1 != "Volid" && NF {print $1}')"$'\n'
  done
  for vol in $leaked; do
    case "${vol#*:}" in
      vm-"$vmid"-* | */vm-"$vmid"-*)
        # Named before the attempt, not only after it. By this point the VM
        # config is already gone, and the free below can run for several
        # rounds of timeout-and-sleep, so a service restart landing in that
        # window leaves a volume this pass never settled. sweep_orphan_volumes
        # picks it up on the next failed clone, so it is not lost -- but that
        # sweep can decline or fail to confirm in turn, and then this line is
        # the only place the volume was ever named.
        log "$vmid" "destroy left volume $vol behind; freeing it now"
        free_leaked_volume "$vmid" "$vol"
        case $? in
          0) log "$vmid" "destroy left volume $vol behind (qm said: $out); freed it, confirmed gone from the storage" ;;
          1) log "$vmid" "destroy left volume $vol behind (qm said: $out) and it is still listed after $FREE_ATTEMPTS attempts; the recovery runbook applies" ;;
          2) log "$vmid" "destroy left volume $vol behind (qm said: $out) and storage '${vol%%:*}' would not list, so it could not be confirmed gone after $FREE_ATTEMPTS attempts; the recovery runbook applies" ;;
          4) log "$vmid" "destroy left volume $vol behind (qm said: $out) and a VM config for $vmid appeared while it was being freed, so freeing stopped; note this slot reclaims that VMID on its next pass, so the VM needs moving off it as well as the volume settling — the recovery runbook applies" ;;
          5) log "$vmid" "destroy left volume $vol behind (qm said: $out) and the config filesystem stopped answering, so it could not be shown to be an orphan; the recovery runbook applies" ;;
          *) log "$vmid" "destroy left volume $vol behind (qm said: $out) and the volume match failed on a storage that listed fine, so it could not be confirmed gone after $FREE_ATTEMPTS attempts; the recovery runbook applies" ;;
        esac ;;
      *)
        log "$vmid" "destroy left unexpected volume $vol behind; leaving it for the recovery runbook" ;;
    esac
  done
  # Drop the isolation policy only when the destroy actually removed the VM.
  # Keying cleanup off `qm destroy` succeeding — not a `qm status` probe, which
  # can fail transiently while the clone still exists — keeps a still-present
  # clone's inbound DROP in place; the caller retries the destroy. A recreated
  # VMID rewrites its .fw before boot, so a briefly-orphaned file is harmless.
  #
  # Its status must not become this function's. Every caller reads a non-zero
  # return as "the VM is still there, retry the teardown", and
  # destroy_clone_holding_id retries on that reading without a bound: an
  # unwritable $FW_DIR would back a slot off forever over a stale file that a
  # recreated VMID overwrites before it boots. Report what the destroy did.
  rm -f "$FW_DIR/$vmid.fw" 2>/dev/null ||
    log "$vmid" "could not remove the firewall policy $FW_DIR/$vmid.fw; the clone is destroyed and a recreated VMID rewrites the file before boot, so the teardown still counts as done"
  return 0
}

# Tear a clone down with a runner id that exists only in the caller's scope,
# retrying until it takes.
#
# destroy_clone consumes the marker on its first call and spends its single
# deregistration attempt there, so a deferral -- the storage gate refusing
# while the pool is unavailable -- leaves nothing for a later reconcile to work
# from: that reconcile finds a marker-less VM and calls destroy_clone with no
# id at all. If the deregistration that already ran was the one that failed (an
# unreachable API reports 000), the id is gone for good and the registration
# outlives every retry that follows.
#
# The id is live only where it was minted, so the retry has to live there too.
# Backoff mirrors the reconcile's, for the same reason it has one: a storage
# outage must not have every slot hammering the failing pool. Unbounded is
# deliberate and matches the reconcile -- a teardown that cannot happen yet is
# not a teardown to give up on, and the slot has nothing else to do until it
# completes.
destroy_clone_holding_id() {
  local vmid=$1 rid=$2 backoff=30
  until destroy_clone "$vmid" "$rid"; do
    sleep "$backoff"
    backoff=$((backoff * 2))
    [ "$backoff" -gt 300 ] && backoff=300
  done
}

# Reclaim volumes still owned by a VMID that has no VM, so a slot can clone
# again instead of wedging on "dataset already exists" forever.
#
# Nothing else recovers this. destroy_clone -- and the leak sweep inside it --
# is only reachable while `qm status` succeeds, so once a config is gone the
# slot loop only ever clones, fails, sleeps, and clones again. Volumes get
# stranded there by an interrupted teardown (the config is destroyed before the
# leaked volume is freed), by a sweep that could not list its storage, and by
# leaks from outside the gated teardown entirely -- a pre-gate deployment, or
# `qm clone`'s own rollback on a half-imported pool.
#
# This frees storage, so what licenses it matters more than what triggers it.
# Three conditions must all hold, and each closes a way of being wrong:
#
#   1. /etc/pve/storage.cfg can be read to completion -- awk's status as well
#      as its output, since a read that produced some names and then failed is
#      not a completed one -- and names at least one storage. This is a
#      LIVENESS check on the config filesystem, not a validation of the file:
#      what it establishes is that /etc/pve is mounted and answering, which is
#      what makes condition 2 mean anything. It must come FIRST -- with pmxcfs
#      down, /etc/pve/qemu-server is empty and every volume on the host looks
#      like an orphan. Absence of evidence is not evidence here.
#   2. /etc/pve/qemu-server/<vmid>.conf does not exist -- checked once up
#      front and again immediately before each free, because the listings in
#      between are bounded at 30s apiece. Deliberately a filesystem test
#      rather than `qm status`, which this script elsewhere refuses to trust
#      in the "gone" direction: a transient qm failure reading as "no VM"
#      would hand this function a running clone's disks.
#   3. The volume name is this VMID's own. `pvesm list --vmid` already scopes
#      the listing, and the name guard is the belt on top -- base images most
#      of all must never be reachable from here.
#
# Freeing goes through free_leaked_volume so there is one place that decides
# whether a volume actually went, rather than trusting `pvesm free`'s status.
# Whether <vmid> has a VM config: 0 it does, 1 it provably does not, 2 could
# not be established.
#
# `[ -e ]` on its own cannot separate "not there" from "could not look" —
# both are simply false — and that distinction is the entire licence to delete
# a volume. So the config directory is enumerated first: a listing that
# succeeds is what makes a missing file mean missing rather than unreadable.
# Callers must treat 2 exactly like 0 and act on 1 alone.
vm_config_state() {
  local dir="$PVE_CONF_ROOT/qemu-server" live
  # pmxcfs liveness first, and re-established on every call rather than
  # inherited from whoever checked it last. A config filesystem that goes away
  # mid-run leaves a qemu-server that is empty AND perfectly enumerable — the
  # one state in which an absent config file means the opposite of what it
  # says. Callers spend minutes inside retry loops, so "it was live when this
  # started" is not a fact any of them still holds by the time they delete.
  # $2 is awk's field reference, not a shell variable — wrapping awk in
  # `timeout` is what stops shellcheck recognising the command and its quoting.
  # shellcheck disable=SC2016
  if ! live=$(timeout -k 5 30 awk '/^[a-z]+:[ \t]+[A-Za-z][A-Za-z0-9_.-]*[ \t]*$/ {print $2}' "$PVE_CONF_ROOT/storage.cfg" 2>/dev/null) ||
    [ -z "$live" ]; then
    return 2
  fi
  # Explicitly a directory. `ls -A` on a readable regular FILE succeeds by
  # listing that file, and the test below would then be false for every VMID —
  # an enumeration that "worked" licensing deletion for a path that is not a
  # config directory at all. That fails open, the one direction this must not.
  [ -d "$dir" ] || return 2
  # Bounded like every other call that touches storage or the config
  # filesystem: a wedged pmxcfs can leave a read outstanding indefinitely, and
  # a slot stuck inside this function never reaches the retry or sleep that
  # would otherwise pace it. (A timeout cannot interrupt an uninterruptible
  # read — this bounds what can be bounded, which is the common case.)
  timeout -k 5 30 ls -A "$dir" >/dev/null 2>&1 || return 2
  [ -e "$dir/$1.conf" ] && return 0
  return 1
}

sweep_orphan_volumes() {
  local name=$1 vmid=$2 defined st listing vol verdict
  # awk's status matters as much as its output. A read that yields some section
  # names and then fails would otherwise pass for a healthy parse, and this
  # function deletes storage on the strength of that parse: a truncated list
  # means the config filesystem is not reliably readable, which is exactly when
  # an empty qemu-server directory must not be read as "the VM is gone". Both
  # halves fail closed into the same line, because both mean the same thing to
  # an operator — the storage list could not be trusted.
  if ! defined=$(awk '/^[a-z]+:[ \t]+[A-Za-z][A-Za-z0-9_.-]*[ \t]*$/ {print $2}' "$PVE_CONF_ROOT/storage.cfg" 2>/dev/null) ||
    [ -z "$defined" ]; then
    log "$name" "not sweeping orphaned volumes for $vmid: no storages readable from /etc/pve/storage.cfg (pmxcfs down?), which is also why an empty config directory cannot be read as proof the VM is gone"
    return
  fi
  vm_config_state "$vmid"
  case $? in
    1) : ;; # provably no config — the only state that licenses anything below
    0)
      log "$name" "not sweeping orphaned volumes for $vmid: it still has a VM config, so the reconcile owns its teardown"
      return ;;
    *)
      log "$name" "not sweeping orphaned volumes for $vmid: the VM config directory would not list, so an absent config is not proof the VM is gone"
      return ;;
  esac
  for st in $defined; do
    if ! listing=$(timeout -k 5 30 pvesm list "$st" --vmid "$vmid" 2>/dev/null); then
      # Skipping silently would drop the only evidence at the moment it is
      # worth most: a storage that will not answer is the likeliest reason this
      # slot keeps failing to clone, and without this line the journal says
      # only that the clone failed. The teardown sweep logs its equivalent for
      # the same reason. (stderr is dropped from the capture on purpose — a
      # warning line mixed into stdout would parse as a volume name.)
      log "$name" "could not list storage '$st' while sweeping orphaned volumes for $vmid; an orphan may remain there — the recovery runbook applies if this slot keeps failing to clone"
      continue
    fi
    for vol in $(printf '%s\n' "$listing" | awk '$1 != "Volid" && NF {print $1}'); do
      case "${vol#*:}" in
        vm-"$vmid"-* | */vm-"$vmid"-*) ;;
        *)
          log "$name" "leaving unexpected volume $vol on '$st' alone; it is listed against $vmid but is not one of its volumes"
          continue ;;
      esac
      # Re-read the config here, not only once at the top. Each storage listing
      # above can spend a bounded 30s, so the check that licensed this ran some
      # time ago. Nothing in the pool recreates a VMID in that window — the
      # slot loop is what sweeps, and it is single-threaded per slot — so what
      # is left is an operator building a VM on a pool VMID by hand. Rechecking
      # at the point of deletion is proportionate to that; it narrows the
      # window rather than closing it, and only a lock held across the check
      # and the free would close it.
      vm_config_state "$vmid"
      case $? in
        1) : ;;
        0)
          log "$name" "stopping the orphan sweep for $vmid: a VM config appeared while it was running, so $vol may belong to a live VM"
          return ;;
        *)
          log "$name" "stopping the orphan sweep for $vmid: the VM config directory stopped answering, so $vol can no longer be shown to be an orphan"
          return ;;
      esac
      log "$name" "volume $vol survives a VM that no longer exists; freeing it so $vmid can clone again"
      free_leaked_volume "$vmid" "$vol"
      verdict=$?
      case "$verdict" in
        0) log "$name" "orphaned volume $vol freed, confirmed gone from the storage" ;;
        1) log "$name" "orphaned volume $vol is still listed after $FREE_ATTEMPTS attempts; the recovery runbook applies" ;;
        2) log "$name" "storage '$st' would not list, so orphaned volume $vol could not be confirmed gone after $FREE_ATTEMPTS attempts; the recovery runbook applies" ;;
        4 | 5)
          # The licence expired mid-free, so nothing further in this pass is
          # licensed either -- same reasoning as the recheck above it.
          if [ "$verdict" -eq 4 ]; then
            log "$name" "stopping the orphan sweep for $vmid: a VM config appeared while $vol was being freed, so it can no longer be shown to be an orphan"
          else
            log "$name" "stopping the orphan sweep for $vmid: the config filesystem stopped answering while $vol was being freed, so nothing here can be shown to be an orphan"
          fi
          return ;;
        *) log "$name" "the volume match failed on a storage that listed fine, so orphaned volume $vol could not be confirmed gone after $FREE_ATTEMPTS attempts; the recovery runbook applies" ;;
      esac
    done
  done
}

slot_loop() {
  local name=$1 template=$2 vmid=$3 os=$4 labels=$5
  # Backoff for deferred teardowns (see the reconcile below): 30s doubling
  # to a 5-minute cap, so a storage outage does not have every slot hammer
  # the failing pool with activation attempts twice a minute, while recovery
  # is still noticed within one cap interval.
  local defer_sleep=30

  while true; do
    # Establish the invariant the rest of the iteration depends on: either the
    # VM does not exist, or it exists AND holds a real job. A clone with no
    # injection marker was created but never configured, so the health check
    # below cannot watch it — it has no runner id to watch — and the guest's
    # own no-config timeout is then the only thing that would end it, half an
    # hour of a slot held for nothing. Three ways to reach that state: this
    # service restarting mid-window, a destroy that did not take (a Proxmox
    # lock, say), and a teardown deferred by the storage gate — destroy_clone
    # consumes the marker before the gate, deliberately, since retaining it
    # would skip this reconcile and turn a deferral into a sleepless
    # destroy/defer spin. That is why the check runs every iteration rather
    # than once at startup, and why the log line says "no live-job marker"
    # rather than claiming the clone was never configured — for a deferred
    # teardown it was. A clone WITH a marker is left alone: an orchestrator
    # restart must never abort an in-flight job.
    if qm status "$vmid" >/dev/null 2>&1 && [ ! -e "$STATE_DIR/$vmid.injected" ]; then
      log "$name" "clone $vmid present with no live-job marker; destroying"
      # destroy_clone's return code is authoritative for whether the VM is
      # gone. Re-probing `qm status` here instead would read that probe's
      # own transient failure as "gone" and fall through into a doomed
      # clone of a VMID that still exists, mislogged as a clone failure.
      if ! destroy_clone "$vmid"; then
        sleep "$defer_sleep"
        defer_sleep=$((defer_sleep * 2))
        [ "$defer_sleep" -gt 300 ] && defer_sleep=300
        continue
      fi
      defer_sleep=30
    fi

    if ! qm status "$vmid" >/dev/null 2>&1; then
      # 2>&1 >/dev/null: capture stderr (errors, and the task-warning
      # trailer a warnings-only clone emits), drop stdout (worker progress
      # lines). Named logging matters here — before it, a failing clone
      # spoke only through qm's anonymous stderr, and attributing the
      # boot-race wedge to a slot meant grepping raw journal lines. A clone
      # that succeeds with warnings used to reach the journal through that
      # same passthrough, so re-emit what was captured instead of swallowing
      # it on the success path.
      if ! cerr=$(qm clone "$template" "$vmid" --name "$name" 2>&1 >/dev/null); then
        log "$name" "clone of template $template to $vmid failed: ${cerr//$'\n'/'; '}"
        # The commonest reason a clone of a pool VMID fails is a volume of its
        # own left over from a teardown that did not finish, and this is the
        # only place that can reclaim it: with no VM there is no route back to
        # destroy_clone. Unconditional rather than keyed on "dataset already
        # exists" -- qm's wording is not pinned by any test and a PVE upgrade
        # may reword it, exactly the reasoning the teardown sweep already
        # applies. On any other failure this is a few VMID-scoped list calls
        # that find nothing.
        sweep_orphan_volumes "$name" "$vmid"
        sleep 30
        continue
      fi
      [ -n "$cerr" ] && log "$name" "clone of template $template to $vmid warned: ${cerr//$'\n'/'; '}"
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
      # Relax the clone's zvols before first boot so every flush the guest
      # ever issues is cheap (see relax_clone_sync). Not fatal: a clone left
      # at sync=standard is slower and flakier, not unsafe, so it is logged
      # by name and booted rather than destroyed. The success line is logged
      # too — one line per clone — so a slot that silently stopped relaxing
      # (a renamed storage, a pvesm change) shows up in the journal.
      if rerr=$(relax_clone_sync "$vmid"); then
        log "$name" "clone $vmid: ${rerr//$'\n'/'; '}"
      else
        log "$name" "clone $vmid not fully relaxed to sync=disabled: ${rerr//$'\n'/'; '}"
      fi
      # A marker can outlive the clone it described: killing this service
      # between the destroy and its `rm` leaves one behind, and the next clone
      # of the same VMID would then look already-configured to the reconcile
      # above and never be recovered. Clearing here binds the marker to THIS
      # clone instance.
      # Fatal if it does not take, unlike the same removal in destroy_clone.
      # There the clone is going away and a surviving marker only misleads the
      # reconcile; here it would be inherited by the clone about to boot. A
      # service that then died between injecting this clone and publishing its
      # marker would leave the PREVIOUS runner's id describing the new clone:
      # the health check would probe a runner that is already gone, strike it
      # out, and reclaim a clone that is working a job — while the new
      # registration, unnamed by anything, leaked. Destroying now costs
      # nothing, because nothing has been minted or started yet.
      if ! rm -f "$STATE_DIR/$vmid.injected" "$STATE_DIR/$vmid.injected.tmp" 2>/dev/null; then
        log "$name" "could not clear the previous marker for $vmid in $STATE_DIR; destroying rather than boot a clone that would inherit a dead runner's id"
        destroy_clone "$vmid"
        sleep 30
        continue
      fi
      # A start refused at volume activation (a storage that went inactive
      # mid-life) would otherwise burn the full agent wait below and be
      # logged as the guest's failure. Name the real cause instead.
      if ! serr=$(qm start "$vmid" 2>&1 >/dev/null); then
        log "$name" "start of clone $vmid failed: ${serr//$'\n'/'; '}"
        destroy_clone "$vmid"
        sleep 30
        continue
      fi

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
        # the registration, with no marker left to recover its id. Held across
        # deferrals for the same reason (see destroy_clone_holding_id): letting
        # this return early hands the next reconcile a clone it cannot name.
        destroy_clone_holding_id "$vmid" "$RUNNER_ID"
        sleep 30
        continue
      fi
      # Only now is the clone recoverable across a restart of this service.
      #
      # Ordering is deliberate, and this is the safe direction. Dying in the
      # sliver between a successful injection and this line loses a clone that
      # was about to run a job — one aborted job, and the pool immediately
      # rebuilds the slot. Writing the marker FIRST would instead leave a
      # clone that never got a config wearing an in-flight job's marker: the
      # reconcile would adopt it rather than clear it, and nothing would end
      # it until the health check reclaimed it on strikes or the guest's own
      # no-config timeout powered it off — minutes of a slot held for nothing,
      # against a rebuild measured in seconds here. Both directions recover;
      # this one recovers at once and without waiting on a timer to notice.
      #
      # What this direction costs is a leaked registration whenever the clone
      # is torn down with no readable marker: the reconcile calls destroy_clone
      # with no id and has nothing to read one from, so the runner minted just
      # above outlives its clone as an inert offline entry. A restart inside
      # this window is the obvious way there, and the failed-injection path
      # above sidesteps it by passing RUNNER_ID explicitly -- but a marker that
      # never landed reaches the same place, which is why the write below is
      # checked and published by rename rather than left to chance.
      #
      # Note also that the guest deletes .jitconfig the moment it reads
      # it (~2 s), so the file's presence cannot be used to detect a running
      # job — proving liveness needs the runner's state from the GitHub API.
      # That is also why the marker carries the runner id rather than being
      # empty: it is what lets the health check below survive a restart of
      # this service and keep watching a clone it did not itself register.
      # Write-then-rename, because a reader here is the reconcile deciding
      # whether a clone holds a live job: a half-written marker is not a
      # smaller version of the truth, it is a runner id that names nothing.
      # Rename within one directory is atomic, so the marker is either the
      # previous state or the complete new one. An unchecked `>` would also
      # fail silently on a full or read-only /run and leave this clone with no
      # health check and a registration nobody can free -- worth a line rather
      # than a shrug.
      if ! printf '%s\n' "$RUNNER_ID" >"$STATE_DIR/$vmid.injected.tmp" ||
        ! mv -f "$STATE_DIR/$vmid.injected.tmp" "$STATE_DIR/$vmid.injected"; then
        # Carrying on here would leave the worst clone this loop can produce:
        # the wait below reads its runner id back from the marker, so an absent
        # one disables the health check for good, and a guest that then wedges
        # holds the slot with nothing left to notice. Teardown would have no id
        # to deregister either, so the registration leaks as well.
        #
        # RUNNER_ID is still in hand at this instant and nowhere else, so this
        # is the last point where either can be avoided. Tear down explicitly
        # with it, exactly as the failed-injection path above does, and let the
        # slot rebuild: an aborted job seconds after registration is the same
        # price that path already accepts, and a self-healing failure beats an
        # unwatched clone. A persistent cause (a full or read-only /run) then
        # shows as a visible rebuild loop instead of one line followed by
        # silence.
        rm -f "$STATE_DIR/$vmid.injected.tmp"
        log "$name" "could not record the runner id for clone $vmid in $STATE_DIR; destroying it rather than running it unwatched"
        destroy_clone_holding_id "$vmid" "$RUNNER_ID"
        sleep 30
        continue
      fi
      log "$name" "runner clone $vmid up and registered"
    fi

    # The GitHub runner id of whatever clone occupies this slot — the one just
    # injected above, or one inherited from before a restart of this service.
    # An empty marker (one written by an older version of this script) simply
    # means no health check for that clone; the wait then behaves as it did
    # before, which is the right way to degrade.
    runner_id=$(read_marker "$STATE_DIR/$vmid.injected")
    # A marker that yields no id is a real state, not an impossible one: older
    # builds of this script wrote .injected in place, so one killed mid-write
    # survives in /run across the upgrade that a restart is, and read_marker
    # refuses a partial record rather than treat a prefix of an id as an id.
    #
    # The reconcile deliberately keeps such a clone -- a marker present means
    # hands off, because it may be working a job, and aborting real work is the
    # worse error. What it cannot do is WATCH it, and that is the gap worth
    # naming: until this clone's job ends, the slot has no wedge detection at
    # all, which is the single thing the health check exists to provide.
    if [ -z "$runner_id" ] && [ -e "$STATE_DIR/$vmid.injected" ]; then
      log "$name" "clone $vmid has an injection marker with no usable runner id; it keeps its slot but runs without a health check until it finishes"
    fi

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
    # Back off on a deferral here too, rather than leaving it to the reconcile.
    # Routing it through the reconcile only works while the marker is gone, and
    # the marker removal above is best-effort: one that survives (a read-only
    # /run) makes the reconcile skip this clone entirely, and the loop then
    # spins — status reads `stopped`, the wait falls straight through, and this
    # teardown defers again, with nothing anywhere pausing between attempts.
    # That is the storage-hammering the backoff exists to prevent, arriving by
    # the one path that had no backoff of its own.
    # Hold the id across deferrals whenever there is one. destroy_clone
    # consumes the marker and spends its single deregistration attempt on the
    # first call, so a deferral leaves a later reconcile with a marker-less VM
    # and no id at all — and the attempt that already ran is exactly the one
    # that may have failed. runner_id is in scope right here and nowhere else
    # afterwards, so this is the last point it can be kept.
    if [ -n "$runner_id" ]; then
      destroy_clone_holding_id "$vmid" "$runner_id"
    elif ! destroy_clone "$vmid"; then
      # No id to hold (a legacy or unusable marker), so keep the in-place
      # backoff: routing a deferral through the reconcile only works while the
      # marker is gone, and its removal is best-effort.
      sleep "$defer_sleep"
      defer_sleep=$((defer_sleep * 2))
      [ "$defer_sleep" -gt 300 ] && defer_sleep=300
      continue
    fi
    defer_sleep=30
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
