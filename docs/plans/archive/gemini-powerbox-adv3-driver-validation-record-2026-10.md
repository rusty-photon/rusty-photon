# gemini-powerbox-adv3-driver: the hardware validation list, 2026-10

A record, moved whole from `docs/services/gemini-powerbox-adv3-driver.md`
on 2026-11-17: the status banner the design document opened with, the six
questions the design left to the first run on the dev box's unit with the
answer each got on 2026-10-26 or 2026-10-27, and the two open items those
answers settled. The banner stood at the top of
[gemini-powerbox-adv3-driver.md](../../services/gemini-powerbox-adv3-driver.md),
the list under
[§ Testing](../../services/gemini-powerbox-adv3-driver.md#testing) and the
open items under
[§ Open items](../../services/gemini-powerbox-adv3-driver.md#open-items);
"this document" in the list is that design document, which states what the
driver does today and, under
[§ Real-hardware validation](../../services/gemini-powerbox-adv3-driver.md#real-hardware-validation),
where each answer is stated now. The run's evidence is its
[validation record](../../validation/2026-10-26-gemini-powerbox-adv3-driver-adv3-linux/README.md).

## Status banner

**Status: implemented (Phase 3 of [development-workflow.md](../../skills/development-workflow.md)), 2026-10-26**,
against a mock that serves the captured frame and streams as the hub does,
and validated on the dev box's unit the same day: the
[write probe](../../services/gemini-powerbox-adv3-driver.md#what-the-wire-actually-does) answered items 1, 2 and 4 of
the [hardware validation](#hardware-validation) list and ConformU ran
clean on the hub ([record](../../services/gemini-powerbox-adv3-driver.md#real-hardware-validation)); items 3 and 5,
the absent-sensor frames and a power cycle, were answered on the same unit
on 2026-10-27.
Everything stated about the wire was either read from the
two open-source drivers named under [Sources](../../services/gemini-powerbox-adv3-driver.md#sources) or measured against
the dev box's own unit on 2026-10-26, and the document says which.

## Hardware validation

The first validation run on the dev box's unit (firmware 308) settles the
facts this design could not, each recorded in the run's
`docs/validation/` entry and folded back into this document:

1. **Two commands in one buffer.** Does `>O3#\n>G#\n` yield a frame
   showing output 3 changed? If the firmware drops the second command, the
   write path falls back to two requests. *Answered 2026-10-26:* yes —
   `>C3#\n>G#\n` drew a frame with DC3 off and `>O3#\n>G#\n` one with
   it on, 124 ms after each send
   ([write probe](../../services/gemini-powerbox-adv3-driver.md#what-the-wire-actually-does)); no fallback is needed.
2. **Fields 8/9 under `>X`/`>Y`.** Does the duty field follow the set at
   once in Manual mode, and what does it show in Auto mode — the manual
   setpoint or the loop's live duty? *Answered 2026-10-26:* in Manual the
   frame behind the set shows the new duty; in Auto the field is the
   loop's live duty, which overwrites a `>X` within a few seconds, and a
   return to Manual keeps the loop's last value
   ([dew heaters](../../services/gemini-powerbox-adv3-driver.md#dew-heaters-three-switches-per-channel)).
3. **Absent sensors.** The frame with the AHT20 unplugged, and with the
   DS18B20 unplugged: fields 2/3 and whatever the firmware puts in 10–13.
   *Answered 2026-10-27:* the flag drops and each of the sensor's fields
   reads a bare `0` — `0T0H0D` for the AHT20, `0S` for the probe, in every
   combination — and nothing else in the frame changes; the AHT20's flag
   and fields changed in one frame, out and back, while the probe's
   transitions fell inside the USB reset its jack causes and were not
   seen in-stream. Pulling or inserting the DS18B20's 3.5 mm
   jack while the hub is powered drops the hub's whole USB side for about
   three seconds, so the serial port goes away and comes back; the AHT20's
   connector does not ([record](../../services/gemini-powerbox-adv3-driver.md#real-hardware-validation)).
4. **Dew modes.** That `>M1x#` is reflected in field 5, that `>Z1x#` is
   reflected in field 4 in Auto and Switch modes, and what `>Z10#` does in
   Manual. *Answered 2026-10-26:* every mode digit and every flag write
   showed in the frame behind it, in Manual too, where the flag changed
   and the duty field did not (the heater's behaviour there was not
   observed; the driver refuses the write in Manual regardless).
5. **Output state across a hub power cycle.** Whether DC2–DC5 and USB come
   back as they were or all on. Not a driver behaviour, but an operator
   fact for this document. *Answered 2026-10-27:* as they were. DC3 and
   USB A off, DEW6 at 50 % in Switch mode and disabled, set before a 14 s
   cut, were all in the first frame after it; the hub also sent nothing on
   its own for the 12 s before the first `>G#`: a power-up disarms the
   stream, which a port close does not.
6. **ConformU**, all four suites, clean, in both dew modes, with the
   version gate first and **both sensors attached**: a `DEWn Mode` write
   to Auto is refused while either sensor is absent, so a run started with
   a sensor still unplugged after item 3 records that refusal as an error.
   *Done 2026-10-26* ([record](../../services/gemini-powerbox-adv3-driver.md#real-hardware-validation)).

## Open items

1. ~~**`DEWn Duty` confirmation**~~ — settled by item 2 above on
   2026-10-26: the field confirms the write in Manual.
2. ~~**The absent-sensor sentinel**~~ — settled by item 3 on 2026-10-27:
   the firmware writes `0`, which is a reading a working sensor gives at
   0 °C, so the rows keep `-127`/`0`.
