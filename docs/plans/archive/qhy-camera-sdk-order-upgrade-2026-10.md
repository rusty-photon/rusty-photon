# qhy-camera: upgrading from SDK-order numbering, 2026-10

A record, moved whole from `docs/services/qhy-camera.md` on 2026-11-13: what
an installation that relied on the no-list default had to check once that
default numbered cameras by USB port rather than in SDK order — the default
it replaced, and the check. It stood under U11 in
[qhy-camera.md § Device claims](../../services/qhy-camera.md#device-claims-usb_devices),
which states what the no-list default does today.

**Upgrading from SDK order.** Until the device-claims plan's C5 the no-list
default registered every camera the SDK enumerated, in SDK order, and
probed each, with no USB scan. A rig with one camera keeps its numbers, 0
for the camera and 0 for its wheel. With more than one, run
`qhy-camera doctor --devices` and compare its **Device** column (the camera
numbers served now) with rp's `cameras[].device_number`, and check rp's
`filter_wheels[].device_number` against the wheels the start logs
(`filter_wheels=`), or paste the block, declaring each wheel, to pin both.
No `UniqueID`, `devices` key or rp `unique_id` pin changes (*Device
identity*).
