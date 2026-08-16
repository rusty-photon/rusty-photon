# Installing svbony-camera on Windows

`svbony-camera` runs on Windows and passes ASCOM ConformU against real
hardware (see the
[validation record](validation/2026-07-26-svbony-camera-sv605cc-windows/README.md)),
but it is **not yet part of the Windows MSI suite** — SVBony gates both its
SDK and its driver behind a captcha'd download page, so neither can be
fetched automatically ([ADR-018](decisions/018-svbony-sdk-no-license-payload-policy.md))
and both installs below are manual. Until
[#720](https://github.com/ivonnyssen/rusty-photon/issues/720) Part 2 lands
an MSI story, install from source as follows. Everything else about
Windows deployments (config locations, doctor, logs) matches the
[Windows packaging & deployment guide](packaging-windows.md).

## 1. Install the SVBony camera driver (required)

SVBony cameras do not advertise Microsoft OS descriptors, so Windows will
**not** bind its in-box WinUSB driver on its own — without the vendor
driver the device shows an error in Device Manager and the SDK enumerates
zero cameras.

1. Download the Windows camera driver from
   [svbony.com → Support → Software & Driver](https://www.svbony.com/downloads/software-driver)
   (e.g. `SVBONY-Driver-DS-V1.13.4-20250205.exe`). The page is
   captcha-gated; a browser is required.
2. Run the installer (it is unattended-friendly: `/VERYSILENT /NORESTART`).
3. With the camera plugged in, verify the binding:

   ```powershell
   Get-PnpDevice | Where-Object InstanceId -like '*VID_F266*'
   ```

   The device should list as **SVBONY USB Camera** with `Status: OK`.

## 2. Stage the SVBony SDK

1. From the same downloads page, fetch the Windows SDK zip pinned by this
   workspace: **`windows-SVBCameraSDK-v1.13.4.zip`** (the SDK version must
   match the pin — check `crates/svbony-rs/libsvbony-sys/build.rs` if in
   doubt).
2. Extract it somewhere stable, e.g. `C:\SVBONY`. The pieces that matter:
   `C:\SVBONY\lib\x64\SVBCameraSDK.lib` (build-time link) and
   `C:\SVBONY\lib\x64\SVBCameraSDK.dll` (runtime).

## 3. Install the build toolchain

- **Visual Studio 2022 Build Tools** with the *Desktop development with
  C++* workload:

  ```bat
  curl.exe -L -o %TEMP%\vs_BuildTools.exe https://aka.ms/vs/17/release/vs_BuildTools.exe
  %TEMP%\vs_BuildTools.exe --quiet --wait --norestart ^
    --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended
  ```

  (The bootstrapper can return before the install finishes; wait until
  `cl.exe` exists under the install's `VC\Tools\MSVC\...\bin` tree.)

- **Rust** (stable, MSVC target) via [rustup](https://rustup.rs):

  ```bat
  curl.exe -L -o %TEMP%\rustup-init.exe https://win.rustup.rs/x86_64
  %TEMP%\rustup-init.exe -y --default-toolchain stable-x86_64-pc-windows-msvc
  ```

- **Git** ([git-scm.com](https://git-scm.com/download/win) or `winget
  install Git.Git`).

## 4. Build the real-SDK binary

From a fresh console (so rustup's `PATH` update is picked up):

```bat
git clone https://github.com/ivonnyssen/rusty-photon.git
cd rusty-photon
set SVBONY_SDK_LIB_DIR=C:\SVBONY\lib\x64
cargo build --release -p svbony-camera
copy C:\SVBONY\lib\x64\SVBCameraSDK.dll target\release\
```

Leave `SVBONY_SKIP_NATIVE_LINK` unset — setting it produces the
simulation-only binary that never talks to hardware. The DLL must sit
next to `svbony-camera.exe` (or on `PATH`) at runtime.

## 5. Verify and run

Check what the SDK sees without starting the service:

```bat
target\release\svbony-camera.exe doctor
```

Expect `hardware.sdk-devices … the SDK sees 1 device(s): <your camera>`.
Then start the service:

```bat
target\release\svbony-camera.exe
```

- The Alpaca server listens on port **11125**; the default configuration
  self-materializes at `%ProgramData%\rusty-photon\svbony-camera.json` on
  first start.
- For clients on other machines, open the port once (elevated console):

  ```bat
  netsh advfirewall firewall add rule name="rusty-photon svbony-camera" dir=in action=allow protocol=TCP localport=11125
  ```

- Quick smoke test:
  `curl http://localhost:11125/management/v1/configureddevices` should
  report your camera with a `UniqueID` of the form
  `SVBONY:<model>:<hardware serial>`.

## Notes

- On connect, the SVBony SDK writes a per-model parameter file
  (`<model>_Cfg_A.bin`, from the handshake's `SVBRestoreDefaultParam`) to
  `%APPDATA%\CKConfig\`. This is SDK behavior, harmless, and safe to delete
  when the camera is closed. (The driver turns the SDK's parameter auto-save
  off at connect, so the `_Cfg_SAVE.bin` it used to write at close no longer
  appears — see the design doc's "Working directory" section.)
- `svbony-camera.exe --help` lists the service flags (`--config`,
  `--port`, `--log-level`, and the `doctor` subcommand's `--json`).
