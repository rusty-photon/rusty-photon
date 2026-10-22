# zwo-camera

ASCOM Alpaca **Camera** driver for ZWO ASI hardware, served on port
**11122**. It is the ZWO analogue of `qhy-camera`, built on the vendored
first-party [`zwo-rs`](../../crates/zwo-rs) FFI crate. Other ZWO devices are
separate services (ADR-014): the EAF focuser is `zwo-focuser`, and the EFW
filter wheel is a future `zwo-filterwheel` service.

See [`docs/services/zwo-camera.md`](../../docs/services/zwo-camera.md) for the
full design, [`docs/plans/zwo-driver.md`](../../docs/plans/zwo-driver.md) for the
decision record, and [ADR-008](../../docs/decisions/008-zwo-camera-native-sdk-ffi.md)
for the native-SDK / FFI-crate decision.

## Status — Phase E (full Camera) landed

This crate implements the full ASCOM `Device` + `Camera` surface over the
`zwo-rs` SDK seam (`backend.rs`): connection lifecycle, sensor geometry (with
`CameraXSize`/`CameraYSize` reported aligned so the binned full frame stays a
valid ASI ROI), symmetric binning, ROI with the ASI `%8`/`%2` alignment rules,
gain/offset, cooling, readout modes, asynchronous ST4 pulse guiding, and the
snap-mode exposure state
machine (start; abort *discards* / graceful stop *preserves*; `ImageArray`,
`CameraState`, `PercentCompleted`, mid-exposure `Error` — all of which report
the **running** session and answer `NOT_CONNECTED` outside one), plus serial-derived
identity and its setup pages (`/setup`, with the Cameras section that edits
`usb_devices`, and a page per camera). A camera that leaves the
bus while connected reads `Connected = false` once a call on it fails and a
rescan no longer finds it; a reconnect finds it again by its serial once it is
back (design doc C6). Phase E landed
validated by **45 unit tests** (against the in-crate mock seam), **57 BDD
scenarios**, and a full **ConformU** pass (both `alpacaprotocol` and
`conformance` suites); the suite now stands at **144 unit tests** (the mock
seam, and the production handle against the `zwo-rs` simulation) and **85 BDD
scenarios** — see the design doc's *Testing* section. Roadmap:

- **Phase F** — re-scoped by ADR-014 to a future separate `zwo-filterwheel`
  service (not part of this crate).
- **Phase G** — mostly done: ConformU is wired into `conformu.yml` (per-service
  matrix, native ZWO SDK provisioned via `install-zwo-sdk`), and the nightly
  `native.yml` builds the real linked path on Linux/macOS/Windows. Remaining
  tail: the `rp` `CameraConfig` consumer.

The six camera BDD feature files under `tests/features/` are live.

## Native dependency (the crux)

`zwo-rs` is built with only its `camera` feature (ADR-014), so this binary
links exactly the ASI camera SDK (`libASICamera2` + `libusb-1.0`).
Consequences:

- **Every machine that compiles this package needs that SDK installed** — dev
  laptops, CI runners, Bazel actions — not just machines with a camera attached.
  (The shared Bazel `zwo-rs` targets build the union of device features, so
  Bazel actions provision all three ZWO blobs.)
- The `simulation` feature makes the build **camera-free, not SDK-free**: it
  fabricates frames at runtime, but the native SDK is still linked.
- The build **fails to link without the SDK**, so **install it before building**
  (see below). `bazel build //...` includes this package, so the SDK is a required
  local-dev prerequisite — CI and the Bazel actions install it the same way.

## Building locally

```sh
# 1. The prerequisites: libusb and libudev for the link, libclang for bindgen.
#    (Debian/Ubuntu)
sudo apt-get install libusb-1.0-0-dev libudev-dev clang libclang-dev
#    (macOS) the install below copies Homebrew's libusb beside the SDK
brew install libusb llvm

# 2. The MIT-licensed ZWO SDK, as CI installs it: the same files, from the
#    same pinned indi-3rdparty commit (Windows: ZWO's developer SDKs).
#    `cargo build -p zwo-camera` links only libASICamera2 (ADR-014), but
#    `bazel build //...` builds the shared zwo-rs targets with the union of
#    device features, so it installs all three SDKs.
bazel run //.github/actions:install -- zwo-sdk

# 3. bindgen needs libclang; point LIBCLANG_PATH at it if not auto-found
#    (e.g. /usr/lib64 on Fedora, /usr/lib/$(uname -m)-linux-gnu on Debian,
#    $(brew --prefix llvm)/lib on macOS).
export LIBCLANG_PATH=/usr/lib64

bazel build //services/zwo-camera/...
cargo run  -p zwo-camera --features simulation -- --port 11122
```

On Linux and macOS the install puts the libraries in `/usr/local/lib` and the
headers in `/usr/local/include`, where libzwo-sys's build script looks, using
`sudo` for the copy when you cannot write there; it replaces those files and
nothing else. The libraries carry no SONAME, so on Linux a binary that loads
them finds them through the loader's cache, and where that has no
`/usr/local/lib`, as on Fedora, the install prints an `LD_LIBRARY_PATH` line
to set. Adding `/usr/local/lib` to a file in `/etc/ld.so.conf.d/` and running
`sudo ldconfig` does it for every binary, Bazel's tests included, which get
no `LD_LIBRARY_PATH`. With `--prefix DIR` elsewhere it also prints the
`ZWO_SDK_LIB_DIR` Cargo needs to link, but Bazel looks only in `/usr/local`.
On Windows it installs into `%USERPROFILE%\.local\opt\zwo-sdk` and prints a
`setx ZWO_SDK_LIB_DIR` line to run once; a build that talks to a real camera
also needs that directory on `PATH`, for the DLLs, and bindgen needs LLVM
(`LIBCLANG_PATH`).

## CLI

| Flag | Description |
|---|---|
| `--config <path>` | Config file (default: `~/.config/rusty-photon/zwo-camera.json`). |
| `--port <port>` | Override `server.port`. |
| `--log-level <level>` | `trace`\|`debug`\|`info`\|`warn`\|`error` (default `info`). |
| `doctor [--json]` | Read-only diagnosis of the config file and what the ASI SDK sees. |
| `doctor --devices` | List the ASI cameras on the bus by USB port and print the `usb_devices` block to paste. Opens nothing. |

## Configuration

Edit it on the service's own setup page, `http://<host>:11122/setup`
(design doc "Setup pages"), or in the file:

```jsonc
{
  // Optional: pin each USB port to an Alpaca device number; a camera on an
  // unlisted port is never opened. Paste it from `zwo-camera doctor --devices`
  // (docs/services/zwo-camera.md "Device claims"). Absent = every camera the
  // USB scan places, in port order.
  // "usb_devices": [{ "device_number": 0, "usb_port": "pci-0000:c1:00.4-usbv3-0:1.4" }],
  "devices": {},          // per-serial name/description overrides (no list only)
  "server": {
    "port": 11122,
    "bind_address": "0.0.0.0", // default: all interfaces
    "tls": null,               // shared server block (rusty-photon-server-config);
    "auth": null               // absent tls/auth = plain unauthenticated HTTP
  }
}
```

## Features

| Feature | Effect |
|---|---|
| `simulation` | Forwards to `zwo-rs/simulation`: a fabricated `ASI2600MM-Pro-Simulated` camera. Removes the camera, **not** the SDK link. |
| `mock` | Alias for `simulation` (the cross-driver test-backend name). |
| `conformu` | Enables `mock`; builds + runs the ConformU compliance test (`tests/conformu_integration.rs`). |
