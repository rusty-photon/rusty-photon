# Sky-Watcher Motor-Controller Command Set — Engineering Notes

**Canonical specification:**
[skywatcher_motor_controller_command_set.pdf](https://inter-static.skywatcher.com/downloads/skywatcher_motor_controller_command_set.pdf)
(Sky-Watcher Application Notes, no version number on the file).

This file is **not** a copy of the upstream specification. It contains
the project's own engineering notes about how we use the protocol —
compatibility, empirically-verified behaviour on our hardware, and the
load-bearing implementation gotchas the codec needs to handle. For the
authoritative command list, frame format, status-bit layout, motion-mode
flags, and error codes, follow the link above.

## What this protocol is

The wire protocol the [Sky-Watcher motor-controller command set] PDF
defines. Used by:

- The [`star-adventurer-gti`](../services/star-adventurer-gti.md) ASCOM
  Telescope driver (USB at 115200 baud, or UDP/11880 over WiFi).
- The `skywatcher-motor-protocol` crate (codec; transport-agnostic).

Per the upstream spec §6 (Wi-Fi Connection), the same protocol runs on
both serial transports.

## Mounts that speak this protocol

Compiled from the [EQMOD project compatibility list][eqmod] and the
[INDI `indi-eqmod` driver manifest][indi-eqmod-xml] — not from the
Sky-Watcher PDF.

**Sky-Watcher branded:** EQ4, EQ5, HEQ5, NEQ6, EQ6-R Pro, EQ8, EQ8-R,
EQ8-Rh, EQM-35 Pro, AZ-EQ5GT, AZ-EQ6GT, AZ-GTi, Star Adventurer GTi,
HDX110.

**Orion branded (Synta OEM):** Sirius Pro AZ/EQ-G, Atlas Pro AZ/EQ-G.

This is the same wire protocol on every mount in the list, but
mount-side parameters (counts-per-revolution, timer-interrupt
frequency, high-speed ratio, capability flags) vary, so the driver
queries them at connect time rather than hard-coding.

## Implementation gotchas

These two are the most common source of codec bugs and are worth
flagging at the top of the codec module.

### 24-bit data is sent low byte first

For a 24-bit value `0x123456`, the wire bytes are ASCII
`"5" "6" "3" "4" "1" "2"` — low byte first, with each byte's nibbles in
normal high-then-low order. Only the byte order is reversed; nibble
order within each byte is normal.

### Axis positions carry a `0x800000` bias

Axis positions are conveyed with a fixed bias of `+0x800000` so that the
unsigned hex on the wire can carry both signed-positive and
signed-negative encoder counts. Subtract on decode, add on encode;
service code should see signed encoder counts only.

### UDP framing is strict

Empirically verified on the Star Adventurer GTi (see `:e1` probe table
below): the controller silently drops UDP packets that contain anything
beyond a single well-formed `:cmd<axis><payload>\r` frame.

| UDP payload | Result |
|---|---|
| `:e1\r` | reply received |
| `:e1\r\n` | reply received (trailing `\n` tolerated) |
| `:e1` (no `\r`) | silent — controller waits for terminator |
| `:e1\r` + zero-padding | silent — extra bytes after `\r` reject the frame |
| `\xff…:e1\r` (junk-prefixed) | silent — bytes before `:` reject the frame |

The codec must enforce: exactly one well-formed frame per UDP packet,
nothing trailing. (This is implied by the spec's framing rules but not
called out directly.)

### Resync on mid-frame `:`

Per the spec, if the controller sees a second `:` before `\r`, it
discards the partial frame and starts fresh. Useful for recovering from
a corrupted half-sent command.

## Empirically verified on the Star Adventurer GTi

These are observations from our own probing on this physical mount, not
content from the upstream PDF.

### USB-CDC accepts 115200 baud

The spec specifies **9600 8N1** for the UART. The Star Adventurer GTi's
USB-C virtual COM port additionally accepts **115200 baud** (matches
EQMOD documentation; faster). We use 115200 by default.

### WiFi requires explicit local-IP source binding

The mount's built-in WiFi answers UDP/11880 at `192.168.4.1` in AP mode.
The host **must** bind the local socket to a `192.168.4.x` source
address explicitly. Relying on the kernel's default-route source-IP
selection picks the wrong address when a competing default route is
present, and the mount silently drops packets it can't reply to.

### Sample probe values on this mount

A clean reply to each handshake command, useful as a smoke-test target
for the mock transport:

| Command | Wire reply | Decoded |
|---|---|---|
| `:e1\r` (motor-board version) | `=03300C\r` | firmware 3.48 (`0x03`, `0x30`), mount code `0x0C` = Star Adventurer GTi — the wire bytes are firmware major, firmware minor, mount code; decoded low-byte-first the value is `0x0C3003`, mount code in the high byte (INDI eqmod's `MountCode`) |
| `:a1\r` (CPR axis 1, RA) | `=005F37\r` | `0x375F00` = 3,628,800 counts/revolution |
| `:a2\r` (CPR axis 2, Dec) | `=004C2C\r` | `0x2C4C00` = 2,903,040 counts/revolution |
| `:b1\r` (TMR_Freq) | `=0024F4\r` | `0xF42400` ≈ 16 MHz |
| `:j1\r` (position axis 1) | `=000080\r` | `0x800000` (with bias) → 0 ticks (home) |
| `:f1\r` (status axis 1) | `=100\r` | tracking-mode preset, motor stopped, not initialised |
| `:F1\r` (initialize axis 1) | `=\r` | empty-payload ack |

CPR is per-axis, and on the GTi the axes genuinely differ: the Dec
CPR is 0.8× the RA CPR (both values above captured from the real
mount over USB, 2026-07-20). CPR also varies between mount models,
so do not bake either value into anything beyond test fixtures —
the mock transport seeds this measured pair precisely so that a
conversion using the wrong axis' CPR fails tests instead of passing
on coincidentally identical values.

### Initialization sequence we use on connect

After opening the transport, before the first motion command:

1. `:e1` — identity gate (mount-code whitelist; see the
   `skywatcher_motor_protocol::MountType` enum). On a wrong-device
   handshake (frame malformed, payload wrong shape, mount code
   outside the whitelist) the driver stops here and surfaces
   `StarAdvError::WrongDevice` — bounding the wrong-device blast radius
   to a single inquiry. Motivated by the 2026-05-17 hardware session
   where the operator pointed the driver at a QHY focuser by mistake
   (issue #254).
2. `:F1`, `:F2` — initialize both axes
3. `:a1`, `:a2` — record CPR per axis
4. `:b1` — record TMR_Freq
5. `:g1`, `:g2` — record high-speed ratio per axis
6. `:j1`, `:j2` — record initial encoder positions

Steps 2–6 seed the in-memory parameter cache used by the coordinate
module and the slew planner.

### Changing the step period of a running axis

The spec restricts `:I` only while the motor slews in high-speed mode,
and marks `:G`, `:E` and `:S` "motor must be full stopped". What `:I`
does on a motor that is running at a slow (tracking) rate was measured
on this mount (firmware 3.48) on 2026-09-28. This was not inferred from
the spec. The service was stopped, and
`services/star-adventurer-gti/examples/probe_live_step_period.rs`
sampled `:j1` every 4 ms, since `:f` does not report the period:

- **Accepted and applied.** 240 of 240 `:I1` frames sent to an RA axis
  tracking at sidereal, 0.5× or 1.5× were answered `=`. The encoder
  took the new rate from the first step after the frame.
- **Inert on a stopped axis.** An `:I1` sent to a stopped axis, or 0 to
  60 ms after a `:K1`, never started it.
- **`:K1` from a tracking rate stops fast.** The running flag clears
  within 3.7–4.4 ms of the `:K1` reply, with 0–1 tick of coast.
- **Every rate change on a running axis steps the encoder forward.**
  On top of the rate change, the count advances by a small amount at
  each edge. The means, in ticks, were:

  | Edge | Step |
  |---|---|
  | sidereal → 0.5× | +0.60 |
  | 0.5× → sidereal | +0.79 |
  | sidereal → 1.5× | +1.52 |
  | 1.5× → sidereal | +1.69 |
  | `:J1` on a running axis | +1.43 |
  | `:J1` from standstill | −0.25 ± 0.67 (none) |

  The rates between edges match `TMR_Freq / period` to within a few
  tenths of a percent. The steps are therefore extra counts at the
  edges, not a rate error.
- **`:i1` (inquire step period) is implemented.** It reads `=FFFFFF`
  before any `:I` has been sent.
- **The `:j` count is not a clean step clock.** It advances in short
  bursts at higher rates and can drift ±2 ticks for a second after a
  stop. Fit rates over hundreds of milliseconds, not single intervals.

## See also

- [INDI `indi-eqmod` driver source][indi-eqmod] — the canonical
  open-source reference implementation; we cross-check ambiguous bits of
  the spec against this driver.
- [EQMOD project][eqmod] — Windows-side reference driver and
  protocol-decoding documentation.
- [Sky-Watcher sample code archive (Google Code, archived)](https://code.google.com/archive/p/skywatcher/)
- [Sky-Watcher application development page (canonical PDFs)](http://www.skywatcher.com/download/manual/application-development/)

[Sky-Watcher motor-controller command set]: https://inter-static.skywatcher.com/downloads/skywatcher_motor_controller_command_set.pdf
[indi-eqmod]: https://github.com/indilib/indi-3rdparty/tree/master/indi-eqmod
[indi-eqmod-xml]: https://github.com/indilib/indi-3rdparty/blob/master/indi-eqmod/indi_eqmod.xml.cmake
[eqmod]: https://eq-mod.sourceforge.net/
