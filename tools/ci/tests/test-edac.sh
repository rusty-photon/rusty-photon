#!/bin/bash
# Exercise rp-edac-check.sh against a synthetic EDAC tree.
#
# This script is a monitor, which makes it the awkward kind to test: every
# interesting path runs only when the hardware is unhappy -- ECC switched off
# in firmware, a counter climbing, a reboot mid-series, a counter that cannot
# be parsed -- and none of those can be staged on a healthy host. That is
# exactly why it takes RP_EDAC_ROOT and RP_EDAC_STATE_DIR. A monitor whose
# failure paths have never executed reports nothing either way, and nothing is
# what "fine" looks like.
#
# Fixtures here are SYNTHETIC BY POLICY. This repository is public. Counter
# values, controller counts and state-file contents are host state; invented
# ones exercise the code exactly as well, and there is no version of chasing a
# real memory fault that is improved by pasting this host's readings in here.
#
# The harness stays hermetic: `logger` is stubbed onto PATH so nothing reaches
# a real journal, and both the EDAC tree and the state directory are tmpdirs.
# It touches no sysfs and needs no particular hardware, which is what lets it
# run in `bazel test //...` on a machine with no EDAC at all.
#
# One structural rule holds across every case below: because each one points
# the script at a synthetic tree, every line it logs must carry the test
# identity -- tag `rp-edac-check-test`, and a `[TEST RUN -- ...]` prefix. That
# is asserted on all of them rather than in a single dedicated case, because a
# lone case is deletable and this property is the one that matters most. A
# test run that logs under the production tag manufactures the exact evidence
# this tool exists to provide honestly.
#
# shellcheck disable=SC2034
# The identity cases at the bottom set EDAC_ROOT, STATE, RP_EDAC_ROOT and
# RP_EDAC_STATE_DIR purely as inputs to `set_run_identity`, which is eval'd in
# from the script under test and so is invisible to the linter. Every one of
# them therefore looks unused. Disabling per line would mean the same sentence
# repeated eight times.
set -u -o pipefail

SRC=${1:?path to rp-edac-check.sh}
TMP=$(mktemp -d)
trap 'chmod -R u+w "$TMP" 2>/dev/null; rm -rf "$TMP"' EXIT

# Inherited overrides would silently rewrite what several cases mean -- the
# refusal case below deliberately passes no state override, and an inherited
# one turns it into an ordinary two-override run that exercises nothing, while
# writing its high-water mark to whatever directory the caller had exported.
# Every case sets what it needs explicitly, so nothing here wants the ambient
# value.
unset RP_EDAC_ROOT RP_EDAC_STATE_DIR

# A copy whose *production* state default points inside the tmpdir.
#
# Two cases below must run without a state override, because passing one is
# the very thing they assert is required. Run against the real script that is
# safe only while the guard works -- which is exactly what those cases exist to
# doubt. If the guard is ever removed, the fallback is /var/lib/rp-edac-check,
# and the suite would corrupt the high-water mark of whatever host ran it at
# the moment it discovered the regression. A test for a production-safety guard
# must not itself depend on that guard.
#
# Redirecting the default also buys the stronger assertion: with a path the
# harness owns, the cases can check that no mark was written at all, rather
# than inferring it from what was logged.
PROD_SIM="$TMP/prod-state"
SRC_SIM="$TMP/edac-with-redirected-default.sh"
sed "s#^PROD_STATE_DIR=/var/lib/rp-edac-check\$#PROD_STATE_DIR=$PROD_SIM#" "$SRC" >"$SRC_SIM"

# A rewrite that quietly matched nothing would hand those cases the real
# default back and undo every word above, so it is checked rather than
# assumed, and it stops the run instead of failing a case: at that point
# nothing in this file is safe to execute.
if ! grep -qxF "PROD_STATE_DIR=$PROD_SIM" "$SRC_SIM"; then
    echo "ERROR the harness could not redirect the production state default in $SRC," >&2
    echo "      so the cases that pass no state override would fall back to the real" >&2
    echo "      /var/lib path. Refusing to run. Did PROD_STATE_DIR get renamed?" >&2
    exit 1
fi

# Fake `logger` so the messages are inspectable. It records the tag as well as
# the priority: both are part of what a line claims, and the tag is what the
# runbook tells an operator to grep.
mkdir -p "$TMP/bin"
cat >"$TMP/bin/logger" <<'EOF'
#!/bin/sh
tag=none
prio=info
while [ $# -gt 0 ]; do
    case $1 in
        -t) tag=$2; shift ;;
        -p) prio=${2#daemon.}; shift ;;
        --) shift; break ;;
        *) break ;;
    esac
    shift
done
echo "$tag $prio: $*" >>"$LOGFILE"
EOF
chmod +x "$TMP/bin/logger"
export PATH="$TMP/bin:$PATH"

mkmc() { # mkmc <root> <n> <ce> <ue>
    local d="$1/mc$2"
    mkdir -p "$d"
    echo "$3" >"$d/ce_count"
    echo "$4" >"$d/ue_count"
    echo 0 >"$d/ce_noinfo_count"
    echo 0 >"$d/ue_noinfo_count"
}

FAILED=0
n=0

# Permission-based cases are meaningless as root: root bypasses the mode bits,
# so a directory chmod'ed 500 still accepts a write. Skip loudly rather than
# report a failure that says nothing about the code under test.
AS_ROOT=0
[ "$(id -u)" -eq 0 ] && AS_ROOT=1
skipped_as_root() { echo "SKIP  $1 (running as root: mode bits do not apply)"; }

# The invariant described in the header, applied to whatever was logged.
tagged_as_test() { # tagged_as_test <logfile>
    local line
    while IFS= read -r line; do
        case "$line" in
            "rp-edac-check-test "*"[TEST RUN -- "*) ;;
            *) return 1 ;;
        esac
    done <"$1"
    return 0
}

run() { # run <desc> <expect_rc> <expect_grep|-> [prev_ce prev_ue]
    local desc=$1 exp_rc=$2 exp_re=$3 prev_ce=${4:-} prev_ue=${5:-}
    n=$((n + 1))
    local root="$TMP/root$n" state="$TMP/state$n"
    export LOGFILE="$TMP/log$n"
    : >"$LOGFILE"
    mkdir -p "$state"
    [ -n "$prev_ce" ] && printf '%s %s x\n' "$prev_ce" "$prev_ue" >"$state/high-water"
    RP_EDAC_ROOT="$root" RP_EDAC_STATE_DIR="$state" bash "$SRC" 2>/dev/null
    local rc=$?
    local out
    out=$(cat "$LOGFILE")
    local ok=1 why=""
    [ "$rc" = "$exp_rc" ] || { ok=0; why="rc"; }
    if [ "$exp_re" = "-" ]; then
        [ -z "$out" ] || { ok=0; why="$why message"; }
    else
        grep -qE "$exp_re" <<<"$out" || { ok=0; why="$why message"; }
    fi
    tagged_as_test "$LOGFILE" || { ok=0; why="$why identity"; }
    if [ "$ok" = 1 ]; then
        echo "PASS  $desc (rc=$rc)"
    else
        echo "FAIL  $desc [$why] (rc=$rc, wanted $exp_rc / '$exp_re'); log: ${out:-<empty>}"
        FAILED=1
    fi
}

# 1. ECC gone entirely -- the branch a BIOS update could create, and the one
#    whose absence looks identical to a clean bill of health.
run "no EDAC controllers -> err and non-zero" 1 "err: .*no EDAC memory controllers"

# 2. Healthy and quiet. Silence is the design, so it is worth pinning down.
mkmc "$TMP/root2" 0 0 0
mkmc "$TMP/root2" 1 0 0
run "healthy, no prior state -> silent" 0 "-"

# 3. Correctable errors climbing, summed across controllers.
mkmc "$TMP/root3" 0 3 0
mkmc "$TMP/root3" 1 2 0
run "ce 0 -> 5 -> correctable warning" 0 "err: .*correctable memory errors: 5 total, up 5" 0 0

# 4. Uncorrectable takes precedence and says the harsher thing.
mkmc "$TMP/root4" 0 5 1
mkmc "$TMP/root4" 1 0 0
run "ue 0 -> 1 -> UNCORRECTABLE warning" 0 "err: .*UNCORRECTABLE memory errors: 1 total" 0 0

# 5. Reboot: counters drop. Must re-baseline at info, not alarm at err --
#    alerting on every reboot is how a reader learns to ignore the tag.
mkmc "$TMP/root5" 0 0 0
mkmc "$TMP/root5" 1 0 0
run "counters reset -> info re-baseline, no err" 0 "info: .*counters reset" 99 3
# "no err" is half of that case's contract and the regex above cannot express
# it: a re-baseline that ALSO escalated would still match. LOGFILE still names
# the case's own log here, so assert the absence directly. This is the
# direction that matters -- escalating on every reboot is how a reader learns
# to ignore the tag, and then a real error scrolls past unread.
if grep -q " err: " "$LOGFILE"; then
    echo "FAIL  counters reset must re-baseline without escalating; log: $(cat "$LOGFILE")"
    FAILED=1
fi

# 6. Steady state between checks.
mkmc "$TMP/root6" 0 4 0
mkmc "$TMP/root6" 1 0 0
run "ce steady at 4 -> silent" 0 "-" 4 0

# 7. noinfo counts are included: errors the driver could not attribute to a
#    rank are still errors, and dropping them under-reports precisely when the
#    hardware is confused enough to stop localising faults.
mkmc "$TMP/root7" 0 0 0
mkmc "$TMP/root7" 1 0 0
echo 7 >"$TMP/root7/mc0/ce_noinfo_count"
run "unattributed ce counted -> warning of 7" 0 "err: .*up 7 since" 0 0

# 8. A truncated state file must not silence escalation. With an empty second
#    field every comparison errors out instead of evaluating, the enclosing
#    `if` reads that as false, and a real uncorrectable error goes unannounced.
n=$((n + 1))
root="$TMP/root$n"
state="$TMP/state$n"
mkmc "$root" 0 0 1
mkdir -p "$state"
printf '5\n' >"$state/high-water" # one field only
export LOGFILE="$TMP/log$n"
: >"$LOGFILE"
RP_EDAC_ROOT="$root" RP_EDAC_STATE_DIR="$state" bash "$SRC" 2>/dev/null
rc=$?
if [ "$rc" = 0 ] && grep -q "err: .*UNCORRECTABLE" "$LOGFILE" && tagged_as_test "$LOGFILE"; then
    echo "PASS  truncated state file -> still escalates (rc=$rc)"
else
    echo "FAIL  truncated state file (rc=$rc); log: $(cat "$LOGFILE")"
    FAILED=1
fi

# 9. Unable to persist -> says so and exits non-zero, rather than looking fine.
#    A check that cannot keep its own state compares against a stale mark on
#    every later run, which is survivable, but doing it silently is not.
n=$((n + 1))
if [ "$AS_ROOT" = 1 ]; then
    skipped_as_root "unwritable state dir -> err and rc=1"
else
    root="$TMP/root$n"
    state="$TMP/state$n"
    mkmc "$root" 0 0 0
    mkdir -p "$state"
    chmod 500 "$state"
    export LOGFILE="$TMP/log$n"
    : >"$LOGFILE"
    RP_EDAC_ROOT="$root" RP_EDAC_STATE_DIR="$state" bash "$SRC" 2>/dev/null
    rc=$?
    chmod 700 "$state"
    if [ "$rc" = 1 ] && grep -q "err: .*could not persist" "$LOGFILE" && tagged_as_test "$LOGFILE"; then
        echo "PASS  unwritable state dir -> err and rc=1 (rc=$rc)"
    else
        echo "FAIL  unwritable state dir (rc=$rc); log: $(cat "$LOGFILE")"
        FAILED=1
    fi
fi

# 10-12. An unusable counter must warn in this script's own voice and exit 1,
#        rather than dying on a raw bash arithmetic or `set -u` error that
#        lands nothing at err priority and leaves the unit failed where nobody
#        is watching.
for bad in "" "abc" "12abc"; do
    n=$((n + 1))
    root="$TMP/root_bad$n"
    state="$TMP/state_bad$n"
    mkmc "$root" 0 0 0
    printf '%s\n' "$bad" >"$root/mc0/ce_count"
    mkdir -p "$state"
    export LOGFILE="$TMP/log_bad$n"
    : >"$LOGFILE"
    RP_EDAC_ROOT="$root" RP_EDAC_STATE_DIR="$state" bash "$SRC" 2>/dev/null
    rc=$?
    if [ "$rc" = 1 ] && grep -q "err: .*unreadable or not a number" "$LOGFILE" && tagged_as_test "$LOGFILE"; then
        echo "PASS  counter value '$bad' -> err and rc=1"
    else
        echo "FAIL  counter value '$bad' (rc=$rc); log: $(cat "$LOGFILE")"
        FAILED=1
    fi
done

# 13. Controllers present but none readable -- a distinct alarm from case 1,
#     and reachable only this way: the root exists and matches `mc[0-9]*`, so
#     the earlier guard passes, and every entry is then skipped for having no
#     readable ce_count/ue_count. Without a fixture that stops between those
#     two gates, a regression in the second alarm leaves the suite green.
n=$((n + 1))
root="$TMP/root$n"
state="$TMP/state$n"
mkdir -p "$root/mc0" "$state" # an mc entry with no counter files in it
export LOGFILE="$TMP/log$n"
: >"$LOGFILE"
RP_EDAC_ROOT="$root" RP_EDAC_STATE_DIR="$state" bash "$SRC" 2>/dev/null
rc=$?
if [ "$rc" = 1 ] && grep -q "err: .*exposed no readable controllers" "$LOGFILE" && tagged_as_test "$LOGFILE"; then
    echo "PASS  controllers present but unreadable -> err and rc=1 (rc=$rc)"
else
    echo "FAIL  controllers present but unreadable (rc=$rc); log: $(cat "$LOGFILE")"
    FAILED=1
fi

# 14. A fixture root with no state override must be refused outright, because
#     the two default independently: the run would read synthetic counters and
#     then write them over the production high-water mark, which is silent and
#     unrecoverable -- every later reading sits below the synthetic mark, reads
#     as a drop, and re-baselines without a word.
#
#     Asserting the refusal message is not enough on its own: a future edit
#     that moved the guard below the persist step would still produce it,
#     after doing the damage. Two things rule that out. The fixture carries a
#     count high enough to force a warning, so the absence of that warning
#     shows the run stopped before it compared anything; and because this runs
#     against the redirected-default copy, the mark it would have written is a
#     path the harness owns and can check never appeared.
n=$((n + 1))
root="$TMP/root$n"
mkmc "$root" 0 99 0
export LOGFILE="$TMP/log$n"
: >"$LOGFILE"
rm -rf "$PROD_SIM"
RP_EDAC_ROOT="$root" bash "$SRC_SIM" 2>/dev/null
rc=$?
if [ "$rc" = 1 ] &&
    grep -q "err: .*RP_EDAC_ROOT is set without RP_EDAC_STATE_DIR" "$LOGFILE" &&
    ! grep -q "correctable memory errors" "$LOGFILE" &&
    [ ! -e "$PROD_SIM/high-water" ] &&
    tagged_as_test "$LOGFILE"; then
    echo "PASS  fixture root without a state override -> refused, no mark written (rc=$rc)"
else
    echo "FAIL  fixture root without a state override (rc=$rc, mark exists: $([ -e "$PROD_SIM/high-water" ] && echo yes || echo no)); log: $(cat "$LOGFILE")"
    FAILED=1
fi

# 15. Naming the production state directory outright is the other way into the
#     same damage, and it is the more plausible thing to type on the host --
#     "point it at the real state so I can see what the last run compared
#     against" -- which sounds like a read and is not. Refused as well.
#
#     The second form checks that the comparison is on resolved paths rather
#     than literal ones, so `/.` and a trailing slash do not walk past it.
for prod_form in "$PROD_SIM" "$PROD_SIM/./"; do
    n=$((n + 1))
    if [ "$prod_form" != "$PROD_SIM" ] && ! command -v realpath >/dev/null; then
        echo "SKIP  equivalent path form is refused (no realpath on this host)"
        continue
    fi
    root="$TMP/root$n"
    mkmc "$root" 0 99 0
    export LOGFILE="$TMP/log$n"
    : >"$LOGFILE"
    rm -rf "$PROD_SIM"
    mkdir -p "$PROD_SIM"
    printf '5 0 baseline\n' >"$PROD_SIM/high-water"
    RP_EDAC_ROOT="$root" RP_EDAC_STATE_DIR="$prod_form" bash "$SRC_SIM" 2>/dev/null
    rc=$?
    if [ "$rc" = 1 ] &&
        grep -q "err: .*resolves to the production state directory" "$LOGFILE" &&
        [ "$(cat "$PROD_SIM/high-water")" = "5 0 baseline" ] &&
        tagged_as_test "$LOGFILE"; then
        echo "PASS  state dir '$prod_form' pointing at production -> refused, mark intact (rc=$rc)"
    else
        echo "FAIL  state dir '$prod_form' pointing at production (rc=$rc, mark now: $(cat "$PROD_SIM/high-water" 2>/dev/null)); log: $(cat "$LOGFILE")"
        FAILED=1
    fi
done
rm -rf "$PROD_SIM"

# 17-21. The identity itself, decided before anything is read.
#
# The production branch cannot be reached by running the script -- with no
# override it reads the real /sys, and whether this machine has EDAC at all is
# not something a test may depend on. So the function is lifted out and called
# directly, which is the only way to assert the case that matters most: that
# an unconfigured run still logs as `rp-edac-check` with no prefix. Without
# that assertion, renaming the production tag would pass every case above.
#
# Lifted with `awk` rather than sourced, because sourcing runs the whole check.
unset RP_EDAC_ROOT RP_EDAC_STATE_DIR
eval "$(awk '/^set_run_identity\(\) \{/,/^\}/' "$SRC")"

identity() { # identity <desc> <want_tag> <want_prefix_substring|-> [root] [statedir]
    local desc=$1 want_tag=$2 want_pre=$3
    n=$((n + 1))
    TAG="(none)"
    MSG_PREFIX="(none)"
    EDAC_ROOT=${4:-/sys/devices/system/edac/mc}
    STATE=${5:-/var/lib/rp-edac-check}/high-water
    if [ -n "${4:-}" ]; then RP_EDAC_ROOT=$4; else unset RP_EDAC_ROOT; fi
    if [ -n "${5:-}" ]; then RP_EDAC_STATE_DIR=$5; else unset RP_EDAC_STATE_DIR; fi
    set_run_identity
    local ok=1
    [ "$TAG" = "$want_tag" ] || ok=0
    if [ "$want_pre" = "-" ]; then
        [ -z "$MSG_PREFIX" ] || ok=0
    else
        case "$MSG_PREFIX" in *"$want_pre"*) ;; *) ok=0 ;; esac
    fi
    if [ "$ok" = 1 ]; then
        echo "PASS  $desc"
    else
        echo "FAIL  $desc (tag=$TAG wanted=$want_tag; prefix=[$MSG_PREFIX] wanted=[$want_pre])"
        FAILED=1
    fi
}

# 17. THE production case: no overrides, no prefix, the tag the runbook greps.
identity "no overrides -> production tag, no prefix" rp-edac-check -

# 18. A synthetic tree: the numbers are not this host's at all.
identity "synthetic EDAC root -> test tag names the root" \
    rp-edac-check-test "reading /fixture/mc, not this host's memory" /fixture/mc

# 19. Real counters, synthetic baseline. The numbers are this host's and the
#     delta is not, so the prefix must not claim the readings are fake.
identity "redirected state dir -> test tag names the baseline" \
    rp-edac-check-test "measuring against /fixture/state/high-water, not this host's baseline" \
    "" /fixture/state

# 20. Both, as every case above sets them: both reasons are named.
n=$((n + 1))
TAG="(none)"
MSG_PREFIX="(none)"
EDAC_ROOT=/fixture/mc
STATE=/fixture/state/high-water
RP_EDAC_ROOT=/fixture/mc
RP_EDAC_STATE_DIR=/fixture/state
set_run_identity
both=0
case "$MSG_PREFIX" in
    *"not this host's memory"*"not this host's baseline"*) both=1 ;;
esac
if [ "$TAG" = rp-edac-check-test ] && [ "$both" = 1 ]; then
    echo "PASS  both overrides -> both reasons named"
else
    echo "FAIL  both overrides -> both reasons named (tag=$TAG prefix=[$MSG_PREFIX])"
    FAILED=1
fi

# 21. An empty override is a production run, because `${RP_EDAC_ROOT:-...}`
#     falls back to the default for one. The identity has to agree with the
#     path that is actually read, or an empty variable would silently downgrade
#     the real check to a test-tagged one and hide a genuine fault.
n=$((n + 1))
TAG="(none)"
MSG_PREFIX="(none)"
EDAC_ROOT=/sys/devices/system/edac/mc
STATE=/var/lib/rp-edac-check/high-water
RP_EDAC_ROOT=""
RP_EDAC_STATE_DIR=""
set_run_identity
if [ "$TAG" = rp-edac-check ] && [ -z "$MSG_PREFIX" ]; then
    echo "PASS  empty overrides -> production tag, no prefix"
else
    echo "FAIL  empty overrides -> production tag, no prefix (tag=$TAG prefix=[$MSG_PREFIX])"
    FAILED=1
fi

echo "ran $n case(s)"
exit "$FAILED"
