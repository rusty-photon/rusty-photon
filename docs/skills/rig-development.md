# Skill: developing against the telescope field rig

How to get a near-real-time dev loop between a dev machine and the field-rig
Raspberry Pi that runs the packaged rusty-photon stack on the telescope.

The model: **drivers stay on the rig, the service you are editing runs
locally.** Every non-driver service (rp, sentinel, ui-htmx, session-runner,
plate-solver, calibrator-flats, phd2-guider, new tools such as autofocus) is
an HTTP client of the ASCOM Alpaca drivers, so it can run on the dev machine
and talk to the rig's live drivers over the LAN. That gives an edit → `cargo
run` → real-hardware loop of a few seconds with no deploy step. Driver work
itself still happens on the rig (build there, or install a deb per
[docs/packaging.md](../packaging.md)).

## One-time setup

1. **ssh alias.** The rig's address must never appear in this repository
   (public repo — keep infrastructure addresses out). All tooling resolves
   the host `rig` from your `~/.ssh/config`:

   ```
   Host rig
       HostName <rig-address>
       User <rig-user>
   ```

   The user needs passwordless sudo on the rig (Raspberry Pi OS grants the
   first user this by default). Override the alias name with `RIG_HOST=...`
   if yours differs.

2. **WiFi power-save must be off on the rig.** With it on (the Raspberry Pi
   OS default), the radio naps between access-point beacons and every inbound
   packet risks a 100–260 ms stall — it poisons ssh, rsync, and every Alpaca
   call with random latency. Symptom: `ping` times that swing from ~4 ms to
   hundreds of ms. Fix, on the rig:

   ```sh
   sudo iw dev wlan0 set power_save off                     # immediate
   sudo nmcli connection modify <wifi-con> 802-11-wireless.powersave 2   # persist
   ```

   Verify with `iw dev wlan0 get power_save` (expect `off`) and a flat ~5 ms
   ping. Cost: well under half a watt of extra idle draw.

## The loop

```sh
scripts/rig.sh fetch-configs      # rig configs -> ~/.config/rusty-photon-rig,
                                  # driver endpoints rewritten to the rig's address
cargo run -p ui-htmx -- --config ~/.config/rusty-photon-rig/ui-htmx.json
scripts/rig.sh logs dsd-fp2 -f    # tail the driver you're talking to
```

`scripts/rig.sh` also has `status`, `restart|start|stop <svc>`, and `ssh`.
Edit the fetched configs freely — they are your local dev copies, never
written back to the rig.

### Rules of engagement

- **One orchestrator at a time.** When running a local rp against the rig's
  drivers, stop the rig's own instance first (`scripts/rig.sh stop rp`) so
  two gateways don't command the same hardware; restart it when done.
  The same applies to any tool that moves equipment.
- **This is live hardware on a telescope.** A local service you're debugging
  can slew the mount, move the focuser, or rotate the imaging train. Know
  what your code will command before pointing it at the rig.
- **Serial device paths on the rig must be `/dev/serial/by-id/...`**, never
  `/dev/ttyUSB<n>` — enumeration order is not stable across boots or
  re-plugs, and with several FTDI adapters on one hub the `ttyUSBn` numbers
  are effectively random.

## Bandwidth expectations (WiFi rig)

The rig link is WiFi (~10 MB/s in practice). What that means for a locally
run service fetching camera data over Alpaca:

| Payload | Transfer time | Verdict |
|---|---|---|
| Autofocus/guiding subframes (≤ a few hundred KB) | ~instant | non-issue |
| Full guide-camera frame (~13 MB) | ~1.3 s | fine |
| Full large-CMOS frame (~50 MB) | ~5 s | tolerable dead time |
| Planetary / video streaming (350+ MB/s) | — | impossible; run on-rig |

In production the whole stack runs on the rig, so none of this applies there
— it is purely a property of the remote dev loop. USB-over-IP forwarding of
cameras was evaluated and rejected for the same reason (usbip and VirtualHere
both top out at ~30–40 MB/s even on wired gigabit, with disconnect-recovery
problems on top); revisit only if the rig ever gets a wired link.
