#!/bin/bash
# In-Band ECC watch for the runner-pool hypervisor.
#
# The host runs non-ECC-module DDR5 with Intel In-Band ECC (IBECC) enabled in
# firmware, which the kernel exposes through igen6_edac as ordinary EDAC
# memory controllers running in SECDED: single-bit errors are corrected and
# counted, double-bit errors are detected. That makes memory faults visible --
# but only to whoever reads the counters, and nothing read them. A silent
# correctable-error climb is exactly the shape of evidence that would have
# settled whether a kernel memory-corruption oops came from the DIMMs, and it
# was unavailable after the fact because the counters reset at every boot.
#
# So this runs periodically and escalates on change rather than on state:
# steady counters say nothing, a counter that MOVED is the signal.
#
# Delivery is deliberately journal-only, at err priority. The host has no
# notification target configured -- postfix has no relayhost, so mail to root
# lands in a spool nobody opens -- and inventing one here would be a worse lie
# than the honest limitation: this makes a fault visible to anyone who looks
# (`journalctl -p err`, and the syslog pane in the PVE web UI), it does not
# page anyone. Wire a real target and this becomes real alerting; until then
# treat it as a black box recorder, not a smoke alarm.
set -u -o pipefail

TAG=rp-edac-check

# Overridable so the branches can actually be tested. Every interesting path
# here is one that only runs when the hardware is unhappy -- ECC turned off,
# counters climbing, a reboot mid-series -- and none of those can be staged on
# a healthy host. A monitor whose failure paths have never executed is the
# worst kind: it reports nothing either way, and nothing is what "fine" looks
# like. Production passes neither variable and gets the defaults.
STATE_DIR=${RP_EDAC_STATE_DIR:-/var/lib/rp-edac-check}
STATE="$STATE_DIR/high-water"
EDAC_ROOT=${RP_EDAC_ROOT:-/sys/devices/system/edac/mc}

note() { logger -t "$TAG" -p daemon.info -- "$*"; }
warn() { logger -t "$TAG" -p daemon.err -- "$*"; }

# Losing IBECC is itself a reportable event, not a reason to stay quiet. A
# firmware update that resets setup defaults, or a BIOS whose ECC option got
# cleared, takes the controllers away entirely -- and the failure mode of a
# silent check is that it looks identical to a clean bill of health. Say so.
if [ ! -d "$EDAC_ROOT" ] || ! compgen -G "$EDAC_ROOT/mc[0-9]*" >/dev/null; then
    warn "no EDAC memory controllers present: In-Band ECC is not active, so single-bit memory faults are no longer detected or counted. Check the firmware ECC setting (it is cleared by a BIOS update) and that igen6_edac loaded."
    exit 1
fi

ce=0
ue=0
controllers=0
for mc in "$EDAC_ROOT"/mc[0-9]*; do
    [ -r "$mc/ce_count" ] && [ -r "$mc/ue_count" ] || continue
    controllers=$((controllers + 1))
    ce=$((ce + $(cat "$mc/ce_count")))
    ue=$((ue + $(cat "$mc/ue_count")))
    # noinfo counts are errors the driver could not attribute to a rank. They
    # are still errors, and omitting them would under-report precisely when
    # the hardware is confused enough to stop localising faults.
    [ -r "$mc/ce_noinfo_count" ] && ce=$((ce + $(cat "$mc/ce_noinfo_count")))
    [ -r "$mc/ue_noinfo_count" ] && ue=$((ue + $(cat "$mc/ue_noinfo_count")))
done

if [ "$controllers" -eq 0 ]; then
    warn "EDAC is present but exposed no readable controllers; memory error counts are unavailable"
    exit 1
fi

mkdir -p "$STATE_DIR"
prev_ce=0
prev_ue=0
if [ -r "$STATE" ]; then
    read -r prev_ce prev_ue _ <"$STATE" 2>/dev/null || { prev_ce=0; prev_ue=0; }
    case "$prev_ce$prev_ue" in *[!0-9]*) prev_ce=0; prev_ue=0 ;; esac
fi

# EDAC counters are cumulative since boot, so a drop means the host rebooted
# rather than that errors were undone. Re-baseline silently: alerting on a
# reboot would train the reader to ignore this tag, which is how a real
# escalation gets missed.
if [ "$ce" -lt "$prev_ce" ] || [ "$ue" -lt "$prev_ue" ]; then
    note "counters reset (ce $prev_ce -> $ce, ue $prev_ue -> $ue); re-baselining after what looks like a reboot"
    prev_ce=0
    prev_ue=0
fi

if [ "$ue" -gt "$prev_ue" ]; then
    warn "UNCORRECTABLE memory errors: $ue total, up $((ue - prev_ue)) since the last check across $controllers controller(s). SECDED detected but could not correct these; the affected data was wrong. Treat this host as unreliable until the DIMMs are tested or replaced."
elif [ "$ce" -gt "$prev_ce" ]; then
    warn "correctable memory errors: $ce total, up $((ce - prev_ce)) since the last check across $controllers controller(s). SECDED corrected these, so nothing was lost yet -- but a count that climbs is a DIMM degrading, and it is the early warning an uncorrectable error does not give you."
fi

printf '%s %s %s\n' "$ce" "$ue" "$(date -Is)" >"$STATE"
