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

# Every counter is validated before it reaches arithmetic, and an unusable one
# ends the run in this script's own voice.
#
# Feeding a malformed value straight to $(( )) is not a soft failure here: an
# empty file raises an arithmetic syntax error, a non-numeric one trips `set -u`
# as an unbound variable, and either way the shell dies mid-loop with a raw
# bash message and nothing at err priority. The unit is then marked failed --
# which nobody is watching, since this host has no notification target. That is
# the same delivery gap this script exists to close, so a counter that cannot
# be read is announced exactly the way a memory error would be.
#
# Result goes through a global rather than a command substitution on purpose:
# `exit` inside $( ) leaves only the subshell, so the failure would be swallowed
# and the malformed value would reach the arithmetic anyway.
COUNTER=0
read_counter() {
    local path=$1 value
    IFS= read -r value <"$path" 2>/dev/null || value=""
    case "$value" in
        '' | *[!0-9]*)
            warn "memory error counter $path is unreadable or not a number ('$value'). ECC counts cannot be trusted this run, so nothing is watching the memory until this is fixed."
            exit 1 ;;
    esac
    COUNTER=$value
}

ce=0
ue=0
controllers=0
for mc in "$EDAC_ROOT"/mc[0-9]*; do
    # Spelled out rather than `A && B || continue`: that form reads as
    # if-then-else and is not one, so it is worth avoiding in a loop whose
    # skip condition decides whether a controller is counted at all.
    if [ ! -r "$mc/ce_count" ] || [ ! -r "$mc/ue_count" ]; then
        continue
    fi
    controllers=$((controllers + 1))
    read_counter "$mc/ce_count"
    ce=$((ce + COUNTER))
    read_counter "$mc/ue_count"
    ue=$((ue + COUNTER))
    # noinfo counts are errors the driver could not attribute to a rank. They
    # are still errors, and omitting them would under-report precisely when
    # the hardware is confused enough to stop localising faults.
    if [ -r "$mc/ce_noinfo_count" ]; then
        read_counter "$mc/ce_noinfo_count"
        ce=$((ce + COUNTER))
    fi
    if [ -r "$mc/ue_noinfo_count" ]; then
        read_counter "$mc/ue_noinfo_count"
        ue=$((ue + COUNTER))
    fi
done

if [ "$controllers" -eq 0 ]; then
    warn "EDAC is present but exposed no readable controllers; memory error counts are unavailable"
    exit 1
fi

mkdir -p "$STATE_DIR"

# Each field is validated on its own, and an empty one counts as invalid.
# Testing the two concatenated cannot tell "5 and nothing" from "5", so a
# truncated state file passed the check and left a comparison operand empty --
# and `[ 7 -gt "" ]` does not evaluate false, it errors, which the surrounding
# `if` reads as false. Both the re-baseline and the escalation then go quiet.
# A monitor that has silently stopped escalating is indistinguishable from a
# host with healthy memory, which is the one outcome this script must never
# produce.
prev_ce=""
prev_ue=""
if [ -r "$STATE" ]; then
    read -r prev_ce prev_ue _ <"$STATE" 2>/dev/null || true
fi
case "$prev_ce" in '' | *[!0-9]*) prev_ce=0 ;; esac
case "$prev_ue" in '' | *[!0-9]*) prev_ue=0 ;; esac

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

# Written to a temp file and published by rename, for the reason the parsing
# above now defends against: a half-written state file is not a slightly stale
# baseline, it is the input that silences the next run. Rename within one
# directory is atomic, so a reader sees the old mark or the new one.
#
# A failure to persist is reported and exits non-zero, which surfaces in
# `systemctl status rp-edac-check`. It is the safe direction on its own -- the
# next run compares against an older, lower mark and so over-reports rather
# than under-reports -- but a check that cannot keep its own state is not
# quietly fine, and the whole point here is to stop treating silence as health.
if printf '%s %s %s\n' "$ce" "$ue" "$(date -Is)" >"$STATE.tmp" &&
    mv -f "$STATE.tmp" "$STATE"; then
    exit 0
fi
rm -f "$STATE.tmp"
warn "could not persist the ECC high-water mark to $STATE (counters read fine: ce=$ce ue=$ue). Escalation still works, but compares against a stale mark, so a rising count may be re-announced and a drop may read as a reboot."
exit 1
