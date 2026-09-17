# Skill: validating a driver against real hardware

## When to Read This

- Before running ConformU against a physical device
- Before adding a record to [`docs/validation/`](../validation/)
- When a driver change needs real-hardware proof: a new service, a vendor
  SDK change, a fix to a connect/exposure/reconnect path, or a first run
  on a new platform

This is the **evidence** task. `docs/validation/` is the proof trail that
a given commit passed on real hardware; the per-service design docs
narrate what was learned. Failures belong in issues, not here.

## The version gate — read this first

`conformu.yml` pins **no** ConformU version. It installs `latest` on
every run, so a locally installed tool silently falls behind. **A record
made on a version the project no longer runs is evidence for a validator
it has moved past** — the run is wasted.

Check before you start, not after:

```sh
conformu --version   # prints e.g. "Conform Universal 4.5.0 (Build …)"
gh api repos/ASCOMInitiative/ConformU/releases/latest \
  --jq '.tag_name | ltrimstr("v")'   # the tag is v-prefixed; prints "4.5.0"
```

Mismatch → reinstall before running. The same drift is why a nightly can
go red with no commit of ours; diagnosing *that* direction is
[babysitting-prs.md](babysitting-prs.md) § "When only the ConformU legs
go red".

## Install and run

```sh
./scripts/test-conformance.sh --install-conformu   # installs the latest release
```

The installer resolves `latest` at run time, matching what CI installs —
it does **not** pin, so it cannot silently put you behind. Re-run the
version check afterwards anyway: it confirms what actually landed, and an
*existing* install from an earlier session is exactly the stale copy the
gate exists to catch.

To reproduce an old run deliberately, pin it:

```sh
CONFORMU_VERSION=v4.4.0 ./scripts/test-conformance.sh --install-conformu
```

A run pinned that way must not be filed as a new record — it is for
reproducing history, and the gate above is the rule for anything new.

For the record itself, invoke ConformU directly so it writes its own
artifacts rather than scraping the console:

```sh
conformu alpacaprotocol <device-url> -n alpacaprotocol.log
conformu conformance    <device-url> -n conformance.log -r conformance-results.json
```

Both suites must be clean. In `conformance-results.json`, `ErrorCount`,
`IssueCount`, `ConfigurationAlertCount` and `TimingIssuesCount` must
**all** be 0 for the run to be recorded.

Working against the field rig rather than a local device:
[rig-development.md](rig-development.md).

## Record the run

One directory per run, `<YYYY-MM-DD>-<service>-<device>-<platform>/`,
containing:

- **`README.md`** — the exact commit tested (`git rev-parse HEAD` of the
  built tree), platform and environment, how the binary was built
  (features, SDK provenance and version), the device identity (model +
  serial as minted into the ASCOM `UniqueID`), the ConformU version, the
  verdicts, and anything platform-specific the run taught us.
- **The unmodified ConformU output** — the `.log` files above.
- **`conformance-results.json`** — the machine-readable verdict.

Then:

1. Add the run to the table in
   [`docs/validation/README.md`](../validation/README.md), newest first.
2. When it is a service's **first** run on a platform, link the record
   from that service's design doc under "Real-hardware validation".

Before committing logs, check they carry no private network addresses or
local usernames. Loopback URLs are fine; the rig's address must never
appear (public repository).

## What does not belong here

- **Failed runs.** They are issues, not records. This directory is a
  proof trail, not a debugging journal.
- **CI conformance behaviour** — when `conformu.yml` runs, the nightly
  cron, the tracking issue it opens: [pre-push.md](pre-push.md)
  § "conformu.yml (rolling)".
- **Per-device findings** — what a given camera taught us about `MaxADU`,
  readout formats or connect handshakes belongs in that service's design
  doc, where the next person to touch the driver will read it.
