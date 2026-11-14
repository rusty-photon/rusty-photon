# svbony-camera: upgrading from SDK-order numbering, 2026-10

A record, moved whole from `docs/services/svbony-camera.md` on 2026-11-13:
what an installation that relied on the no-list default had to change once
that default numbered cameras by USB port rather than in SDK order — the
default it replaced, and the three steps. It stood under U10 in
[svbony-camera.md § Device claims](../../services/svbony-camera.md#device-claims-usb_devices),
which states what the no-list default does today.

**Upgrading from SDK order.** Until the device-claims plan's C5 the no-list
default registered every camera the SDK enumerated, in SDK order, with no
USB scan, and gave a serial-less camera `noserial-{index}`. A rig with one
camera keeps its number, 0. With more than one, or a serial-less camera:
1. run `svbony-camera doctor --devices` and compare its **Device** column
   (the numbers served now) with rp's `cameras[].device_number`, or paste
   its block to pin them;
2. re-key any `devices` entry keyed `noserial-<index>` to the
   `noserial-<port>` the camera now has (or move it into a `usb_devices`
   entry);
3. re-copy any rp `unique_id` pin on a serial-less camera from rp's
   bound-device log line or `GET /api/equipment`
   ([rp.md](../../services/rp.md#device-identity-pin)): its `UniqueID` changed, and rp
   refuses the stale pin by name.
