# Hardware validation records

Successful **real-hardware ConformU runs**, one directory per run. Where the
per-service design docs (`docs/services/<service>.md`, "Real-hardware
validation") narrate *what was learned*, this directory preserves *the
evidence*: which commit was tested, on what platform, against which physical
device, and the unmodified ConformU output.

## Runs

| Date | Service | Device | Platform | Commit | ConformU | Result | Record |
|------|---------|--------|----------|--------|----------|--------|--------|
| 2026-08-16 | svbony-camera | SVBONY SV605CC | Raspberry Pi OS (Debian 13) aarch64 — field rig | [`97854524`](https://github.com/rusty-photon/rusty-photon/commit/97854524) | 4.5.0 | `alpacaprotocol` (0 information alerts) + `conformance` clean, against the packaged nightly deb started from a **pristine working directory** — the issue #891 connect-handshake fix (`Gain` settable before the first exposure) confirmed on the physical camera, with a before/after probe of the previously deployed binary | [record](2026-08-16-svbony-camera-sv605cc-rig-connect-handshake/README.md) |
| 2026-08-08 | svbony-camera | SVBONY SV605CC | Raspberry Pi OS (Debian 13) aarch64 — field rig | [`c530940a`](https://github.com/rusty-photon/rusty-photon/commit/c530940a) | 4.5.0 | `alpacaprotocol` (0 information alerts) + `conformance` clean, against the packaged nightly deb over the **production TLS+auth endpoint** (Let's Encrypt chain verified, HTTP Basic on every request) | [record](2026-08-08-svbony-camera-sv605cc-rig/README.md) |
| 2026-08-07 | qhy-camera | QHY178M + CFW | Fedora Linux 44 x86_64 | [`54a7a168`](https://github.com/rusty-photon/rusty-photon/commit/54a7a168) (`main` at the `rusty-photon-camera-core` merge) | 4.5.0 | `alpacaprotocol` + `conformance` clean, **both** Camera and FilterWheel; the QHY leg of the shared-crate refactor, through a second vendor SDK | [record](2026-08-07-qhy-camera-qhy178m-cfw-linux/README.md) |
| 2026-08-07 | zwo-camera | ZWO ASI1600MM-Cool + ASI178MM + ASI120MC-S | Fedora Linux 44 x86_64 | [`7e12a9b3`](https://github.com/rusty-photon/rusty-photon/commit/7e12a9b3) (`main` + the `rusty-photon-camera-core` refactor under test) | 4.5.0 | `alpacaprotocol` + `conformance` clean on **all three** bodies from one service instance; puts the shared ROI rules, Bayer offsets and `ImageArray` unpack on real hardware | [record](2026-08-07-zwo-camera-three-cameras-linux/README.md) |
| 2026-08-05 | zwo-camera | ZWO ASI1600MM-Cool | Fedora Linux 44 x86_64 | `222ffe08` (`main` + the `MaxADU` margin under test) | 4.4.0 | `alpacaprotocol` + `conformance` clean; `MaxADU` measured against delivered pixels (65504, not 65520) and cooling exercised with the TEC powered | [record](2026-08-05-zwo-camera-asi1600mm-cool-maxadu/README.md) |
| 2026-08-05 | zwo-camera | ZWO ASI178MM | Fedora Linux 44 x86_64 | [`269a4cc3`](https://github.com/rusty-photon/rusty-photon/commit/269a4cc3) | 4.4.0 | `alpacaprotocol` + `conformance` clean, on the negotiated readout-format list; `MaxADU` re-measured against delivered pixels | [record](2026-08-05-zwo-camera-asi178mm-maxadu/README.md) |
| 2026-08-05 | svbony-camera | SVBONY SV605CC | Raspberry Pi OS (Debian 13) aarch64 — field rig | [`4b8b8179`](https://github.com/ivonnyssen/rusty-photon/commit/4b8b8179) | 4.4.0 | `alpacaprotocol` (**0 information alerts**) + `conformance` clean, on the negotiated readout-format list | [record](2026-08-05-svbony-camera-sv605cc-rig-readout/README.md) |
| 2026-07-30 | svbony-camera | SVBONY SV605CC | Raspberry Pi OS (Debian 13) aarch64 — field rig | [`05d74aab`](https://github.com/ivonnyssen/rusty-photon/commit/05d74aab) | 4.4.0 | `alpacaprotocol` + `conformance` clean, against the **packaged** arm64 deb | [record](2026-07-30-svbony-camera-sv605cc-rig/README.md) |
| 2026-07-28 | qhy-camera | QHY178M + CFW | Fedora Linux 44 x86_64 | [`e7ce4a0e`](https://github.com/ivonnyssen/rusty-photon/commit/e7ce4a0e) | 4.4.0 | `alpacaprotocol` + `conformance` clean, **both** Camera and FilterWheel | [record](2026-07-28-qhy-camera-qhy178m-cfw-linux-4.4.0/README.md) |
| 2026-07-28 | qhy-camera | QHY178M + CFW | Windows 11 Pro 25H2 x64 | [`7d9d93c9`](https://github.com/ivonnyssen/rusty-photon/commit/7d9d93c9) | 4.4.0 | `alpacaprotocol` + `conformance` clean, **both** Camera and FilterWheel | [record](2026-07-28-qhy-camera-qhy178m-cfw-windows/README.md) |
| 2026-07-28 | qhy-camera | QHY178M + CFW | Fedora Linux 44 x86_64 | [`7d9d93c9`](https://github.com/ivonnyssen/rusty-photon/commit/7d9d93c9) | 4.3.0 | `alpacaprotocol` + `conformance` clean, **both** Camera and FilterWheel | [record](2026-07-28-qhy-camera-qhy178m-cfw-linux/README.md) |
| 2026-07-27 | zwo-camera | ZWO ASI1600MM-Cool | Windows 11 (25H2) x64 | [`1f2b9d16`](https://github.com/ivonnyssen/rusty-photon/commit/1f2b9d16) | 4.4.0 | `alpacaprotocol` + `conformance` clean | [record](2026-07-27-zwo-camera-asi1600mm-cool-windows/README.md) |
| 2026-07-27 | zwo-camera | ZWO ASI1600MM-Cool | Fedora Linux 44 x86_64 | [`e0281daf`](https://github.com/ivonnyssen/rusty-photon/commit/e0281daf) | 4.3.0 | `alpacaprotocol` + `conformance` clean | [record](2026-07-27-zwo-camera-asi1600mm-cool-linux/README.md) |
| 2026-07-27 | svbony-camera | SVBONY SV605CC | Fedora Linux 44 x86_64 | [`bdd97201`](https://github.com/ivonnyssen/rusty-photon/commit/bdd97201) | 4.3.0 | `alpacaprotocol` + `conformance` clean | [record](2026-07-27-svbony-camera-sv605cc-linux/README.md) |
| 2026-07-26 | svbony-camera | SVBONY SV605CC | Windows 11 (25H2) x64 | [`ef03a1cd`](https://github.com/ivonnyssen/rusty-photon/commit/ef03a1cd7b9e0831e731d0ed9d37df7661fe5edd) | 4.4.0 | `alpacaprotocol` + `conformance` clean | [record](2026-07-26-svbony-camera-sv605cc-windows/README.md) |

## Adding a run

Each run gets a directory named `<YYYY-MM-DD>-<service>-<device>-<platform>/`
containing:

- `README.md` — the run record: the exact commit tested (`git rev-parse HEAD`
  of the built tree), platform and environment details, how the binary was
  built (features, SDK provenance and version), the device identity
  (model + serial as minted into the ASCOM `UniqueID`), the verdicts, and
  anything platform-specific the run taught us.
- The unmodified ConformU output. Ask ConformU to write its own artifacts
  rather than scraping the console:

  ```sh
  conformu alpacaprotocol <device-url> -n alpacaprotocol.log
  conformu conformance    <device-url> -n conformance.log -r conformance-results.json
  ```

- `conformance-results.json` — ConformU's machine-readable verdict
  (`ErrorCount` / `IssueCount` / `ConfigurationAlertCount` /
  `TimingIssuesCount` must all be 0 for a run to be recorded here).

**Check the local ConformU matches what CI resolves before you run.**
`conformu.yml` pins no version — it installs `latest` on every run — so the
locally installed tool can silently fall behind. A record made on a version
the project no longer runs is evidence for a validator it has moved past:

```sh
conformu --version   # prints e.g. "Conform Universal 4.5.0 (Build …)"
gh api repos/ASCOMInitiative/ConformU/releases/latest \
  --jq '.tag_name | ltrimstr("v")'   # the tag is v-prefixed; print "4.5.0"
```

Only **successful** runs are recorded — this directory is the proof trail
that a given commit passed on real hardware, not a debugging journal.
Failures belong in issues. Before committing logs, check they carry no
private network addresses or local usernames (loopback URLs are fine).

Finally, add the run to the table above (newest first) and, when the run is
a service's first on a platform, link the record from the service design
doc's "Real-hardware validation" section.
