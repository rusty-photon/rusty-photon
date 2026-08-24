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

# Overridable so the branches can actually be tested. Every interesting path
# here is one that only runs when the hardware is unhappy -- ECC turned off,
# counters climbing, a reboot mid-series -- and none of those can be staged on
# a healthy host. A monitor whose failure paths have never executed is the
# worst kind: it reports nothing either way, and nothing is what "fine" looks
# like. Production passes neither variable and gets the defaults.
PROD_STATE_DIR=/var/lib/rp-edac-check
PROD_EDAC_ROOT=/sys/devices/system/edac/mc
STATE_DIR=${RP_EDAC_STATE_DIR:-$PROD_STATE_DIR}
STATE="$STATE_DIR/high-water"
EDAC_ROOT=${RP_EDAC_ROOT:-$PROD_EDAC_ROOT}

# Whether the mark this run reads and rewrites is production's own: yes (0),
# no (1), or cannot be established (2). Everything below turns on it, and the
# three-way answer is deliberate -- "could not tell" is not "no".
#
# The literal comparison first is what keeps an ordinary production run
# working on a host with no realpath: with no override the two names are the
# same string, so the common case resolves nothing and cannot fail. Only a run
# that passed a state directory gets as far as needing the tool.
state_is_production() {
    [ "$STATE_DIR" = "$PROD_STATE_DIR" ] && return 0
    local here there
    if ! here=$(realpath -m -- "$STATE_DIR" 2>/dev/null) ||
        ! there=$(realpath -m -- "$PROD_STATE_DIR" 2>/dev/null); then
        return 2
    fi
    [ "$here" = "$there" ]
}
state_is_production
STATE_IS_PROD=$?

# The same question about the counters, and it has to be asked the same way
# rather than inferred from whether an override was passed. An override naming
# the production tree reads this host's real memory, so a run that treats mere
# presence as proof of a fixture disclaims real errors as synthetic -- the
# reading is genuine and the line says it is not. Of the two directions this
# script can lie in, that is the one that loses a fault outright.
root_is_production() {
    [ "$EDAC_ROOT" = "$PROD_EDAC_ROOT" ] && return 0
    local here there
    if ! here=$(realpath -m -- "$EDAC_ROOT" 2>/dev/null) ||
        ! there=$(realpath -m -- "$PROD_EDAC_ROOT" 2>/dev/null); then
        return 2
    fi
    [ "$here" = "$there" ]
}
root_is_production
ROOT_IS_PROD=$?

# A run pointed somewhere synthetic must not be mistakable for the run that
# reads this host's memory, because this tag's whole contract is "steady
# counters say nothing, so a line here is a real event". A test run logged
# under the production tag inverts that: it manufactures exactly the evidence
# the check exists to provide honestly, at err priority, on a host whose
# memory is fine -- and it outlives its fixture, because the counters reset at
# boot but the journal entry does not. The reader it fools is the one this was
# built for: an operator grepping this tag after an unexplained reboot,
# already inclined to believe a memory fault.
#
# So the identity is derived from the overrides rather than fixed. There are
# two of them and two different false impressions to head off: a synthetic
# EDAC root means the numbers are not this host's at all, while a redirected
# state directory means the numbers are real but the delta is measured against
# a baseline that is not. Name whichever applies, because a reader who found
# the line by priority rather than by tag still has to be able to tell.
#
# An empty override is a production run, matching the `:-` defaults above.
#
# So is a state override that lands on the production directory, which is the
# subtler half. Passing RP_EDAC_STATE_DIR=/var/lib/rp-edac-check with no root
# override reads this host's real counters and rewrites its real mark: that is
# a production run in every respect that matters, however it was spelled.
# Calling it a test would invert the failure this tagging exists to prevent --
# instead of a fixture masquerading as production, a genuine rising error
# count would be filed under the test tag, where the runbook's grep never
# looks. Same operator, same missed fault, opposite direction.
set_run_identity() {
    local why=""
    if [ -n "${RP_EDAC_ROOT:-}" ] && [ "$ROOT_IS_PROD" -ne 0 ]; then
        if [ "$ROOT_IS_PROD" -eq 2 ]; then
            why="unable to tell whether $EDAC_ROOT is this host's own memory"
        else
            why="reading $EDAC_ROOT, not this host's memory"
        fi
    fi
    if [ -n "${RP_EDAC_STATE_DIR:-}" ] && [ "$STATE_IS_PROD" -ne 0 ]; then
        if [ -n "$why" ]; then
            why="$why; "
        fi
        if [ "$STATE_IS_PROD" -eq 2 ]; then
            why="${why}unable to tell whether $STATE_DIR is this host's own baseline"
        else
            why="${why}measuring against $STATE, not this host's baseline"
        fi
    fi
    if [ -z "$why" ]; then
        TAG=rp-edac-check
        MSG_PREFIX=""
        return 0
    fi
    TAG=rp-edac-check-test
    MSG_PREFIX="[TEST RUN -- $why] "
}
set_run_identity

note() { logger -t "$TAG" -p daemon.info -- "$MSG_PREFIX$*"; }
warn() { logger -t "$TAG" -p daemon.err -- "$MSG_PREFIX$*"; }

# The journal is only half the evidence; the high-water mark is the other half,
# and it defaults independently of the EDAC root. So a run given a fixture tree
# and nothing else reads synthetic counters and then writes them over the real
# mark -- correctly tagged as a test the whole time, and still destroying the
# baseline every later run is measured against. What follows is worse than the
# wrong line this tagging was added to prevent, because it is silent: the next
# real reading sits below the synthetic mark, so it reads as a drop, and a drop
# is a reboot, so the check quietly re-baselines and says nothing. Nothing
# afterwards distinguishes a corrupted mark from an honest one.
#
# Refusing is the only safe answer. The pairing is cheap to satisfy and there
# is no case for reading a fixture while keeping production's baseline.
#
# The reverse pairing is fine and stays allowed: a redirected state directory
# with the real EDAC root reads this host's true counters against a scratch
# baseline, which touches nothing production owns.

# An unresolvable state directory ends the run whether or not a fixture root
# came with it. Which identity is honest depends on that answer, so without it
# this run cannot say truthfully what it is, and reporting what it cannot
# establish is the one thing this script must never do. Production is not
# reachable here: with no override the two names are the same string and
# resolve nothing.
if [ "$ROOT_IS_PROD" -eq 2 ]; then
    warn "refusing to run: $EDAC_ROOT could not be resolved to tell whether it is this host's own EDAC tree at $PROD_EDAC_ROOT. That decides whether this run's output may describe itself as this host's memory, and a wrong answer either disclaims a real fault or invents one, so it is not guessed at. This needs realpath, from coreutils."
    exit 1
fi
if [ "$STATE_IS_PROD" -eq 2 ]; then
    warn "refusing to run: $STATE_DIR could not be resolved to tell whether it is the production state directory $PROD_STATE_DIR. That decides both whether this run may write there and whether its output belongs under the production tag, so neither is guessed at. This needs realpath, from coreutils."
    exit 1
fi

# Omitting the override is the likely mistake; naming the production directory
# outright is the other one, and it ends in the same place. The second is a
# plausible thing to type on the host -- "point it at the real state so I can
# reproduce what the last run saw" -- which reads as harmless and is not,
# because this run does not only read that file, it replaces it. Both are the
# same condition once the paths are resolved, so they are one check with two
# messages, the wording following whichever mistake was made.
#
# Resolved rather than literal, so `/var/lib/rp-edac-check/.`, a trailing
# slash or a symlinked parent do not walk straight past. That covers the ways
# a person arrives here by accident, which is the whole scope claimed: a bind
# mount defeats any comparison of paths, as does swapping the directory after
# the check, and no amount of normalising would make this a defence against
# someone trying.
#
# Keyed on the root actually being a fixture rather than on an override having
# been passed, because the danger is synthetic counters reaching the real mark,
# and "an override was passed" was only ever a proxy for that. Naming the
# production tree explicitly alongside the production state directory is an
# ordinary production run spelled the long way, and there is nothing to refuse.
if [ "$ROOT_IS_PROD" -ne 0 ] && [ "$STATE_IS_PROD" -eq 0 ]; then
    if [ -z "${RP_EDAC_STATE_DIR:-}" ]; then
        warn "refusing to run: RP_EDAC_ROOT is set without RP_EDAC_STATE_DIR, so this would read counters from $EDAC_ROOT and then overwrite the production high-water mark at $STATE with them, leaving every later run measuring against a synthetic baseline. Set both, or neither."
    else
        warn "refusing to run: RP_EDAC_ROOT points at $EDAC_ROOT while RP_EDAC_STATE_DIR resolves to the production state directory $PROD_STATE_DIR, so the fixture's counters would replace the real high-water mark. This run reads that file and then overwrites it. Point the state directory somewhere scratch."
    fi
    exit 1
fi

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
