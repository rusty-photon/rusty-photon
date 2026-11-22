# Runner pool templates: their lineage, 907 to 928 and 909 to 911

A record, moved whole from `tools/ci/rp-runner-pool.sh` on 2026-11-22: the
lineage of the runner pool's Linux and Windows templates — what each
template added over its parent, why, and which one was kept for rollback —
as the comment above the script's slot table held it. The comment there now
says where a template lives and why, and points at
[proxmox-runner-pool.md](../../skills/proxmox-runner-pool.md), which states
what a template carries today (§ What This Is) and how one is rebuilt and
rolled forward (§ Operational Notes). The VMIDs below are one host's; the
slot table that names a host's templates is host state, in each
hypervisor's `/etc/rp-runner/slots`. "relax_clone_sync below", "slot_loop"
and "the NIC isolation below" are the script's.

```sh
# Templates live on cipool (the 4 TB NVMe), not the root mirror: clone disks
# are the write-heavy, disposable part of the workload and the mirror collapses
# under concurrency (see docs/skills/proxmox-runner-pool.md, storage layout).
# 928 = Linux, 911 = Windows, both 16 GB / 6 vCPU — resized 2026-08 after the
# oversubscription flake wave (5 slots × 12 vCPU on a 20-thread host bred
# timing flakes across nine suites; 5 × 6 keeps worst-case load ~1.5×). The
# guest-wide freezes behind most of that wave turned out to be storage-side
# sync-write queueing, which relax_clone_sync below removes at the source.
# Current templates: 928 (Linux), 911 (Windows).
#
# 928 is a full clone of 927 that bakes the coverage job's annotation tool:
# python3-pip is installed, diff-cover==10.5.1 is pre-installed for the ci
# user (the same pin the uncovered-diff-lines action installs per run, so on
# a pool clone that command becomes an offline "already satisfied" no-op),
# and the runner's .env gains PIP_BREAK_SYSTEM_PACKAGES=1. That variable is
# load-bearing: Ubuntu 24.04's apt-installed pip enforces PEP 668
# (externally-managed-environment) and fails a plain `pip install --user`
# even when the requirement is already satisfied, so without it the baked
# package would sit unused while the annotations go unposted. 927 is
# retained for rollback until 928's clones are proven.
#
# 927 is a full clone of 926 with the guest's unattended upgrades removed:
# `unattended-upgrades` and `needrestart` are purged and the `apt-daily` /
# `apt-daily-upgrade` timers and services are masked. 926 shipped stock Ubuntu
# defaults there, and `apt-daily-upgrade.timer` is `Persistent=true` with a
# 60-minute random delay, so every fresh clone ran unattended-upgrades within
# an hour of boot. Once the archive carried a newer libssl than the template
# (the runner process maps libssl and libcrypto), needrestart's apt hook —
# which Ubuntu runs in automatic-restart mode — restarted `gha-runner.service`
# underneath whatever it was doing. A busy runner meant a killed job ("The
# runner has received a shutdown signal"); an idle one meant a deregistered
# runner in a VM that stayed up, which the health check reclaimed as wedged
# ten probes later. An ephemeral clone gains nothing from patching itself:
# it is destroyed after one job, so updates belong in the template. 927 took
# the pending updates once, at build time, before the purge. 927 also carries
# the in-repo tools/ci/runner-guest/one-job.sh (926's copy predated the
# 30-minute no-config deadline) and a refreshed coverage warmup.
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
```
