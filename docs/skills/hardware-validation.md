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

The installer is **Linux x64 only** and refuses to run anywhere else — it
fetches the `conformu.linux-x64.tar.xz` asset. On macOS, Windows, or a
Linux box of another architecture (the aarch64 field rig, say), install
the matching asset by hand from the
[ConformU releases page](https://github.com/ASCOMInitiative/ConformU/releases/latest)
— the arm64 asset is `conformu.linux-arm64.tar.xz`, and it unpacks flat,
so extract it into its own directory.

Either way it lands **off your `PATH`**: the installer writes
`$HOME/tools/conformu/conformu` and changes nothing else. The bare
`conformu` commands below assume you have put it on yours —

```sh
export PATH="$HOME/tools/conformu:$PATH"
```

The installer resolves `latest` at the moment you run it, matching what
CI installs — so a fresh install is never behind on arrival. It says
nothing about later: releases keep landing, and an *existing* install
from an earlier session is exactly the stale copy the gate exists to
catch. Run the version check on every validation run, including right
after installing, where it confirms what actually landed.

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

Both verbs call ConformU's `SetFullTest()` after reading any `-s` settings
file, so a record is always the **full** test set. A settings file on
these verbs is for connection settings (the `AlpacaConfiguration` block
for HTTPS and credentials), timeouts and delays: every key ConformU marks
`[MandatoryInFullTest]` is overridden, whatever it says, and the
`TelescopeTests` dictionary is rebuilt all-true. (A few settings that
shape the test set carry no such attribute and are left to the file. A
deselected `DomeTests` entry raises a configuration alert, which the
all-zero rule below rejects; there is no dome driver here anyway.
`TestPerformance` and `AlpacaConfiguration.ProtocolTestPrimaryUrlStructure`
only add tests and are off by default. The camera caps (`CameraMaxBinX`
/ `CameraMaxBinY`, `CameraXMax` / `CameraYMax`) and a Switch's
`SwitchExtendedNumberTestRange` narrow the tests they bound without
raising any alert.) The forced-on keys that
ConformU ships *off* are the ones that move hardware, so plan for them:
on a Switch, `SwitchEnableSet` is forced on and ConformU **will write**
the switches (a powerbox's outputs included); on a German-equatorial
mount at a site latitude beyond ±10° whose `SideOfPier` read succeeds,
`TestSideOfPierRead` is forced on and ConformU slews to hour angles −3,
−9, +3 and +9 to check `SideOfPier`, whatever `CanSetPierSide` says;
and on one that also reports `CanSetPierSide = true`, the forced-on
`TestSideOfPierWrite` makes ConformU slew to HA −3 at Dec 0, then to
HA −0.03 h (two minutes east of the meridian), let the mount track
across it for seven minutes and set `SideOfPier` to the opposite
pointing state — a meridian flip. The write test runs inside the read
test's gate, so a mount whose `SideOfPier` read answers
`NOT_IMPLEMENTED` gets neither, only an `INFO` skip.
Tolerances (`TelescopePulseGuideTolerance`, `TelescopeSlewTolerance`,
`FocuserMoveTolerance`, …) *are* honoured, and a loosened one raises no
configuration alert and appears nowhere in the results file; the only
evidence is the value ConformU prints inline on each measurement line
(`Test tolerance: …`), which nothing in the all-zero rule reads. A
record's settings file must never loosen a tolerance beyond ConformU's
defaults. The smallest valid file is
`{"SettingsCompatibilityVersion": 1}` plus whatever `AlpacaConfiguration`
the endpoint needs; every property it omits keeps ConformU's default. Spell
that key exactly so, with no whitespace before the colon: ConformU finds it
by literal text search, and a file without it is silently renamed to
`<file>.prereleaseversion` and replaced by defaults — HTTPS and credentials
included. A file ConformU cannot parse (a comment, a trailing comma, a
wrongly typed value) is ignored for the run with only an `ERROR` line at the
top of the output, and the exit code does not show it. (The
in-tree tests build theirs from `bdd_infra::FullRunSettings`, which cannot
express a selection or a tolerance at all — see
[testing.md §1.4](testing.md#14-conformu-integration-tests).)

Both suites must be clean. In `conformance-results.json`, `ErrorCount`,
`IssueCount`, `ConfigurationAlertCount` and `TimingIssuesCount` must
**all** be 0 for the run to be recorded.

### Scoped records — the one exception, and what it must carry

A run that is not all-zero may still be recorded as a **scoped record**
when the evidence it carries is worth having before the remaining
defects are fixed — the case is a run made to prove one specific fix on
hardware while unrelated, already-filed defects keep the counts nonzero.
It is an exception by decision, not a softer default, and it holds only
when every one of these is true:

- **Every remaining error or issue is attributable to an open issue that
  already exists**, named in the record's README. A finding with no issue
  is a failed run, and gets one filed instead of a record.
- **None of the remaining findings is in the behaviour the run set out to
  validate.** A scoped record proves one thing and says which; it does
  not quietly narrow what "pass" means.
- **`ConfigurationAlertCount` and `TimingIssuesCount` are still 0.** The
  exception covers findings against the device that an open issue
  already owns. A configuration alert means the run itself was narrowed,
  and a timing issue is a defect no open issue is likely to carry —
  either one makes it a failed run, not a scoped record.
- **The decision to file is recorded outside the record** — on the issue
  that asked for the run — so the README reports a decision rather than
  making one.
- **The README's first paragraphs and the index row both say "scoped
  record"** and name the open issues, so a reader can never mistake it
  for an all-zero proof.
- **A clean run is still owed**, and the README says what has to land
  before it can be made. Filing the scoped record does not discharge
  that.

The first such record is
[2026-09-26-star-adventurer-gti-gti-rig](../validation/2026-09-26-star-adventurer-gti-gti-rig/README.md):
the counterweight-up PulseGuide direction, with the RA-axis findings all
carried by two open issues.

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
- **The unmodified ConformU output** — the `.log` files above. The one
  edit permitted is the privacy scrub below: a private address or
  hostname replaced by a placeholder such as `<rig-host>`, with the
  README naming the lines that were touched, so a reader can tell the
  scrub from the evidence.
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
  proof trail, not a debugging journal. The only nonzero run that belongs
  here is a *scoped record* meeting every condition in § "Scoped records"
  above — anything short of that is a failed run.
- **CI conformance behaviour** — when `conformu.yml` runs, the nightly
  cron, the tracking issue it opens: [pre-push.md](pre-push.md)
  § "conformu.yml (rolling)".
- **Per-device findings** — what a given camera taught us about `MaxADU`,
  readout formats or connect handshakes belongs in that service's design
  doc, where the next person to touch the driver will read it.
