# Proxmox PR Routing Plan — real CI legs on the ephemeral runner pool

## Goal

Route real `pull_request`-triggered CI legs of this public repository to the
Proxmox ephemeral runner pool ([skill doc](../skills/proxmox-runner-pool.md)):
`bazel / ubuntu-latest`, `bazel coverage` (bazel-coverage.yml), and
`bazel / windows-latest` — the three required Bazel checks. msi.yml's
`build-verify` was measured and deliberately left on hosted (R4b below). Fork
PRs stay on GitHub-hosted
runners and every layer of the security contract in
[ADR-020](../decisions/020-ephemeral-self-hosted-runners-for-pr-checks.md)
holds. Measured baseline: the pool completes the Linux Bazel steps in ~16 s
on an unchanged tree with a warm LAN cache versus 4–10 minutes hosted.

This deliberately supersedes the blanket "dispatch/schedule triggers only"
rule for the **ephemeral pool only**. Persistent self-hosted runners (the
Raspberry Pi nightly runner) keep the old rule unchanged — see ADR-020 for
the full layered contract and its rationale.

## Implementation Status

| Phase | Description | Status |
|-------|-------------|--------|
| R1 | Isolation + credential hardening: runner VLAN, write credential removed from runner `.env` | Done |
| R2 | Route Linux: conditional `runs-on` in bazel.yml, LAN write secret gated on push, provisioning guards, kill switch | Done |
| R3 | Windows runner template + orchestrator pool slots (Windows slot, second Linux slot) | Done |
| R4 | Route Windows: `bazel / windows-latest` with the `RP_POOL_WINDOWS` kill switch | Done |
| R4b | Route msi.yml `build-verify` | Measured — not routing (a wash; stays hosted) |
| R5a | Route `bazel coverage`: workflow routing in bazel-coverage.yml | Done |
| R5b | Linux-template coverage warmup (`bazel coverage` into the template output base) | Done — template 918 built + warmed, rolled in via #903 |

Current state: `bazel / ubuntu-latest`, `bazel / windows-latest`, and
`bazel coverage` run on the pool for push-to-main and same-repo PRs. The macOS
leg has no pool venue and is no longer a PR check at all — it runs on
push-to-main and on the nightly schedule, both hosted. Templates and clone
VMIDs live in the `SLOTS` array in `tools/ci/rp-runner-pool.sh`, which is the
source of truth for what the pool runs — check there, not this document.
Since 2026-08 the LAN cache is served by the operator's NAS from an
interface on the runner VLAN over a 25 GbE fabric (see the skill doc);
routing, cache flags, and credentials in the workflows are unchanged.

R5 split into a workflow change (R5a, this file + `bazel-coverage.yml`) and a
template change (R5b), with a load-bearing ordering: `RP_POOL_LINUX` is already
`on` for the build/test leg, and R5a's routing gates on that same switch, so
R5a takes effect the moment it merges. R5b was therefore completed first — the
Linux template was rebuilt as **918** with a one-time `bazel coverage //...`
warmup so the nightly toolchain and instrumented externals live in its output
base (rolled in via #903); without it every ephemeral coverage clone would
re-fetch the nightly toolchain over the WAN, defeating the pool's zero-WAN
property. Measured payoff: the coverage leg runs ~2.8 min on a warmed pool
clone versus ~12 min cold. The second Linux slot R5 needs (a PR event fires
both Linux legs at once) is in place. R4b was measured and stays hosted.

The macOS leg is not deferred so much as **resolved the other way**. Giving it
a pool venue needs physical Apple hardware, which was scoped and measured in
#893; at current prices that is not worth buying for a leg that runs a build
everything else has already run. So instead of routing it, it was taken off
the PR gate: `bazel.yml`'s matrix drops `macos-latest` on `pull_request` and
keeps it on push-to-main and the nightly schedule, both hosted. macOS is still
built and tested on every merge — it simply no longer decides when a PR goes
green. `bazel / macos-latest` was removed from the branch ruleset's required
checks in the same change; leaving it required would have deadlocked every PR
on a check that no longer reports.

Two consequences worth stating, since they are the price of this:

* A macOS-only break now lands on main and is caught minutes later by the
  push-to-main run, instead of being caught before merge. Reverting is the
  remedy, and the window is one merge wide.
* Nothing on a PR denies rustc warnings for macOS any more (`check.yml`'s
  clippy is ubuntu-only). `test.yml`'s nightly `macos` job therefore runs with
  `RUSTFLAGS=-Dwarnings`, so the macOS-only warning class stays enforced
  somewhere rather than merely printed.

The remote-cache wedge ladder that was the strongest motivation for a macOS
venue (#765) is unaffected — it still bites the hosted runs.

### Host capacity (14 cores / 20 threads, 94 GB)

Slot RAM is **16 GB on both OSes**, measured rather than estimated. Method:
`bazel clean` then the full `build` + `test` + `bdd` sequence on a real slot,
sampling every 2s, run at two sizes so the elastic component is visible.

| | Linux (peak anon) | Windows (peak committed) |
|---|---|---|
| large slot | 9.35 GiB @ 32 GB | 14.63 GiB @ 20 GB |
| **16 GB slot** | **8.96 GiB** | **13.90 GiB** |
| headroom at 16 GB | 5.23 GiB available | 6.29 GiB available, 3% pagefile |
| wall clock | 479s → 459s | 578s → 578s |

Two things that make "peak vs slot size" the wrong way to budget:

* **Demand is elastic.** Bazel sizes its JVM heap and its action concurrency
  (`--local_ram_resources` defaults to 67% of visible RAM) from the box it is
  given, so halving the slot *lowered* peak demand on both OSes. Shrinking a
  slot partly shrinks the workload.
* **The two numbers are not comparable to each other.** Linux `AnonPages` and
  Windows *committed bytes* are each the metric that governs their own OS's
  failure mode — an OOM kill on Linux, commit exhaustion on Windows. Windows
  genuinely costs more (see #874: `--jobs=64` permits 64 heavyweight processes
  on a 16-core guest, and the peak lands in the link phase), but the ~1.5×
  ratio is indicative, not arithmetic.

The rule: **slot RAM ≥ 1.5× the measured peak of the heaviest workload,
re-measured when that workload changes.** The bazel job is the heaviest — the
MSI packaging job peaks near 9 GiB, well under it, because cargo self-limits
to core count.

**Disk, not RAM, is the binding constraint on slot count.** Clone disks belong
on `cipool` (the 4 TB NVMe), not the root mirror. Measured with fio, ZFS
file-based, mixed 70/30 16k — one job per simulated slot:

| concurrent jobs | rpool (500 GB QLC mirror) | cipool (4 TB) |
|---|---|---|
| 1 | 2,595 IOPS | 6,733 IOPS |
| 2 | 3,336 IOPS | 9,846 IOPS |
| 3 | **3,259 IOPS — declines** | **11,258 IOPS — still scaling** |

The root mirror saturates between one and two concurrent jobs and gets *worse*
at three, with p99.9 latency reaching 3.5s; 1.27% of random writes exceed two
seconds. That is QLC past its SLC cache, and it is why a slot count above two
is only useful once clone disks live on `cipool`.

vCPU is deliberately overcommitted, but the ceiling is real: the host is a
mobile i9-13900H with 14 cores / 20 threads. Slots run 12 vCPU each (the
"drop per-slot vCPU rather than hold 16" rule, applied when the pool grew
past three slots), since adding slots adds queueing capacity, not CPU.

A PR event fires at most three pool jobs (ubuntu, coverage, windows); msi — if
routed — queues briefly behind the Windows bazel leg. A second Windows slot is
gated on #872, not on capacity. Since 2026-08 a third Linux slot exists —
the bazel cache moving to the NAS freed its LXC's RAM and its share of
cipool I/O — so two PR events' Linux legs can overlap without queueing.

## Venue and cache matrix

The single behavioral contract everything below implements:

In the table and throughout this plan, "cloud cache" means the Cloudflare
R2-backed remote cache (`--config=remote-cache`) — spelled out to avoid
colliding with the phase identifiers.

| Event | Linux + Windows legs run on | Cache | Cache writes |
|---|---|---|---|
| `pull_request`, same-repo branch | pool | LAN | no (anonymous read) |
| `pull_request`, fork (after approval) | GitHub-hosted | cloud | no |
| `push` to main | pool | LAN | yes (repo secret) |
| nightly `schedule` | GitHub-hosted | cloud | yes (as today) |
| macOS leg (`push` + nightly only — never on a PR) | GitHub-hosted | cloud | as today |

Each OS carries its own kill switch — `RP_POOL_LINUX` and `RP_POOL_WINDOWS`
— because the two venues fail independently: a wedged Windows slot or a
stale Windows template should not cost Linux its speed, and vice versa.
Flipping one moves only that OS back to GitHub-hosted runners.

The nightly schedule staying **hosted** is deliberate: it is what keeps the
cloud cache's Linux entries warm, so a fork PR (which always runs hosted)
still gets a warm cache. The LAN cache is instead warmed by every push to
main.

## How routing works today

1. **Conditional `runs-on`** on bazel.yml's matrix job, one branch per pool
   OS:

   ```yaml
   runs-on: >-
     ${{ (matrix.os == 'ubuntu-latest'
          && vars.RP_POOL_LINUX == 'on'
          && (github.event_name == 'push'
              || (github.event_name == 'pull_request'
                  && github.event.pull_request.head.repo.full_name == github.repository))
          && fromJSON('["self-hosted", "proxmox-ephemeral"]'))
         || (matrix.os == 'windows-latest'
             && vars.RP_POOL_WINDOWS == 'on'
             && ...same trusted-event test...
             && fromJSON('["self-hosted", "proxmox-ephemeral-windows"]'))
         || matrix.os }}
   ```

   Every falsy branch resolves to `matrix.os` (hosted) — a fork PR, a
   schedule run, a deleted variable, or a null `head.repo` all land on the
   safe side. `macos-latest` reaches this expression only on the events that
   still put it in the matrix (push-to-main, nightly), and falls through to
   hosted. The trusted-event test is spelled out once per OS because
   `runs-on` is evaluated before the job exists, so neither the `env` context
   nor a job output can hold it; **the copies must stay identical — a
   divergence there is a security boundary moving.**

2. **Check names stay `bazel / <os>`** on both venues, so the
   `main_protection` ruleset needs no changes and a fork PR satisfies the
   same required check from a hosted runner.

3. **Provisioning steps are gated** on `runner.environment ==
   'github-hosted'`: both templates ship those tools at the same pins, and
   re-downloading them per ephemeral clone would put the WAN traffic back.
   Two Windows steps stay deliberately **ungated** — the long-paths registry
   key (downloads nothing, idempotent, and on the pool it guards against
   template drift) and `--output_base=C:/b`, which is load-bearing there
   because `C:\b` is the output base the template pre-warmed its external
   repos into.

4. **Cache flags.** Pool jobs override the cloud cache with
   `--remote_cache="$RP_LAN_CACHE_URL"`; hosted jobs keep
   `--config=remote-cache`. The LAN write credential
   (`BAZEL_LAN_CACHE_WRITE_AUTH`) attaches only on `push`, mirroring the
   cloud cache's public-read/token-write defense — fork PRs get no secrets at
   all, and same-repo PR events are excluded by the event gate.

5. **Runner VMs are VLAN-fenced** and carry no cache write credential; the
   security contract is ADR-020 and the skill doc, not this plan.

## Remaining work

### R4b — Route msi.yml `build-verify` (Measured — not routing)

Measured and **left on hosted**. `build-verify` is Cargo, not Bazel, so the
LAN Bazel cache does not help it; the open questions were raw cores versus
hosted, and whether `Swatinem/rust-cache` (which pulls over the WAN) helps or
hurts on this link. Both were answered by a same-cache A/B: two
`workflow_dispatch` runs of the same commit, one pinned to the Windows pool
and one on `windows-latest`.

Result: **pool 726s versus hosted 732s — a wash.** The pool's 12-core clone
compiles the suite ~200s faster (`Build the suite MSI` 425s vs 627s), but
that entire advantage is eaten by `rust-cache`'s save-over-WAN Post step
(224s) plus a smaller artifact-upload WAN tax (pool 31s vs hosted 4s). The
cause is structural, not incidental: the ephemeral clone never persists a
`target/` between jobs, so it pays the WAN cache tax on both ends of every
run, while 12 cores make even a *cold* compile fast enough that `rust-cache`
saves less than it costs to transfer — on this venue `rust-cache` is net
overhead. A `rust-cache`-off pool variant would land at ~495s (a real ~32%
win), but `build-verify` is a **non-required** check that fires only on
packaging-input PRs and would contend for one of just two Windows slots
against the required `bazel / windows-latest`, so the added complexity is not
worth it. Left hosted; revisit only if msi latency ever starts to matter.

### R5 — Route `bazel coverage` (Done — implementation record)

The third required Bazel check (bazel-coverage.yml), routed the same way as
the Linux leg above, with three coverage-specific points — all implemented
(R5a = the routing/cache/guards in bazel-coverage.yml; R5b = template 918's
coverage warmup, rolled in via #903):

1. **Expression.** Not a matrix job, so the routing expression above with a
   literal fallback: `… && fromJSON('["self-hosted", "proxmox-ephemeral"]') ||
   'ubuntu-latest'`. The event gate already sends the nightly `schedule` run
   to hosted runners — deliberate for the same reason as bazel.yml's: the
   schedule is what keeps the **cloud** cache's coverage entries warm for
   fork PRs. Kill switch: the same `RP_POOL_LINUX` variable — both Linux
   legs are healthy or unhealthy together (it is the same pool), and one
   flip must evacuate everything Linux.
2. **Cache split.** Same REMOTE_FLAGS block as bazel.yml: LAN URL override
   on the pool, LAN write auth (`BAZEL_LAN_CACHE_WRITE_AUTH`) on `push`
   only, `--remote_upload_local_results=false` otherwise. Push-to-main then
   warms the LAN cache's *coverage* namespace exactly as it does the
   build/test namespace. The provisioning steps (lld, bazelisk, OmniSim,
   Pebble, libusb, QHY SDK, ZWO SDK) get the same
   `runner.environment == 'github-hosted'` guards.
3. **Template warmup.** Coverage builds the whole graph **instrumented on
   the nightly toolchain** — a distinct action + external-repo namespace
   from the stable build/test the template was benched with. Before routing,
   the Linux template got a one-time warmup (a full `bazel coverage //...` run
   during the 917→918 rebuild) so the nightly toolchain and the instrumented
   externals live in the template's output base; without it, every ephemeral
   clone would re-fetch the nightly toolchain over the WAN on every job. That
   warmup run also served as the coverage pre-roll validation (110/110 tests
   green on the template) that `proxmox-runner-test.yml` does not cover. The
   codecov CLI download (~small, rolling `latest`) stays per-run; the Codecov
   upload runs from the pool over the WAN like any other egress.

## References

- [ADR-020](../decisions/020-ephemeral-self-hosted-runners-for-pr-checks.md)
  — the security contract this plan implements
- [Proxmox runner pool skill](../skills/proxmox-runner-pool.md) — pool
  architecture, ops, template rebuild procedure
- [Raspberry Pi runner skill](../skills/raspberry-pi-runner.md) — the
  unchanged rule for persistent runners
