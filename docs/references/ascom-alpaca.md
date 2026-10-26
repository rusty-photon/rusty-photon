# ASCOM Alpaca Reference

This document contains reference information for ASCOM Alpaca development in the rusty-photon project.

## Official Resources

- **ASCOM Alpaca API (Swagger UI)**: https://ascom-standards.org/api/
- **ASCOM Master Interfaces Documentation**: https://ascom-standards.org/newdocs/
- **Switch Interface Specification**: https://ascom-standards.org/newdocs/switch.html
- **API YAML Specifications**:
  - Device API: https://ascom-standards.org/api/AlpacaDeviceAPI_v1.yaml
  - Management API: https://ascom-standards.org/api/AlpacaManagementAPI_v1.yaml

## Rust Crate

This project uses the `ascom-alpaca` crate: https://crates.io/crates/ascom-alpaca

Current version: `1.0.0-beta.10`

Features used:
- `server` - HTTP server for exposing devices
- Device-specific features: `safety_monitor`, `switch`, etc.

## URL Format and Conventions

**Base Format:** `http(s)://host:port/api/v1/{device_type}/{device_number}/{method}`

**Key Rules:**
- Device type and method names must be lowercase
- Parameter names are case-insensitive
- Parameter values may use mixed case
- GET operations use URL query strings
- PUT operations place parameters in message body
- All responses return JSON format

**Examples:**
- Switch max count: `http://192.168.1.89:11112/api/v1/switch/0/maxswitch`
- Get switch value: `http://192.168.1.89:11112/api/v1/switch/0/getswitchvalue?Id=2`

## Response Format

All responses include standard fields:
```json
{
  "ClientTransactionID": 123,
  "ServerTransactionID": 456,
  "ErrorNumber": 0,
  "ErrorMessage": "",
  "Value": <device-specific>
}
```

## HTTP Status Codes

| Code | Meaning |
|------|---------|
| 200 | Request formatted correctly, passed to device handler (check ErrorNumber for actual success) |
| 400 | Device couldn't interpret request: an invalid device number, a misspelled device type, an unknown method name in the path (`GET …/camera/0/canslew`), or a missing or malformed parameter. An unknown `Action` *name* sent to `PUT …/action` is not a 400: it answers 200 with `ErrorNumber` 0x40C (`ACTION_NOT_IMPLEMENTED`) |
| 500 | Unexpected device error |

## Common Device Endpoints (All Devices)

All device types share these standard endpoints:

| Endpoint | Method | Purpose |
|----------|--------|---------|
| `/{device_type}/{device_number}/connected` | GET/PUT | Retrieve or set device connection state |
| `/{device_type}/{device_number}/connect` | PUT | Starts async connect to device |
| `/{device_type}/{device_number}/disconnect` | PUT | Starts async disconnect from device |
| `/{device_type}/{device_number}/connecting` | GET | Check if connect/disconnect in progress |
| `/{device_type}/{device_number}/description` | GET | Device description |
| `/{device_type}/{device_number}/driverinfo` | GET | Device driver description |
| `/{device_type}/{device_number}/driverversion` | GET | Driver version number |
| `/{device_type}/{device_number}/interfaceversion` | GET | ASCOM interface version |
| `/{device_type}/{device_number}/name` | GET | Device name |
| `/{device_type}/{device_number}/supportedactions` | GET | List of custom actions |
| `/{device_type}/{device_number}/devicestate` | GET | Aggregated operational state |
| `/{device_type}/{device_number}/action` | PUT | Execute device-specific action |

`Action` names are matched without regard to case: "SelectWheel,
selectwheel and SELECTWHEEL all refer to the same action".
`SupportedActions` returns each name in its canonical spelling.

## Setup Pages (Management API)

The Management API defines two HTML pages, served by the server itself
(our drivers serve them behind their own TLS and Basic Auth; see
[driver-setup-pages.md](../plans/archive/driver-setup-pages.md)):

| Path | Method | Purpose |
|------|--------|---------|
| `/setup` | GET, POST | Whole-server configuration page; "must also enable the user to set cross cutting information that applies to the whole device such as the operating IP address and port number". Ours (`rusty-photon-setup`) answers `POST` with the save |
| `/setup/v1/{device_type}/{device_number}/setup` | GET, POST | One device's configuration page; "must be implemented, even if the response to the user is that the device is not configurable"; 403 with an HTML message for an unknown device |

The pinned `ascom-alpaca` fork routes both for `GET` and `POST` through
`Device::setup_request` and `Server::setup`; the crate's old device page
path, `/api/v1/{device_type}/{device_number}/setup`, answers a 308 to the
specification's path.

---

## Switch Interface (ISwitchV3)

The Switch interface manages controllable switches and sensors. Switches are numbered from 0 to `MaxSwitch - 1`.

### Switch Properties

| Property | Type | Description |
|----------|------|-------------|
| `maxswitch` | GET | Number of switch devices (0 to MaxSwitch-1) |

### Switch Methods - State Operations

| Method | HTTP | Parameters | Description |
|--------|------|------------|-------------|
| `getswitch` | GET | `Id` (int) | Get boolean state (True=On, False=Off) |
| `setswitch` | PUT | `Id` (int), `State` (bool) | Set boolean state |
| `getswitchvalue` | GET | `Id` (int) | Get numeric value (between min and max) |
| `setswitchvalue` | PUT | `Id` (int), `Value` (double) | Set numeric value |

### Switch Methods - Metadata

| Method | HTTP | Parameters | Description |
|--------|------|------------|-------------|
| `getswitchname` | GET | `Id` (int) | Get switch name |
| `setswitchname` | PUT | `Id` (int), `Name` (string) | Set switch name (if supported) |
| `getswitchdescription` | GET | `Id` (int) | Get switch description |
| `canwrite` | GET | `Id` (int) | Check if switch is writable |

### Switch Methods - Value Range

| Method | HTTP | Parameters | Description |
|--------|------|------------|-------------|
| `minswitchvalue` | GET | `Id` (int) | Minimum allowed value |
| `maxswitchvalue` | GET | `Id` (int) | Maximum allowed value |
| `switchstep` | GET | `Id` (int) | Step size between valid values |

### Switch Methods - Async Operations (V3+)

| Method | HTTP | Parameters | Description |
|--------|------|------------|-------------|
| `canasync` | GET | `Id` (int) | Check if async operations supported |
| `setasync` | PUT | `Id` (int), `State` (bool) | Non-blocking boolean state change |
| `setasyncvalue` | PUT | `Id` (int), `Value` (double) | Non-blocking numeric value change |
| `statechangecomplete` | GET | `Id` (int) | Check if async operation complete |
| `cancelasync` | PUT | `Id` (int) | Cancel in-progress async operation |

### Switch Implementation Notes

**Boolean vs Analog Switches:**
- Boolean switches: `MinSwitchValue=0.0`, `MaxSwitchValue=1.0`, `SwitchStep=1.0`
- Analog switches: Custom min/max/step values (e.g., 0-100% for PWM)

**GetSwitch for Variable Outputs:**
- Returns `False` when at minimum value
- Returns `True` when above minimum value

**SetSwitch Behavior:**
- `SetSwitch(id, True)` should set value to `MaxSwitchValue`
- `SetSwitch(id, False)` should set value to `MinSwitchValue`

**Async Completion Detection:**
- Do NOT use `GetSwitch()`/`GetSwitchValue()` to detect completion
- These may reflect requested (not actual) values during operation
- Use `StateChangeComplete()` exclusively

---

## Telescope Interface (Site Properties)

Used by `rp` for site validation on mount connect (see
[`docs/services/rp.md`](../services/rp.md) §"Site Validation
Against the ASCOM Mount"). The Telescope interface is large
(~70 properties/methods); only the subset `rp` reads at startup
is documented here. The full interface is at the upstream Alpaca
device spec — <https://ascom-standards.org/api/>.

### Site Geographic Properties

| Property | HTTP | Type | Description |
|----------|------|------|-------------|
| `sitelatitude` | GET | `f64` (degrees, +N) | Geodetic latitude (WGS84) of the observing site |
| `sitelongitude` | GET | `f64` (degrees, +E) | Longitude (WGS84) of the observing site |
| `siteelevation` | GET | `f64` (metres, ASL) | Site elevation — `rp` does not consume this in v1 |

### Capability Probe Pattern

ASCOM has **no** `CanGetSiteLatitude` / `CanGetSiteLongitude`
capability bit — the read attempt itself is the capability probe.
A mount that does not expose the property returns
`0x400 NOT_IMPLEMENTED`; `rp` treats any read error as "skip the
validation" rather than "fail loud", with a `debug!()` log to make
the skip visible. The setters (`set_sitelatitude`,
`set_sitelongitude`) are not used by `rp` — site lat/lon comes
from the config file on disk; the mount is asked only to confirm.

---

## SafetyMonitor Interface

Used by the `filemonitor` service in this project.

### SafetyMonitor Methods

| Method | HTTP | Description |
|--------|------|-------------|
| `issafe` | GET | Returns boolean indicating safe (True) or unsafe (False) condition |

---

## Error Codes

Standard ASCOM error codes:

| Code | Name | Description |
|------|------|-------------|
| 0x400 | NOT_IMPLEMENTED | Method/property not implemented |
| 0x401 | INVALID_VALUE | Invalid parameter value |
| 0x402 | VALUE_NOT_SET | Value has not been set |
| 0x407 | NOT_CONNECTED | Device not connected |
| 0x408 | INVALID_WHILE_PARKED | Invalid while device is parked |
| 0x409 | INVALID_WHILE_SLAVED | Invalid while device is slaved |
| 0x40B | INVALID_OPERATION | Invalid operation in current state |
| 0x40C | ACTION_NOT_IMPLEMENTED | Requested action not implemented |
| 0x500 | UNSPECIFIED_ERROR | Catch-all for other errors |

---

## Device Types

ASCOM Alpaca supports these device types:

- `camera` - CCD/CMOS cameras
- `covercalibrator` - Flat panel and dust cover devices
- `dome` - Observatory domes
- `filterwheel` - Filter wheels
- `focuser` - Focus controllers
- `observingconditions` - Weather/environment sensors
- `rotator` - Camera/instrument rotators
- `safetymonitor` - Safety monitoring devices
- `switch` - Generic switch/relay controllers
- `telescope` - Telescope mounts

---

## Testing and Conformance

**ConformU**: ASCOM provides ConformU for testing Alpaca device conformance.
- Download: https://github.com/ASCOMInitiative/ConformU

**Testing Pattern** (from filemonitor):
```rust
// tests/conformu_integration.rs
// Run ConformU against the service to validate ASCOM compliance
```
