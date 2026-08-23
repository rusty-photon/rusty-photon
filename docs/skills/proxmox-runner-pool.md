# Skill: Proxmox Ephemeral Runner Pool (x86_64 Linux)

## When to Read This

Read this before touching `.github/workflows/proxmox-runner-test.yml`,
`tools/ci/rp-runner-pool.sh`, the runner VM template, or before pointing any
new workflow at the `proxmox-ephemeral` runner label.

## Changing the Orchestrator

`tools/ci/rp-runner-pool.sh` has tests, and they run in the ordinary gate:

```sh
bazel test //tools/ci:all        # or just `bazel test //...`
shellcheck tools/ci/rp-runner-pool.sh
```

They are hermetic — every external command is stubbed, and the config
filesystem is a tmpdir the harness points `PVE_CONF_ROOT` at (the same
variable production resolves from `RP_PVE_CONF_ROOT`) — so they need no
Proxmox host and run anywhere. **Keep them that way.** A harness that reaches
for a real `qm` or `pvesm` stops being runnable in CI, which is the whole
point of having them.

Two rules for anyone adding cases:

* **Fixtures are synthetic.** This repository is public. Chasing a failure on
  the hypervisor, it is tempting to paste in a real `pvesm list` dump or a live
  runner id; do not. Those are host state, and invented values exercise the
  code exactly as well.
* **Test the refusals, not just the happy path.** Most of this script's
  functions delete storage or deregister runners, so the cases that matter are
  the ones where it must decline — a config view that will not answer, a VMID
  that owns a config again, a storage that will not list. The bugs worth
  catching here are the ones where a check said yes because it could not tell.

`shellcheck` is a required PR check (`stable / shellcheck`) over everything in
`tools/ci`, so a footgun it names cannot reach `main` unnoticed. That gate
exists because several of this script's worst defects were exactly that kind:
a pipeline status inverted through `!`, an assignment in a `local` reading an
outer variable of the same name.

## What This Is

A self-hosted, ephemeral GitHub Actions runner pool on a Proxmox VE host on
the operator's LAN, plus a LAN `bazel-remote` cache served by the operator's
NAS over the same 25 GbE switch fabric. Measured
against the GitHub-hosted `bazel / ubuntu-latest` leg (which runs 4-core
runners and re-fetches its remote cache over the operator's WAN link every
run), the pool's 6-vCPU clones complete the same three Bazel steps in
roughly 16 seconds on an unchanged tree with a warm LAN cache — all with zero
WAN traffic after the first population. A fully cold cache measured about
6.5 minutes, but that number belongs to the **16-vCPU** sizing and has not
been re-measured since; it is compute-bound, so expect it to be materially
worse at 6 vCPU. (The 16 s warm figure dates from that same 16-vCPU sizing —
which dropped to 12 and then 6 as slots were added — but warm runs are
cache-lookup-bound rather than compute-bound, which is why it still holds.)

Components:

* **Linux template VM** (`runner-template`): Ubuntu 24.04 provisioned exactly
  like `bazel.yml`'s Linux leg (lld, pinned bazelisk, the patched OmniSim fork,
  Pebble, QHYCCD SDK, ZWO SDK blobs — same pins, same SHA checks), plus the
  GitHub Actions runner and a `gha-runner` systemd unit that waits for a JIT
  config file, runs **exactly one job**, and powers the VM off. The
  template's machine-id and cloud-init state are wiped so every clone boots
  with a fresh identity.
* **Windows template VM** (`runner-template-win`): Windows Server 2025 — the
  same build as GitHub's `windows-latest` image — provisioned like
  `bazel.yml`'s Windows leg. Everything installs **machine-wide** under
  `C:\ci` and is located by MACHINE environment variables
  (`OMNISIM_PATH`, `PEBBLE_PATH`, `PEBBLE_CHALLTESTSRV_PATH`,
  `QHYCCD_SDK_DIR`, `ZWO_SDK_LIB_DIR`, `LIBCLANG_PATH`, `BAZELISK_HOME`,
  `CARGO_HOME`, `RUSTUP_HOME`, `RP_LAN_CACHE_URL`), which is the Windows
  analogue of the Linux runner's `.env`. The one exception is the runner
  itself, which lives at `C:\actions-runner` (mirroring the Linux
  `/home/ci/actions-runner`) — that is where the orchestrator injects
  `.jitconfig`, so a template rebuild must keep the path. A `gha-runner`
  scheduled task plays the systemd unit's role, and **it must run in an
  interactive desktop session, not as SYSTEM** — see below. Three
  Windows-only requirements: `BAZEL_SH` must point at Git's `bash.exe` or
  Bazel reports "No suitable shell toolchain found"; `QHYCCD_SDK_DIR` only
  reaches build actions because `.bazelrc` forwards it (a machine-wide SDK is
  invisible to the `GITHUB_WORKSPACE` fallback hosted runners rely on); and
  **PowerShell 7 (`pwsh`) must be installed**, because a stock Windows Server
  ships only Windows PowerShell 5.1 and every `shell: pwsh` step then has
  nothing to run — the failure that broke this venue's first real job.

  The general rule behind that last one: **the template must supply whatever
  GitHub's hosted image supplies and the workflows assume.** Hosted images
  carry a large pre-installed inventory that workflow YAML consumes without
  ever naming it as a dependency, so a gap is invisible until a job trips
  over it. `proxmox-runner-test.yml`'s Windows job asserts the ones known to
  matter before it builds — extend it whenever a workflow starts depending on
  something new from the image, and dispatch it after any template rebuild.
* **The Windows runner needs an interactive desktop session.** The
  `gha-runner` task runs as `Administrator` with `LogonType=Interactive` and
  an **AtLogOn** trigger, and the template has autologon enabled
  (`AutoAdminLogon`/`DefaultUserName`/`DefaultPassword` under
  `HKLM\...\Winlogon`) so that session exists from boot. Running it as
  SYSTEM instead puts the job in session 0, which has no desktop — and
  OmniSim builds a **system tray icon** at startup, so it throws
  `System.InvalidOperationException: TryCreate failed` and dies with exit
  code `0xe0434352`. Every BDD suite then fails or times out while `bazel
  build` stays perfectly green, which is exactly how this stayed hidden until
  the venue's first real job. GitHub's own hosted Windows images use the same
  autologon arrangement.

  **The credential tradeoff is deliberate and bounded.** Autologon stores the
  local administrator password in the registry in cleartext, which reads at
  first glance like a breach of ADR-020 layer 4 ("no standing credentials on
  the runner"). That layer is about credentials which unlock something
  *outside* the VM — a GitHub token, a cache write key. This one unlocks only
  the ephemeral clone itself, on which the job already runs elevated, so it
  grants a malicious job nothing it does not already have. What keeps the
  blast radius at one VM: the clones are VLAN-fenced from the rest of the
  network, the Linux template does not share the password, and — now that two
  Windows slots share this credential — each clone's NIC drops all inbound
  traffic (the per-clone firewall policy written by the orchestrator; see the
  storage/isolation notes below), so a compromised clone cannot reach a peer's
  SMB/RDP/WinRM even knowing the shared password. Measured directly: clone-to-
  clone TCP is silently dropped on every port — because the policy is
  `policy_in: DROP`, connections hang and time out rather than being refused
  with a RST/ICMP reject, so validate isolation by expecting a timeout, not a
  "connection refused". Only ICMP echo passes, which carries no credential and
  no lateral-movement capability. Adding a *third* concurrent
  Windows slot needs no new review — the isolation is per-clone, not
  pair-specific — but re-confirm the host has the RAM and that cipool still
  scales at the new concurrency (fio, per docs below) before doing so.
* **Pool orchestrator** (`tools/ci/rp-runner-pool.sh`): runs on the Proxmox
  host; keeps one warm linked clone per **pool slot** registered just-in-time
  and destroys it after its single job. Slots are declared in the script's
  `SLOTS` array (name, template VMID, clone VMID, guest OS, labels) and each
  runs its clone/register/wait/destroy loop concurrently. Slots sharing a
  label set are interchangeable — that is how the Linux slots keep
  `bazel.yml` and `bazel-coverage.yml`, which fire on the same PR event, from
  queueing behind each other; the third Linux slot (added when the cache
  moved off-host and freed its RAM and cipool I/O) absorbs a second PR
  event's Linux legs landing while the first is still running. Every slot holds one powered-on clone, so host
  memory must cover their sum. See the script header for deployment.
* **LAN build cache**: a `bazel-remote` Docker app (pinned image) on the
  operator's NAS, its data on the NAS's SSD pool — anonymous reads,
  credential-gated writes (same public-read / token-write model as the cloud
  R2 cache; the same htpasswd file moved with the data, so the GitHub secret
  did not change). The NAS holds a VLAN interface **on the runner VLAN** over
  its 2×25G LACP bond, and the runner bridge on the Proxmox host uplinks
  through a 25G port, so clone↔cache traffic is switched L2 at 25 GbE and
  never crosses the inter-VLAN gateway. Only the cache's HTTP port is
  published on that interface — the NAS's management UI, SSH, and file shares
  are bound elsewhere and are not reachable from the runner VLAN. Jobs
  receive the endpoint from the runner's `.env` (`RP_LAN_CACHE_URL`), never
  from workflow files, and mask it before use so it cannot appear in public
  logs; changing that URL is a template rebuild (both OSes), per the
  procedure below. The **write** credential deliberately does not exist on
  the runner VM: it is a GitHub Actions secret
  (`BAZEL_LAN_CACHE_WRITE_AUTH`) that bazel.yml attaches only on
  push-to-main events, mirroring the cloud cache's poisoning defense
  (ADR-020 layer 4). The cache formerly ran in a container on the Proxmox
  host; it moved to the NAS when the 25G fabric arrived (2026-08).

* **Storage layout: clone disks belong on `cipool`, never the root
  mirror.** `rpool` is a mirror of two 500 GB QLC drives; `cipool` is a
  single 4 TB NVMe with `compression=lz4`. Two reasons this split is
  load-bearing, both measured:

  * **The root mirror collapses under concurrency.** fio, mixed 70/30 16k,
    one job per simulated slot: rpool goes 2,595 → 3,336 → **3,259** IOPS at
    1/2/3 jobs — it saturates between one and two and *declines* at three,
    with p99.9 latency hitting 3.5s. `cipool` goes 6,733 → 9,846 → **11,258**
    and is still scaling. Sustained sequential write is 119 MiB/s on rpool
    (in 510 MiB/s bursts separated by 2–5s stalls, which is QLC past its SLC
    cache) against 2,180 MiB/s on cipool.
  * **Mirroring disposable data doubles writes for nothing.** A clone's disk
    is destroyed with the clone; if it were lost mid-job the job simply
    reruns. `cipool` is deliberately non-redundant.

  The cache's 230 GiB ceiling lives on the NAS's SSD pool now (the cloud R2
  cache reaches ~150 GB within its 7-day retention window, so the LAN cache
  needs the same order of headroom); neither host pool carries it any more.

  Clones inherit their template's storage, so moving the pool to `cipool`
  means rebuilding the templates there (`qm clone --full --storage cipool`,
  then `qm template`), not moving live clones.

* **Clone zvols run `sync=disabled`; templates keep the default.** The
  orchestrator sets it on every ZFS-backed volume of a clone right after
  `qm clone`, before first boot (`relax_clone_sync` in the script; the
  journal carries one `sync=disabled on N zvol(s)` line per clone, and names
  any clone it could not relax). It only ever touches datasets named
  `vm-<vmid>-*` — the clone's own — so a config line that merely looks like
  a volid cannot make it relax a template or another VM's disk. Root cause it addresses (#876): every flush
  a guest issues (ext4/NTFS journal commits, `fsync` from Bazel, the runner,
  the services under test writing configs, FITS frames and OmniSim profiles)
  became a synchronous ZFS write on the single cipool NVMe, queued behind
  ZFS's 10-slot sync-write class. Measured on the host with several busy
  slots: `zpool iostat -w cipool` accumulated tens of thousands of sync
  writes with 8–34 s queue wait while the device itself answered in
  milliseconds; per-VM cgroup PSI showed every clone I/O-stalled 5–7 % of the
  time; and the failing runs' logs showed all concurrently running suites in
  a guest going silent for ~20 s at once while timers kept firing. That is a
  frozen guest filesystem, and any wall-clock-bounded BDD step whose work
  touches disk times out inside it — the sky-survey `PUT /connected`
  (answered only after a probe write), an OmniSim site PUT (persists its
  profile), a 10 s wait for a service to materialise its config. Coverage
  runs were hit most because instrumented suites write the most and run
  longest, but the freeze is guest-wide. With `sync=disabled` a guest's
  flush completes immediately and the data still lands with the next
  transaction group; the cost — a host crash can lose a clone's last few
  seconds of writes — is nothing for a VM that is destroyed after one job.
  It is deliberately per clone, not `zfs set sync=disabled cipool`: the
  templates (`base-920-disk-0`, `base-910-disk-{0,1}`) stay durable through
  a rebuild, and nothing else lives on cipool. If a clone ever shows the
  freeze signature again, check the journal line first: a `pvesm path`
  change or a storage rename would silently stop the relaxation and the
  script only reports it, it does not refuse to boot.

* **ZFS ARC is capped well below what this host can afford.** With a 1 GiB
  cap the demand-data hit rate sits near 73%, so a quarter of data reads go
  to the platter unnecessarily. Right-sizing the slots frees RAM that is
  better spent here than on slot allocation nobody touches.
* **Per-clone network isolation.** Both templates carry `firewall=1` on their
  NIC, and `slot_loop` writes `/etc/pve/firewall/<vmid>.fw` for each clone with
  `policy_in: DROP` / `policy_out: ACCEPT` / `dhcp: 1` (removed in
  `destroy_clone`). A pool clone only ever talks to GitHub (off-subnet,
  through the gateway) and the LAN cache (the NAS's runner-VLAN interface,
  on-subnet at L2) — and never to a peer, so dropping all inbound costs
  nothing and blocks clone-to-clone TCP entirely.
  This is what makes two Windows slots sharing one local-admin password safe
  (see the autologon note in the security model). The host firewall stays
  unconfigured, so host SSH is never affected; only the guest NICs are filtered.
  Known residual: Proxmox permits ICMP echo regardless of the DROP policy —
  clones can ping each other but nothing more, which carries no
  lateral-movement risk.

## Security Model — DO NOT WEAKEN

This repo is public, and on `pull_request` events Actions executes the
PR's copy of the workflow YAML — so self-hosted runners and fork PRs are a
dangerous combination. The rule bifurcates by runner kind
([ADR-020](../decisions/020-ephemeral-self-hosted-runners-for-pr-checks.md)):

* **Persistent** self-hosted runners (the Raspberry Pi nightly runner)
  keep the binary rule: `workflow_dispatch:`/`schedule:` triggers only,
  never `pull_request` or `push`. Non-negotiable.
* **This ephemeral pool** may serve `push` and **same-repo**
  `pull_request` jobs under ADR-020's six-layer contract: a fork-excluding
  `runs-on` expression (every falsy branch lands on GitHub-hosted), the
  fork-PR approval checkpoint, JIT single-use VMs, no credentials on the
  runner, VLAN fencing, and the per-OS kill-switch variables
  (`RP_POOL_LINUX`, `RP_POOL_WINDOWS`).
  bazel.yml's Linux and Windows legs are the implementation. **Approving a fork PR's
  workflow runs is the human layer: review the workflow-file diff first —
  a fork can only reach this pool by editing `runs-on`.**
* Runners are **JIT-registered and single-use**: the config injected into a
  clone registers one runner for one job; a compromised job cannot mint
  further registrations. The orchestrator deletes the runner's org
  registration when it tears the clone down, so the GitHub-side entry does not
  outlive the clone — done host-side rather than trusting the guest's exit,
  because a Windows clone's forced power-off (and a wedge reclaim) would
  otherwise leave the entry lingering `offline` and the org's single runner
  list would grow without bound.
* The GitHub PAT (fine-grained, resource owner: the `rusty-photon`
  organization, sole permission "Self-hosted runners: Read and write") lives
  root-only on the hypervisor at `/etc/rp-runner/github-token`. It is never
  present inside any VM. Runners register at **org level** precisely because
  that permission exists only there — repo-level registration would require
  the far broader Administration permission (settings, deletion, teams).
  Free-plan orgs have only the default runner group, so org runners are
  usable by every repo in the org; the org is kept essentially
  single-project for that reason.
* Every job runs on a **fresh linked clone**; the clone powers off and is
  destroyed after its job. The only state shared between jobs is the build
  cache, whose writes are credential-gated.
* The runner VMs live on a dedicated VLAN whose router firewall allows
  exactly two things off-VLAN: the WAN and DNS. Everything else on RFC1918
  is dropped — verified by probing from inside a clone. The LAN cache no
  longer needs a router rule: the NAS serves it from an interface **on** the
  runner VLAN, reached at L2. What bounds that exposure is the NAS's own
  binding discipline — only the cache's HTTP port is published on the
  runner-VLAN interface; the NAS UI, SSH, and shares are bound to other
  networks only. Anonymous reads are by design; writes need the credential
  that exists only as a GitHub secret. Pool control runs over the QEMU
  guest agent (no network path), so the fencing cannot break pool
  mechanics.
* The repo-level "require approval for all outside collaborators" fork-PR
  policy must stay enabled — approval is the checkpoint for a fork PR that
  edits workflow YAML (ADR-020 layer 2).

## Operational Notes

* **Org runner groups ship with "Allow public repositories" disabled.** A
  freshly registered org runner then sits Idle while jobs from this (public)
  repo stay queued forever — no error anywhere. Either check the box on the
  Default group under the org's Actions → Runner groups settings, or use
  the pool's own token against the GitHub REST API (find the group id via
  `GET` on the same path):

  ```sh
  curl -X PATCH \
    -H "Authorization: Bearer $(cat /etc/rp-runner/github-token)" \
    -H "Accept: application/vnd.github+json" \
    https://api.github.com/orgs/<org>/actions/runner-groups/<group-id> \
    -d '{"allows_public_repositories": true}'
  ```

  This is part of the one-time setup contract.
* **Kill switches, one per OS:** routing of bazel.yml's Linux leg is gated
  on the repo Actions variable `RP_POOL_LINUX` being `on`, and its Windows
  leg on `RP_POOL_WINDOWS`. They are separate because the venues fail
  independently — a wedged Windows slot or a stale Windows template should
  not cost Linux its speed. If the pool host is down, required checks sit
  queued with no error anywhere (a queued self-hosted job is cancelled only
  after 24 hours — see
  [GitHub Actions limits](https://docs.github.com/en/actions/reference/limits))
  — flip the switch for whichever OS is affected and
  re-run; that OS routes back to GitHub-hosted runners with no commit
  needed. Match the variable to the leg that is stuck:

  ```sh
  gh variable set RP_POOL_LINUX  --body off   # bazel / ubuntu-latest (and, after R5, bazel coverage)
  gh variable set RP_POOL_WINDOWS --body off  # bazel / windows-latest
  ```

  A whole-pool evacuation — the host is down, rather than one slot — means
  running both.
* **Pins live in three places:** the hosted install steps in bazel.yml and
  *both* pool templates (Linux and Windows) carry the same toolchain pins
  (bazelisk, OmniSim, Pebble, camera SDKs). Bumping a pin in the workflow
  requires rebuilding both templates (procedure below) — the pool otherwise
  keeps running the old pin silently, and a pin bumped on only one template
  makes the two OS legs disagree about what they tested.
* The orchestrator logs to the journal of its systemd unit
  (`rp-runner-pool.service`) on the Proxmox host.
* **Leaked-volume defenses and their journal signatures.** `qm destroy` on a
  VM whose backing storage is not active removes the config but leaves the
  volumes; a leftover cloudinit volume then wedges its slot on `dataset
  already exists` at every clone (cloudinit volumes have a fixed name; disk
  volumes take the next free index, so they leak silently instead, pinning
  the template's base snapshot until a template retirement fails with
  "dependent clones"). Healthy runners elsewhere in the pool are not
  evidence the pool is intact. The orchestrator defends in layers, each with
  a named signature:

  - `deferring destroy (runner id <id>): storage '<name>' is not active` —
    the teardown is waiting for the storage, retrying with backoff (30 s
    doubling to 5 min). The probe itself attempts activation, so an
    importable pool comes back on its own; if this line repeats since boot,
    run `pvesm list <storage>` by hand, fix what it reports (usually the
    import), and the next cycle heals. Variants of the reason: `probe timed
    out after 30s` (activation is hanging — look at the pool's vdevs), `the
    VM config is unreadable` / `no storages readable from
    /etc/pve/storage.cfg` (pve-cluster is down).
  - `destroy left volume <volid> behind; freeing it now` — emitted *before*
    the attempt, so the volume is named even if the sweep never finishes. One
    of the outcome lines below normally follows within a minute or two. **A
    `freeing it now` with no outcome line after it is itself a signature**:
    this pass did not record what happened — almost always the service being
    restarted while it was retrying. Read it as *unrecorded*, not as *still
    there*; the free can have succeeded with the process dying before the
    outcome line was written. Re-list the storage before acting on it.

    Whether the slot then recovers on its own depends on which volume it is,
    and that is worth settling before assuming it healed:

    - A leaked **cloudinit** volume self-heals. It is allocated under a fixed
      name, so the next clone of that VMID collides with it and fails — and
      that failure is exactly what runs the orphan sweep.
    - A leaked **disk** volume does not. `qm clone` takes the next free index,
      so it collides with nothing: the clone *succeeds*, the sweep never runs,
      and the volume stays — pinning the template's base snapshot, and costing
      a template roll its space back — until someone frees it. Nothing in the
      pool will mention it again.

    The second case is what this line is really for. It names the volume at
    the only moment anything does, so a `freeing it now` whose outcome never
    landed is the one thing that says where to look.
  - `destroy left volume <volid> behind (qm said: ...); freed it, confirmed
    gone from the storage` — a destroy leaked anyway (e.g. a busy dataset on
    an imported pool, often a clone whose stop failed) and the orchestrator
    freed the orphan itself. Self-healed; recurrence is the signal worth
    chasing. The wording is literal: the volume's absence is re-read from the
    storage, because `pvesm free` exits 0 even when the `imgdel` task it
    starts loses the storage lock and the volume survives — so its exit
    status cannot decide this, and only a listing can.
  - `... and it is still listed after <n> attempts; the recovery runbook
    applies` — the free was retried `FREE_ATTEMPTS` times, spaced by
    `FREE_RETRY_SLEEP`, and the storage was read each time and still lists
    the volume. This is a genuinely stuck volume: the manual runbook below is
    needed.
  - `... and storage '<name>' would not list, so it could not be confirmed
    gone after <n> attempts; the recovery runbook applies` — distinct from
    the line above, and the distinction is the point. Nothing here says the
    volume is still there; it says the question could not be answered,
    because the listing failed on the last attempt. The immediate problem is
    the storage, not the volume — fix that first (`pvesm list <storage>` by
    hand). Whether the slot then recovers on its own depends on which volume
    it is, exactly as above: a **cloudinit** orphan wedges the next clone, and
    that failure is what runs the sweep, so it clears once the storage answers
    again; a **disk** orphan wedges nothing, so no clone ever fails, no sweep
    ever runs, and it stays until someone frees it. Fixing the storage is step
    one either way — it just is not the last step for a disk volume.
  - `... and the volume match failed on a storage that listed fine ...` — the
    rarer sibling of the line above, and it means the opposite about where to
    look. The storage answered; deciding whether the volume was among what it
    returned is what failed. Do not go debugging the storage: something is
    wrong on the host itself.
  - `volume <volid> survives a VM that no longer exists; freeing it so <vmid>
    can clone again` — the orphan sweep, which runs on a failed clone and is
    the slot's own way out of a `dataset already exists` wedge. It is followed
    by one of the same six outcomes as the teardown sweep. This is the line
    that turns most of the manual recovery below into something that happens
    on its own.
  - `not sweeping orphaned volumes for <vmid>: ...` — the sweep declined,
    and **that is the safe direction, not a fault**. Two reasons: `no storages
    readable from /etc/pve/storage.cfg` means the storage list could not be
    trusted (pmxcfs down, or a read that failed part-way), and with the config
    filesystem unavailable an empty `qemu-server` directory would make every
    volume on the host look like an orphan; `it still has a VM config` means
    the VM exists, so its teardown belongs to the reconcile. Fix pmxcfs in the
    first case and the sweep resumes by itself.
  - `could not list storage '<storage>' while sweeping orphaned volumes for
    <vmid>` — the sweep ran but one storage would not answer, so an orphan may
    still be sitting on it. Paired with a slot that keeps failing to clone,
    this is the line that says *which* storage to look at.
  - `... the config filesystem would not answer ...` / `... the config
    filesystem stopped answering ...` — the check that establishes whether a
    VMID still owns a VM could not complete. Either half of it can be the
    cause — `storage.cfg` unreadable (the pmxcfs liveness signal) or
    `/etc/pve/qemu-server` not enumerable — and the wording covers both
    deliberately, because the remedy is the same and naming only one would
    send you to the wrong component half the time. The point either way: an
    absent config file proves nothing when the thing that would have shown it
    is not answering, and "not there" versus "could not look" is exactly the
    distinction that licenses deleting a volume. Same remedy as the pmxcfs
    line above.
  - `... a VM config for <vmid> appeared while it was being freed ...` — the
    fifth outcome: nothing was established, and nothing more will be
    attempted. Freeing retries over a couple of minutes and ownership is
    rechecked before every attempt, so a VMID that acquires a config part-way
    through stops the run rather than risk deleting a live VM's volume.
    **If that VM is yours, move it off this VMID.** The stop protects the
    volume, not the VM: these VMIDs belong to the slots, and the slot reclaims
    its own on the next pass — it will destroy a VM it finds there with no
    live-job marker, which is the pool working as designed rather than a bug.
  - `... the config filesystem stopped answering ...` — the sixth, and it
    means the opposite about where to look. Nobody created anything; the
    config view itself became untrustworthy mid-run, so ownership could not be
    re-established. That is a pmxcfs problem, and the sweep resumes on its own
    once it is fixed.
  - `clone <vmid> has an injection marker with no usable runner id` — the
    marker survived but holds no id to watch (an in-place marker from an older
    build of this script, killed mid-write). The clone keeps its slot,
    deliberately: a marker means it may be working a job, and aborting real
    work is worse than the cost here. The cost is that it runs **with no wedge
    detection** until its job ends, so if that slot goes quiet it will not
    reclaim itself. Expected at most once per slot, on the first teardown
    after a pool upgrade, and never afterwards.
  - `could not clear the injection marker in <dir>` — the teardown ran but
    `/run` would not let the marker go (read-only or otherwise broken). Worth
    acting on rather than filing away: while that marker survives, the
    reconcile reads the clone as holding a live job and will not recover it,
    so the slot needs the marker removed by hand once `/run` is writable.
  - `stopping the orphan sweep for <vmid>: a VM config appeared while it was
    running` — rare and, again, the safe direction. A VM with that ID was
    created between the check that licensed the sweep and the free, so its
    volumes are no longer provably orphans and the sweep stops rather than
    guess. In this pool only a human creates a VM on a slot's VMID; if that
    was you, nothing is wrong.

  A `dataset already exists` wedge is no longer permanent by construction. The
  slot *attempts* to clear it: every failed clone sweeps volumes owned by its
  VMID, 30 seconds apart, which covers a leak from *outside* the gated teardown
  (a pre-gate deployment, or `qm clone`'s own rollback on a half-imported pool)
  and one left by a teardown that was interrupted before it finished.

  An attempt is not an outcome, and the journal is what tells them apart. The
  sweep can decline (below), and the free it runs can end still-listed or
  unconfirmed, or stop because the VMID has a config again or its config view
  went away — the same six verdicts as the teardown sweep. **A wedge that
  repeats is not evidence of self-healing in progress; it is the signal to read
  the outcome lines and go to the runbook.** What has changed is that most
  wedges now clear without anyone, not that every wedge does.

  What that sweep will not do is act while it cannot tell an orphan from a live
  volume, which is the whole reason it is safe to run automatically. It stops
  when pmxcfs is down and when the VM still has a config, and it reports either
  rather than guessing. So a wedge that *persists* now means one of three
  things, and the journal says which: the sweep declined (fix pmxcfs), the free
  could not be confirmed, or the volume genuinely will not go.

  Note what that does **not** cover: a leaked volume only reaches the sweep by
  wedging a clone, and a leaked *disk* volume does not wedge anything (see the
  `freeing it now` signature above). It leaks silently while the slot keeps
  working normally, so it will never appear as a stuck slot — the way to find
  one is to look, not to wait for a symptom. `zfs list -r -o name,origin
  <pool>` is the check — `-r` is required, since naming the pool without it
  reports the pool row alone and finds nothing. Every clone dataset names the
  snapshot it holds, so a base snapshot still carrying origins after its
  template was rolled is a leak, and step 3 below identifies which of them is
  an orphan.

  For those, note what the unconfirmed verdicts do and do not claim. Neither
  says the volume is still there — that is the point of their wording — only
  that nothing established it either way. Treat them as **unknown and needing a
  look**, not as confirmed orphans: check whether the volume actually exists
  before acting, since step 3 below is what tells an orphan from a volume
  already in use again.

  Manual recovery, now the exception rather than the routine:

  1. `systemctl stop rp-runner-pool` — never race the slot loops with a
     manual `zfs destroy`; they recreate the very names being cleaned.
  2. Inventory the leftovers: grep the journal for the signatures above and
     for `Could not remove disk` (qm's own warning; also preserved in the
     task logs under `/var/log/pve/tasks/`), and `zfs list -r <pool>` shows
     what exists.
  3. A volume is an orphan when no VM config references it — test per
     volume with a whole-word match,
     `grep -RFw "vm-<vmid>-disk-0" /etc/pve/qemu-server/` (likewise for
     `vm-<vmid>-cloudinit`; `-w` keeps `disk-1` from matching `disk-10`),
     not per VMID: after a partial recovery the VMID is back in use while
     the leaked volume still is not. The test is only meaningful while the
     config filesystem is up — an empty `/etc/pve/qemu-server/` means
     pve-cluster is down, not that everything is an orphan.
  4. `zfs destroy <pool>/vm-<vmid>-cloudinit` (and each orphaned disk
     volume) — fully qualified, one dataset at a time, never `-r`/`-R`,
     never a `base-*` dataset. Destroying a linked clone's dataset never
     touches its base.
  5. `systemctl start rp-runner-pool` — the slots heal on their own.

  The deployment requirements close the window rather than wait it out: the
  checked-in unit (`tools/ci/rp-runner-pool.service`) orders the service
  after `zfs.target`, and the pool must be registered in the ZFS import
  cachefile, which a pool PVE imported on demand is not — both are spelled
  out in the script header.
* An idle registered runner is a warm clone waiting for a dispatch; pickup
  is immediate. Replacement after a job takes under a minute (linked clone +
  boot + JIT registration).
* **A wedged clone is reclaimed; an idle one never is.** While a slot waits
  for its clone to power itself off, it polls that clone's runner through the
  GitHub API once a minute and destroys the clone once ten polls have reported
  the runner not connected with no `online` between them. Not ten *consecutive*
  polls: a poll that cannot reach the API counts for neither side, so an API
  outage stretches the grace instead of driving a reclaim. Ten minutes is
  therefore the floor, not the duration — the reclaim log prints the time
  actually elapsed, and a number well above ten is telling you the API was
  flaky rather than that a clone sat wedged that long. This is a check on the
  runner's liveness
  and deliberately *not* a cap on how long a clone may live: an idle runner
  reads `online` and so does a busy one, so neither is ever reclaimed, while a
  guest that wedges — BSOD, hung shutdown, a runner process that dies without
  powering off — would otherwise hold its slot forever. With a single Windows
  slot that is the whole venue lost, silently. Two rules keep it honest: an
  API that cannot be reached is never a verdict (the slot keeps waiting), and
  every reclaim is logged with its reason, so a recurring wedge shows up in
  the journal as a pattern instead of as capacity quietly going missing.
* **The guest one-job scripts live in the repo**, in `tools/ci/runner-guest/`:
  `one-job.sh` (Linux, installed as `/home/ci/run-one-job.sh` and run by the
  `gha-runner` systemd unit) and `one-job.ps1` (Windows, installed at
  `C:\actions-runner\one-job.ps1` and run by the `gha-runner` scheduled task).
  They are the source of truth — a template rebuild copies them in rather than
  editing the guest's copy in place. Both wait a bounded 30 minutes for the
  injected config and power the VM off if it never arrives; that is the
  guest's own backstop for the orchestrator being stopped, which is the one
  case the slot health check above cannot cover.

  The Linux template additionally carries `rp-set-hostname` (installed as
  `/usr/local/sbin/rp-set-hostname`) and `rp-hostname.service` (installed as
  `/etc/systemd/system/rp-hostname.service` and enabled), which give each clone
  a unique hostname derived from its NIC MAC. **These live in
  `tools/ci/runner-guest/` too and a rebuild must copy them in exactly like the
  one-job scripts, then `systemctl enable rp-hostname.service`.** A rebuild
  that forgets them produces a template whose clones all share one name again,
  silently — which is how the duplicate arose in the first place. A hostname is a DHCP
  identity (option 12), so clones sharing one collide on a lease however large
  the address pool is. The unit is ordered ahead of `systemd-networkd`, and
  that ordering is the whole point: a unit running after it is too late,
  because the first DHCP request already carried the template's name and that
  is the request which takes the lease.
* **The Windows one-job loop empties the job account's `%TEMP%` at logon,
  before the runner starts.** A clone's user temp is whatever the template
  captured, and a template is warmed by running the workspace's tests inside
  it — so every Windows clone used to boot with the debris of that run:
  hundreds of `rp-test-*` data directories and configs, and a handful of rp
  session registries still marked `active` (rp keeps an active registry
  across a stop on purpose; that is its restart recovery). Windows never
  empties a user temp on its own, and Bazel does not re-point `TMP`/`TEMP`
  for test actions, so a job's test process could be handed the same PID as
  one of those leftovers, mint the same registry name, and hand rp a
  thirteen-day-old live session to restore — one flake of exactly that shape
  is what surfaced it. The harness now mints its paths inside a per-process
  random-named scratch directory (`crates/bdd-infra`, `rp_harness/scratch.rs`),
  which closes the collision on every venue; the sweep is the pool-side
  half, and it is deliberately the whole directory rather than a name
  pattern: an inventory of a live clone found nothing in there but that
  debris plus Bazel's self-healing JNI/perf-data extractions, and emptying it
  is what Linux clones already get from Ubuntu's tmpfiles `D /tmp` rule at
  every boot. Locked entries are skipped silently. `one-job.log` in the guest
  prints one `emptied <dir>: N entries before, M left` line per sweep. The
  Linux loop needs no counterpart while `/tmp` stays on the tmpfiles rule.
* **The whole Windows action cache hangs off `GITHUB_WORKSPACE`.**
  `.bazelrc` sets `build:windows --action_env=GITHUB_WORKSPACE`, so that path
  string is baked into every Windows action key. Consequences worth knowing
  before they surprise someone:
  * Change the runner's work-directory layout — a different `_work` root, a
    renamed runner — and **every Windows job goes cold at once**, with no
    error to explain it.
  * Reproducing a Windows build by hand outside the Actions runner gets a
    100% cache miss unless `GITHUB_WORKSPACE` is exported to exactly the
    path CI used. That is a useful property when deliberately measuring an
    uncached build, and a baffling one when not.

  Linux does **not** carry this variable in its action env, so the two
  venues behave differently here.
* To update the runner toolchain (new SDK pin, new runner release), boot a
  fresh clone of the template, apply the change, copy in the guest one-job
  script from `tools/ci/runner-guest/` if it has changed, wipe
  `/etc/machine-id`, run `cloud-init clean`, power off, and convert to the new
  template — then roll the template VMID forward in `rp-runner-pool.sh`. Validate the new template
  by dispatching `proxmox-runner-test.yml` **before** rolling the VMID
  forward, and validate with the whole job: `bazel build` alone never spawns
  OmniSim, so it cannot see a template that can build but cannot test.
  * **Rolling the VMID forward does not move the clones that are already
    running, and the pool's own startup line will not tell you otherwise.**
    The reconcile matches a clone by VMID and injection marker, never by
    template, and it leaves a *marked* clone alone deliberately so that
    restarting the service does not abort a job already under way. The marker
    is written only after injection succeeds, so a restart landing in the
    window between the two finds a marker-less clone and destroys it — the one
    case where a restart can take out a clone that was about to run. The
    ordering is deliberate, and buys immediate cleanup: a marker-less clone is
    reclaimed by the reconcile on the spot, whereas a marker written first
    would leave an unconfigured clone looking like an in-flight job, ended
    only by the health check's strike path or the guest's own no-config
    timeout. It is not free, though. `destroy_clone` takes the runner id from
    the marker, so a clone destroyed inside that window is torn down without
    its freshly minted registration being deleted — the id was minted, it
    simply had nowhere to be recorded yet — leaving an orphaned runner entry
    on GitHub to remove by hand. So after `systemctl restart rp-runner-pool`,
    a marked clone that is still running keeps serving from the *old*
    template until it has carried another job through to completion. Usually
    there is no job to wait on yet: a registered clone with nothing assigned
    is not stalled but *warm*, which is the point of the pool, so the slot
    sits on the old template until CI sends it work and that work finishes.
    The roll completes lazily, at whatever rate the pool happens to be fed.
    The `starting slot (template N, clone M, ...)` line reports the configured
    template, not the one the running clone was built from.

    Lazy is the normal path rather than a guarantee, and three cases depart
    from it. A marked clone that is already **stopped** when the service
    restarts breaks straight out of the wait loop and is re-cloned at once,
    with no further job. A clone whose runner goes **offline** while its VM
    stays up is reclaimed by the health check after its strike count — that
    runner may have died mid-job, so such a clone rolls without *completing*
    a job, which is not the same as never having started one. A **paused or
    suspended** clone is skipped by the probe on purpose and does not roll at
    all.

    That reclaim needs a runner id to probe. An **empty** marker — one left
    by an older version of the script — degrades to no health check for that
    clone by design, so an offline runner behind an empty marker is never
    reclaimed and the slot waits on `stopped` with no liveness bound at all.
    A slot that has sat on the old template far longer than its peers is
    worth checking for this before anything else.

    This is what makes "validate before rolling forward" easy to get wrong in
    the other direction. A `proxmox-runner-test.yml` dispatched straight after
    the restart can land on a clone of the template being replaced and come
    back green without having touched the new one — a pass that proves
    nothing, and reads exactly like a pass that proves everything. Confirm
    lineage first, and dispatch only once every slot shows the new base:

    ```sh
    zfs list -H -o name,origin -t volume | grep -E 'vm-9[0-9]{3}-disk-'
    ```

    Windows slots carry two disks and so contribute a line each. `qm clone`
    takes every disk of a clone from the one template, so a slot's disks do
    not disagree in practice; listing them all is about the check showing the
    whole slot rather than a chosen disk of it.

    To force a slot over rather than wait for CI, `qm stop <clone vmid>` —
    but **confirm the slot is idle first, because this is destructive if it
    is not.** `reason=finished` is set before the wait loop and a `stopped`
    status breaks straight out of it, so the loop cannot distinguish a job
    that ended from a clone an operator powered off underneath one: it
    reports the kill as a completion and destroys the clone, taking the
    in-flight job with it. Prefer waiting for the current job unless you mean
    to cancel it. GitHub is what settles idle-versus-busy, and the runner id
    to ask about is in the slot's own marker:

    ```bash
    # Set this to the clone you are about to stop. A busy-check that names a
    # different slot than the qm stop below is worse than no check at all.
    vmid=9100
    # Only a complete, newline-terminated numeric record counts. `read` fails
    # on a partial line — what an in-place marker from an older build looks
    # like after an interrupted write — and a PREFIX of a runner id is another
    # runner's id, so `cat` here would ask about a stranger immediately before
    # the destructive command below. The daemon applies the same rule.
    rid=""
    IFS= read -r rid 2>/dev/null <"/run/rp-runner-pool/$vmid.injected" || rid=""
    case "$rid" in '' | *[!0-9]*) rid="" ;; esac
    if [ -z "$rid" ]; then
      echo "no usable runner id for $vmid (missing, empty, or a marker that" \
           "never finished writing): this check cannot answer, so do not" \
           "treat silence as idle"
    else
      curl -sS --connect-timeout 5 --max-time 15 \
           -H @<(printf 'Authorization: Bearer %s' "$(cat /etc/rp-runner/github-token)") \
           -H "Accept: application/vnd.github+json" \
           "https://api.github.com/orgs/rusty-photon/actions/runners/$rid" |
        python3 -c 'import json,sys; print(json.load(sys.stdin)["busy"])'
    fi
    ```

    The guard is not decoration, and it fails two different ways without it.
    An empty id substitutes away to nothing, the URL collapses to the *list*
    endpoint, and the reply describes the whole pool rather than this slot — an
    answer to a question nobody asked, immediately before a destructive
    command. A *partial* id is worse, because it looks like an answer: the URL
    is well-formed, GitHub replies about whatever runner that number belongs
    to, and a confident `False` sends you on to `qm stop` a clone that is
    working a job. The timeouts
    mirror the daemon's own call, so a black-holed `api.github.com` cannot
    leave you waiting indefinitely with a `qm stop` queued up behind it.

    Run it under bash: `-H @<(...)` is process substitution, and it is there
    for a reason. Passing the credential as `-H "Authorization: Bearer
    $TOKEN"` would place it in curl's argv, where anyone on the host can read
    it out of `ps` for as long as the request runs. The daemon reads its own
    token this way for the same reason; the runbook should not teach the
    weaker form.

    It reads the pool's token because that is the credential on the host
    carrying the runner-administration scope; `gh` would authenticate as
    whatever it is logged in as, which on the hypervisor is usually nothing.
    The script's own health check hits this same object but reads `status`,
    never `busy` — it is there to catch a wedged runner and would answer
    `online` for a slot mid-job, so it is not a busy-check to borrow.

    The check narrows the window; it does not close it. A job can be
    dispatched between the read and the stop, and the loop will read that
    poweroff as a completion just the same. If you want certainty rather than
    good odds, delete the runner registration first (`DELETE
    /orgs/<org>/actions/runners/<id>`) so GitHub stops routing work to it —
    `destroy_clone`'s own deregistration then returns 404, which it already
    treats as success. Note that its deregistration is best-effort either
    way: a non-2xx or an unreachable API is logged and the destroy proceeds
    regardless, so a registration can outlive the clone it belonged to.

    Do **not** delete the marker file to force a roll. The runner id lives in
    it and `destroy_clone` reads it back to deregister, so removing it first
    strands the registration with nothing left that knows its id.
  * **The `/etc/machine-id` wipe is not optional, and booting the template to
    verify it repopulates it.** systemd only regenerates a *unique* machine-id
    when the file is empty at boot; a non-empty one is kept. A Linux template
    captured with a populated machine-id hands every clone the same one, hence
    the same DHCP client identity and the same leased IP — two slots then
    collide on one address, with ARP flapping and intermittent return-traffic
    loss that presents as a flake, not a clean failure. So the wipe must be the
    **last** thing before `qm template`: if you boot the clone to check
    anything (bazel, the warm cache), re-wipe `/etc/machine-id` (and clear
    `/var/lib/dhcp/*.leases`) afterwards. Windows is immune to *this* one —
    its clones DHCP by MAC, which Proxmox regenerates per clone — but not to
    the hostname problem directly below, which bites both OSes.
  * **The hostname must be unique per clone, and a template captures exactly
    one.** Clients send their hostname as DHCP option 12, so the name is part
    of the identity a client presents, not merely a label — and every clone of
    a template claims the same one. On this pool that coincided with an
    address conflict that pool size could not explain: the conflicting address
    was nowhere near any static reservation, on a /16.

    **How firmly this is established:** duplicate hostnames were the only
    duplicated identity left after checking the others — MACs distinct across
    all slots, `machine-id` distinct on every Linux clone, no orphaned VM
    holding a lease, statics far from the conflicting address. That makes
    option 12 the leading suspect, *not* a proven mechanism; how a given
    router keys its bindings and its DNS is its own business and was never
    confirmed here. Confirm before trusting a mitigation that depends on it:
    the router's client list showing one entry with a flipping MAC, or two
    entries sharing an address, settles it. Treat the paragraph below as the
    remedy for a duplicate identity, which is worth removing regardless, and
    not as proof of how the router behaves. Two same-named Windows clones on one L2
    segment are also a NetBIOS name collision in principle, independent of
    DHCP — but that half was measured here and stayed benign (no NetBT/Tcpip
    name-conflict events); see the sysprep bullet in the Windows notes below.
    Do not treat NetBIOS as already broken on this pool.
    So a rebuild must take one of two remedies.

    **Rename per clone at first boot.** The name has to be set before
    `systemd-networkd` sends its first request, since that request is the one
    that takes the lease. Ordering alone does not achieve this: `Before=`
    controls *when* a unit runs relative to another, but does not cause it to
    run at all. A unit carrying only `Before=network-pre.target` can still be
    pulled in late — at `multi-user.target`, long after DHCP — with the
    ordering directive having had nothing to constrain. It needs all three —
    pulled into early boot (`WantedBy=sysinit.target`),
    `DefaultDependencies=no`, and
    `Before=network-pre.target systemd-networkd.service`. All three, or the
    unit runs — successfully, and too late.

    `DefaultDependencies=no` is not optional here, and it has a cost worth
    understanding. A service left with the default set gains
    `Requires=`/`After=sysinit.target` and `After=basic.target`; combined with
    a `Before=` on networkd that is an ordering cycle, which systemd resolves
    by dropping one of the constraints — so the unit runs at an arbitrary
    point rather than reliably early. `systemd.service(5)` scopes the opt-out
    to exactly this case: *"only services involved with early boot"*. The cost
    is that turning it off also drops the implicit ordering you would
    otherwise inherit, so add back whatever the script genuinely needs — for
    one that writes `/etc/hostname` and reads `/sys`, `After=local-fs.target`
    is enough.

    Set the name the primitive way: write `/etc/hostname` and set the live
    name with `hostname`, falling back to `/proc/sys/kernel/hostname`. Do not
    reach for `hostnamectl` here — it talks to `systemd-hostnamed` over dbus,
    neither of which is up this early, so the unit runs, reports success, and
    changes nothing before the first DHCP request. The live name is the part
    that matters: networkd composes its request from the running hostname, not
    from `/etc/hostname`.

    `WantedBy=` is an `[Install]` directive and does nothing until the unit is
    enabled: dropping the file into `/etc/systemd/system/` without
    `systemctl enable` leaves it inert, and the template then captures a guest
    that looks configured and still sends the template's name. Enable it
    before the capture, and confirm with `systemctl is-enabled`. Verify with `qm guest exec <vmid> -- /bin/hostname` across two live slots
    before rolling a template forward: identical output means the rename did
    not take.

    **Or stop sending the name** (`SendHostname=no` in the template's
    `.network` file), which drops option 12 from what the client presents and
    leaves the MAC and the client-id. Under this remedy the in-guest names stay
    identical *by design*, so the `hostname` check above does not apply —
    verify instead that the effective `.network` file disables it —
    `SendHostname=no` or `false`, not merely the key, which is equally present
    when the value is `yes`. It is a networkd setting rather than anything on
    a service unit.

    Configure and inspect are different places here, and conflating them
    silently loses the change. On these guests the `.network` file is
    *generated* — netplan renders it under `/run/systemd/network/` — so that
    file is where you **read** the effective result (take the path
    `networkctl status <iface>` reports rather than guessing), never where you
    edit. Editing it survives until the next boot and no further, which is
    long enough to verify, capture a template, and ship the collision anyway.
    `/etc/netplan/50-cloud-init.yaml` is no better: cloud-init rewrites it
    every boot. Persist the setting somewhere neither regenerates — a
    higher-numbered `/etc/netplan/*.yaml`, which netplan merges over the
    cloud-init one, or a drop-in under `/etc/systemd/network/`. Then read the
    `/run` file to confirm it took — `grep -Ei 'SendHostname=(no|false)'`.
    Accept either spelling, because systemd treats them identically and a
    generated file need not spell it the way you wrote it, but do not drop the
    value from the match: the key alone is also present when the setting reads
    `yes`, so grepping for `SendHostname=` passes a config that still sends
    the name. Then confirm the router stops showing a shared name.

    This remedy has not been exercised on this pool — the rename was taken
    instead — so treat the mechanics above as the documented behaviour of the
    tools rather than as something verified here.

    Note what neither remedy does: the client-id is derived from `machine-id`,
    so both remove one duplicated identity and leave the other untouched. The
    `machine-id` wipe above stays mandatory either way — two independent
    failures that happen to share a symptom.
  * **Empty `/tmp` and `/var/tmp` before capture, and confirm it on disk.**
    systemd-tmpfiles ships `D /tmp`, so a clone empties `/tmp` on every boot
    regardless — which makes this a cost question, not a correctness one: a
    template captured with a populated `/tmp` makes every clone pay the I/O of
    clearing it during boot, for nothing. `/var/tmp` is not covered by that
    rule at all (its tmpfiles line is commented out), so whatever is left there
    persists into every clone for the life of the template. Clear both just
    before the machine-id wipe. The `systemd-private-*` entries belong to
    running services and are removed as those services stop, so they need no
    deleting by hand; what is worth checking is the result, and that is
    readable on disk once the VM is stopped, without booting it again:
    `mount -o ro,noload /dev/zvol/cipool/vm-<vmid>-disk-0-part1 /mnt/<dir>`
    (`ro,noload` skips journal replay, so the image is not modified). Do not
    measure a live clone instead — one mid-job has job working files in `/tmp`
    that say nothing about what the template carries.
  * **Check that an SDK installer has not chowned system directories to
    `ci`.** The QHY SDK install left `/etc`, `/usr`, `/usr/sbin`, `/usr/lib`,
    `/usr/share` and their udev and firmware subdirectories owned by `ci` at
    mode 775, and every Linux template through 921 inherited it — 920 among them,
    so the hazard is live until the roll to 926 lands. Directory
    write permission governs unlink and create regardless of who owns the
    files inside, so the unprivileged job account could replace binaries under
    `/usr/sbin`, libraries under `/usr/lib`, and udev rules that root executes
    — a local escalation to root inside the clone. All must be `root:root`.

    **`/usr/local` cannot be skipped wholesale**, tempting as it is to write
    it off as the SDK's own tree. `rp-set-hostname` is installed in
    `/usr/local/sbin` and systemd runs it **as root at early boot**. Its file
    and its directory were already root-owned — and that was not enough,
    because `/usr/local` itself was `ci`-writable, and write permission on the
    *parent* is what governs renaming `sbin` and substituting a directory of
    one's own. So `/usr/local`, `/usr/local/sbin` and `/usr/local/bin` must be
    `root:root` too. Only the SDK's own subtrees below them (`include`, `lib`,
    `testapp`, `udev`, `fx3load`, `doc`, `riffa_linux_driver`,
    `cmake_modules`) stay `ci`-owned, which is what keeps jobs writing there
    unaffected.

    Verify with two checks, because one alone leaves a gap.

    First assert the root-executed paths directly. These are the ones whose
    compromise hands a job root, so they are worth naming rather than
    inferring from a sweep:

    ```sh
    stat -c '%U:%G %a %n' \
        /usr/local /usr/local/sbin /usr/local/bin \
        /usr/local/sbin/rp-set-hostname
    ```

    The owner:group field — the first on each line — must read
    `root:root`. The command also prints the mode and path
    (`root:root 755 /usr/local`), so compare that field, not the whole
    line.

    Then sweep for anything else an installer left behind. Prune the SDK's
    own subtrees by name — **not** `/usr/local/*` as a wildcard, which is
    what would let `/usr/local/sbin` regress to `ci` without the checklist
    noticing, in flat contradiction of the paragraph above:

    ```sh
    find / -xdev \
        \( -path '/home/ci' \
           -o -path '/usr/local/include' -o -path '/usr/local/lib' \
           -o -path '/usr/local/testapp' -o -path '/usr/local/udev' \
           -o -path '/usr/local/fx3load' -o -path '/usr/local/doc' \
           -o -path '/usr/local/riffa_linux_driver' \
           -o -path '/usr/local/cmake_modules' \) -prune \
        -o -user ci -print
    ```

    which should print nothing. The prune list is deliberately the explicit
    set of subtrees that are allowed to be `ci`-owned: anything not named
    there stays under the check, `/usr/local`, `/usr/local/sbin` and
    `/usr/local/bin` included. Adding a new SDK to the template means adding
    its tree to that list, which is the point at which someone has to decide
    whether it really needs to be job-writable.

    Run both against the **template** before capture, never against a live
    clone. A clone mid-job has `ci`-owned runtime state the runner itself
    creates — `/usr/share/GitHubActionsService/...`, `/tmp/clr-debug-pipe-*` —
    which is legitimate there and says nothing about the image. Measured on a
    920-derived clone the sweep returned 137 paths; the same sweep on the
    template it was built from returns none.
  * **A Linux template rebuild must run a coverage warmup before capture, not
    just a build/test warmup** — specifically
    `bazel coverage --config=coverage //...`. That `--config=coverage` flag is
    load-bearing: it overrides the default stable channel (`.bazelrc` pins
    `channel=stable`) to the pinned *nightly* toolchain — a bare
    `bazel coverage //...` compiles on stable and fetches nothing new,
    defeating the warmup. The build/test toolchain never pulls that nightly,
    so a template warmed only with `bazel build`/`bazel test` still hands
    every coverage clone a cold nightly toolchain to fetch over WAN on its
    single job — measured at ~12 min cold versus ~2m47s once the toolchain is
    baked into the output base. The warmup must check out to the **exact**
    runner work path
    (`/home/ci/actions-runner/_work/rusty-photon/rusty-photon`): external
    repos — the toolchain among them — live under an output-base dir keyed by
    an md5 of the workspace path, so a warmup done at any other path populates
    a directory no clone will read. (Linux action keys are path-independent —
    they carry no `GITHUB_WORKSPACE`, unlike Windows — so the *build/test*
    actions transfer regardless of warmup path; only the external-repo fetch
    is path-sensitive, and that is what the nightly coverage toolchain is.)
    Run it as the `ci` user with the runner's own CI flags —
    `bazel coverage --config=ci --config=coverage --config=remote-cache //...`
    plus the LAN-cache override the pool jobs apply
    (`--remote_cache="$RP_LAN_CACHE_URL"` after `--config=remote-cache`, the
    URL sourced from the runner's `.env`, never the repo) — so the warmup
    exercises the same cache routing a real job does, then re-wipe
    `/etc/machine-id` as above.
* **Windows template rebuilds, things that will bite:**
  * **The template must provision the MSI packaging toolchain**, or the msi
    job (`msi.yml` / `release.yml` / the msi leg of `nightly-packages.yml`)
    cannot run on the pool even though `bazel build` is green. Three parts,
    all measured the hard way:
    * **.NET SDK** — the wix CLI is a dotnet global tool; `build-msi.ps1`
      fails its own `dotnet not found` precondition without it.
    * **`DOTNET_ROOT` set machine-wide** — not optional. `wix.exe` is a
      framework-dependent apphost and resolves the runtime through this
      variable, never through PATH, so `dotnet --version` can succeed while
      every global tool dies with "You must install .NET to run this
      application / .NET location: Not found".
    * **wix at the pinned version** (matching `$WixVersion` in
      `build-msi.ps1`) on a machine-wide tool path — `--tool-path`, not
      `--global`, so the job's user sees it.

    Provision alongside the other `C:\ci` toolchain, machine-scoped. `msi.yml`
    runs these scripts under `pwsh` (PowerShell 7); the scripts are kept pure
    ASCII so they also parse under Windows PowerShell 5.1, whose ANSI codepage
    would otherwise mangle any non-ASCII punctuation into parse errors — keep
    them that way.
  * **Do not `sysprep /generalize`.** It looks like the analogue of the Linux
    template's `machine-id` wipe, but its specialize pass runs on every clone
    boot and would add minutes to a pool whose whole value is fast pickup —
    and little it buys applies here: Proxmox gives each clone a fresh MAC, the
    guests are not domain-joined, and the runner's identity comes from the
    injected JIT config rather than the host name. **Duplicate computer names
    do occur** once more than one clone runs at a time (both come up as
    `RUNNER-TPL` on the same segment). That was measured benign at the
    *NetBIOS* layer — no NetBT/Tcpip name-conflict events, registration
    unaffected — and that part still holds. It is **not** benign at the DHCP
    layer: a duplicate hostname is a duplicate option-12 identity, the problem
    described in the **hostname bullet** under the Linux rebuild notes above —
    not the `machine-id` one next to it, which is a different failure with
    different remedies. An address conflict was observed on this pool while
    these names were duplicated. Read that bullet for how firmly the link is
    established — by elimination, not by confirming the router's behaviour —
    but take only the diagnosis from it, **not the remedies**: a systemd unit
    ordered before `network-pre.target` and `SendHostname=no` in a `.network`
    file are Linux-only, and there is nothing there for a Windows rebuild to
    apply.

    Windows has two options, neither yet taken. `Rename-Computer` gives a
    unique name but option 12 uses the *active* computer name, which does not
    change until restart — so it costs a reboot per clone, roughly 30–60s
    against a pool whose value is fast pickup. That is far cheaper than a
    sysprep specialize pass, so it is affordable if needed, but it is not
    free. The other is a **DHCP reservation per Windows MAC**, which costs
    nothing at runtime and sidesteps hostnames entirely; the catch is that
    `qm clone` regenerates the MAC every cycle, so the address has to be
    pinned in the slot's config first. Try the reservation before the rename.
    Neither has been done because Windows is the smaller duplicate group (two
    clones against Linux's three) and it is not established that Windows was
    ever implicated in the observed conflict — so this stays a watch item, to
    be revisited if a conflict recurs after the Linux fix. Skipping
    sysprep stays the right call for the reasons above. The
    shared-credential exposure the second slot introduced (#872) is contained
    by the per-clone inbound-DROP firewall described above, not by name
    uniqueness.
  * **A linked clone inherits the template's RTC** and boots badly out of
    date (~9 hours, in practice). That alone breaks TLS to GitHub. It also
    means an in-guest script must never use a wall-clock deadline: the first
    one-job loop computed an end time, Windows then corrected the clock past
    it, and every clone powered itself off before the orchestrator could
    inject. The loop resyncs time before touching the network and measures
    its wait with a monotonic timer.
  * **The warm-up's test debris is captured with the image.** Running the
    workspace's tests in the template to warm its caches leaves the job
    account's `%TEMP%` full of BDD scratch, live rp session registries among
    it, and every clone inherits that. The one-job loop's logon sweep (above)
    is what makes it harmless, so a rebuild must copy in the *current*
    `one-job.ps1` — an old copy without the sweep reintroduces the hazard on
    the very rebuild that just refreshed the debris. Emptying the directory
    by hand before capture is optional tidiness, not the fix.
* What lives where: **VMIDs are in the repo**, in `rp-runner-pool.sh`'s
  `SLOTS` array — they are local to one hypervisor, meaningless anywhere else,
  and the orchestrator needs them to do its job. What is deliberately absent
  is anything that identifies or unlocks infrastructure: **addresses**
  (this repo is public — see the LAN cache endpoint, which reaches jobs only
  via the runner's `.env` and is masked before use) and **credentials** (the
  PAT lives on the hypervisor at `/etc/rp-runner/github-token`, the cache
  write credential only as a GitHub Actions secret). A reader should be able
  to see exactly which VM does what, and nothing about where it is or how to
  reach it.

* **The host's memory is checked, and the check is worth keeping that way.**
  The hypervisor runs ordinary non-ECC DDR5 modules, but Intel In-Band ECC is
  enabled in firmware, so the kernel presents real EDAC controllers through
  `igen6_edac` running in SECDED — single-bit errors corrected and counted,
  double-bit errors detected. Confirm with:

  ```sh
  cat /sys/devices/system/edac/mc/mc*/dimm*/dimm_edac_mode   # expect SECDED
  cat /sys/devices/system/edac/mc/mc*/ce_count               # expect 0
  ```

  This matters more than a hypervisor's RAM usually does. A kernel oops from
  corrupted memory looks the same whether the corruption came from the DIMMs,
  from microcode, or from a kernel bug, and the pool's whole failure surface
  is downstream of the host staying up. IBECC is what separates "the DIMMs did
  it" from the alternatives — but only if someone reads the counters, which
  reset at every boot and so cannot answer the question after an unexplained
  reboot.

  `rp-edac-check.sh` closes that: a timer reads the counters, keeps a
  high-water mark across runs, and logs at `err` priority when they move.
  Steady counters say nothing. Install it alongside the pool service:

  ```sh
  install -m 755 rp-edac-check.sh /usr/local/sbin/
  install -m 644 rp-edac-check.service rp-edac-check.timer /etc/systemd/system/
  systemctl daemon-reload && systemctl enable --now rp-edac-check.timer
  ```

  Read it with `journalctl -t rp-edac-check`, or `journalctl -p err -b` along
  with everything else worth seeing. **Know its limit before relying on it:**
  the host has no notification target — postfix has no relayhost, so mail to
  root goes to a spool nobody opens — so this is a recorder, not a pager. It
  guarantees the evidence exists and is findable; it does not tell anyone.
  Wiring a real notification target is what would change that.

  Three things it reports, in descending order of how much they should worry
  you: uncorrectable errors (data was wrong — treat the host as unreliable);
  correctable errors climbing (SECDED is covering for a degrading DIMM, and
  this is the early warning that an uncorrectable error does not give you);
  and *no EDAC controllers at all*, which is not silence but its own alarm —
  it means IBECC is off and memory faults have stopped being counted. That
  last one is the reason to run this check even when the memory is healthy:
  **a BIOS update resets firmware defaults, and the ECC setting goes with
  them.** Anyone flashing this board should re-check `dimm_edac_mode`
  afterwards, and the check will say so on its next firing if they forget.

## Bootstrapping a Runner Manually (no orchestrator)

For one-off validation without the pool service: clone the template, start
it, mint a JIT config (`POST /orgs/<org>/actions/runners/generate-jitconfig`
with the labels above and the default runner group's id), and write it to
`/home/ci/actions-runner/.jitconfig` in the guest
(write to a temp file and `mv` — the service polls for the file). The clone
registers, waits for one job, runs it, and powers off.
