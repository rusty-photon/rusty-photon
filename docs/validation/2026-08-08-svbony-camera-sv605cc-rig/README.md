# svbony-camera on the field rig over production TLS+auth — SV605CC, 2026-08-08

Routine re-validation of the packaged `svbony-camera` after a fleet-wide
nightly upgrade, and two firsts: the first SV605CC record on **ConformU
4.5.0**, and the first ConformU run made against the service's **production
TLS + HTTP Basic auth endpoint** — the certificate chain (a real Let's
Encrypt wildcard) validated against the system trust store, every request
authenticated with the observatory credential. No trust overrides, no
config changes on the rig, no temporarily de-secured service.

## What was tested

| | |
|---|---|
| Commit | [`c530940a`](https://github.com/rusty-photon/rusty-photon/commit/c530940a) (`origin/main` at test time) |
| Service | packaged arm64 nightly deb `rusty-photon-svbony-camera 0.1.0+nightly.202608080551.gc530940`, installed in the same-session upgrade of all 18 fleet packages |
| SDK | `libSVBCameraSDK.so` v1.13.4 `armv8`, placed by the packaged `rusty-photon-svbony-sdk-install` helper (unchanged since the 2026-07-30 record) |
| udev | packaged `90-rusty-photon-svbony.rules` |
| Platform | Raspberry Pi 5 (BCM2712, aarch64), Raspberry Pi OS (Debian 13 trixie) — the telescope field rig; camera direct on a Pi USB 3 port |
| Camera | SVBONY SV605CC, hardware serial `0123481353808C03EE2512150035` (the same physical unit as every prior record) |
| Transport | HTTPS to the service's LAN endpoint from the x86_64 dev box; server certificate is the rig's Let's Encrypt wildcard, verified against the system store; HTTP Basic auth with the observatory credential on every request |
| ConformU | 4.5.0 build 53834, x86_64 dev box |

## Verdicts

- **`alpacaprotocol`** — *"no errors, issues or information alerts"*:
  [alpacaprotocol.log](alpacaprotocol.log). Zero information alerts again,
  consistent with the 2026-08-05 record's finding that the packaged unit's
  writable `WorkingDirectory` eliminates the historical `PUT Gain` items.
- **`conformance`** — *"no errors, warnings or issues found"*, and every
  member inside its response-time target:
  [conformance.log](conformance.log), machine-readable
  [conformance-results.json](conformance-results.json)
  (`ErrorCount`/`IssueCount`/`ConfigurationAlertCount`/`TimingIssuesCount`
  all 0). Full frames captured at every bin 1×1 through 4×4 (8.9 MPix at
  1×1), `MaxADU 65535`, readout modes `Raw16`/`Raw8` both advertised and
  index-consistent, interface version 4.

## Running ConformU against an authenticated Alpaca device

First time ConformU met a TLS+auth rusty-photon endpoint; two of its
behaviours cost a false-start each and are worth keeping:

1. **The `conformance` suite takes its URL scheme from the settings file,
   not the CLI URI.** The device client is constructed from
   `AlpacaConfiguration.AccessServiceType` (default `Http`) even when the
   command line says `https://…`. Plain HTTP to a TLS port answers with the
   same-port 308 redirect, and .NET's `HttpClient` strips the
   `Authorization` header when it follows a cross-scheme redirect — so the
   run dies at connect with `Unauthorized` despite correct credentials.
   Fix: set `"AccessServiceType": "Https"` alongside `AccessUserName` /
   `AccessPassword` in the settings file passed via `-s`.
2. **The `alpacaprotocol` suite only authenticates its raw protocol
   requests.** Its embedded ASCOM client-library calls (device setup,
   `NumX` and friends) are built without the configured credentials and
   401 — an upstream ConformU gap, present in 4.5.0. The suite was
   therefore run through a loopback forward proxy that injects the
   `Authorization` header only when a request lacks one and forwards over
   verified TLS to the service. The device under test is unchanged and
   every request still authenticates at the service; the raw protocol
   requests — the substance of this suite — carry their own credentials
   end to end.

The `conformance` suite needed no proxy: with `AccessServiceType` set it
authenticates natively, which is what this record's log shows.

## Same-session companion run

The QHY5III715C on the same rig and nightly passed `alpacaprotocol` clean
but failed `conformance` with 10 issues, all one cause: the 715C's bin
list is sparse (`{1, 2, 4}`), `MaxBinX/Y` reports 4, and ConformU's
exposure ladder demands the missing 3×3. The written ASCOM spec does not
require contiguous bins (`MaxBinX` is the maximum *supported* value and
the interface has no way to enumerate a sparse set), so this was settled
as working-as-intended: the driver keeps reporting the true maximum
rather than hiding the hardware's 4×4 — evidence, spec citations and the
decision in
[#933](https://github.com/rusty-photon/rusty-photon/issues/933). A 715C
record here therefore stays blocked on current ConformU's exposure-ladder
interpretation, not on the driver.
