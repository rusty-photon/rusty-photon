# ADR-020: Ephemeral self-hosted runners may serve pull_request checks under a layered contract

## Status

Accepted (2026-08-02). Implementation tracked in
[`docs/plans/proxmox-pr-routing.md`](../plans/proxmox-pr-routing.md). Until
that plan's R2 lands, the skill docs' dispatch/schedule-only trigger rule
remains the enforced state; this ADR is the authority for what replaces it
and why.

## Context

This is a public repository. On `pull_request` events, GitHub Actions
executes the PR's own copy of the workflow YAML, so any workflow that a fork
can trigger effectively hands the fork author arbitrary code execution on
whatever runner the job lands on. For a self-hosted runner on the operator's
LAN, that is an unacceptable default — which is why
[the Raspberry Pi runner skill](../skills/raspberry-pi-runner.md) and
[the Proxmox pool skill](../skills/proxmox-runner-pool.md) both pinned
self-hosted workflows to `workflow_dispatch`/`schedule` triggers, and why
the Pi doc explicitly named the only acceptable escape hatch: "a JIT
ephemeral runner pool with PR-approval gating."

That pool now exists and is measured: ~16 s for the Linux Bazel steps on an
unchanged tree (warm LAN cache) versus 4–10 minutes on GitHub-hosted
runners, with zero WAN traffic after cache population. The value is
concentrated exactly where the old rule forbids running: per-PR required
checks. Meanwhile the risk profile of the pool is categorically different
from a persistent runner: single-use JIT registration, a fresh VM per job,
destruction after every job.

## Decision

`pull_request`-triggered jobs may target the **ephemeral pool** — never any
persistent self-hosted runner — provided *all* of the following layers are
in place. The layers are deliberately independent: each one contains a
failure of the others.

1. **Routing excludes forks by construction.** The job's `runs-on` is an
   expression that selects the pool only for same-repo events
   (`push`, or `pull_request` where `head.repo.full_name == github.repository`);
   every falsy branch — fork PR, schedule, unset kill-switch variable,
   null `head.repo` — resolves to a GitHub-hosted label. Check names stay
   identical on both venues so required checks are venue-agnostic.
2. **Fork approval is the human checkpoint.** The repo's fork-PR policy
   requires approval for **all** outside collaborators. A fork PR can edit
   the routing expression itself, so the approval step is what stands
   between that edit and execution: reviewing the workflow-file diff before
   approving a fork PR's runs is part of the approval contract.
3. **Runners are single-use and short-lived.** JIT registration binds one
   runner identity to one job; the VM is a fresh linked clone that powers
   off and is destroyed after its job. A compromised job cannot mint
   further registrations and leaves nothing behind.
4. **No standing credentials on the VM.** The runner `.env` carries only
   what anonymous reads need (the LAN cache URL, masked in logs). The cache
   **write** credential is a GitHub Actions secret attached only on `push`
   events — fork PRs receive no secrets at all, and same-repo PR events are
   excluded by the event gate. This mirrors the Cloudflare R2 cloud cache's
   public-read/token-write poisoning defense exactly.

   One bounded exception, on Windows only: the template enables autologon,
   which stores that VM's local administrator password in the registry in
   cleartext. It buys the interactive desktop session the test suite needs
   (see the skill doc), and it is not what this layer guards against — the
   credential unlocks nothing beyond the ephemeral clone, on which the job
   already runs elevated. It stays bounded because only one Windows clone
   runs at a time and the Linux template does not share the password.
5. **Network fencing.** Runner VMs live on a dedicated VLAN that can reach
   the WAN, DNS, and the LAN build cache's port — nothing else on RFC1918.
   Pool control runs over the QEMU guest agent, which needs no network
   path, so fencing cannot break pool mechanics.

   *Amended 2026-08-10:* the LAN cache moved from a container on the
   Proxmox host (reached through the inter-VLAN gateway) to the operator's
   NAS, which now holds an interface on the runner VLAN itself and serves
   the cache there at L2 over the 25 GbE fabric. The reachable surface from
   a clone is unchanged in intent: only the cache's HTTP port is published
   on that NAS interface (management UI, SSH, and file shares are bound to
   other networks only), reads stay anonymous, and writes still require the
   GitHub-secret credential per layer 4. The router fence now allows only
   the WAN and DNS off-VLAN. The pool skill doc carries the operational
   details.
6. **A no-commit kill switch.** Routing is gated on a repo Actions variable
   (opt-in) — one per pool OS, since the venues fail independently — so a
   pool outage is a settings flip back to hosted runners, not an emergency
   workflow PR while a required check blocks every merge.

Residual risk, with all layers up: an approved-despite-review malicious
fork job steals one VM's CPU for at most the job timeout and has WAN egress
from the operator's IP — no secrets, no cache writes, no LAN reachability,
no persistence.

## Consequences

- The trigger rule bifurcates: **persistent** self-hosted runners (the Pi
  nightly runner) keep dispatch/schedule-only, unchanged and non-negotiable;
  the **ephemeral pool** may serve `pull_request` under this contract. Both
  skill docs state their side of the split and link here.
- Required checks gain a hard dependency on operator infrastructure being
  up, mitigated by the kill switch (layer 6) and by keeping the hosted path
  working at all times (fork PRs exercise it continuously; the nightly
  schedule stays hosted and keeps the cloud cache warm for it).
- Toolchain pins live in two places: the workflow's install steps (which
  hosted runners still execute) and the pool VM template. A pin bump
  requires a template rebuild; the pool skill doc owns that procedure.
- Weakening any single layer is not a local decision: it invalidates the
  residual-risk analysis above and requires revisiting this ADR.
