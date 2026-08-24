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
# The harness needs no Proxmox host, no sysfs and no particular hardware,
# which is what lets it run in `bazel test //...` on a machine with no EDAC at
# all: `logger` is stubbed onto PATH so nothing reaches a real journal, and
# both the EDAC tree and the state directory are tmpdirs.
#
# It is not free of the host entirely, and saying otherwise would be the same
# kind of overclaim these cases exist to catch. Every fixture run compares
# resolved paths, so coreutils `realpath` is a real dependency -- of the
# script first, which refuses without it by design, and therefore of this
# file. It is checked once up front rather than left to surface as twenty
# cases failing with a message about EDAC trees. Not stubbed: the normalising
# is the behaviour under test, and a hand-rolled stand-in would be testing
# itself.
#
# One structural rule holds across every case that hands the script an
# override: because those point it at a synthetic tree, every line each one
# logs must carry the test identity -- tag `rp-edac-check-test`, and a
# `[TEST RUN -- ...]` prefix. That is asserted on all of them rather than in a
# single dedicated case, because a lone case is deletable and this property is
# the one that matters most. A test run that logs under the production tag
# manufactures the exact evidence this tool exists to provide honestly.
#
# The production-identity cases at the end are the mirror and hold the
# opposite rule: what they hand the script resolves to what production reads,
# so every line must carry `rp-edac-check` with no marker anywhere. Both
# directions need asserting, because each one hides a real fault from the
# reader who goes looking for it -- a fixture filed as production invents
# evidence, and a real reading filed as a test discards it.
#
# shellcheck disable=SC2034
# The identity cases at the bottom set EDAC_ROOT, STATE, STATE_DIR, PROD_STATE_DIR,
# RP_EDAC_ROOT and RP_EDAC_STATE_DIR purely as inputs to the two functions
# eval'd in from the script under test, which are invisible to the linter. Every
# one of them therefore looks unused. Disabling per line would mean the same
# sentence repeated a dozen times.
set -u -o pipefail

SRC=${1:?path to rp-edac-check.sh}
TMP=$(mktemp -d)
trap 'chmod -R u+w "$TMP" 2>/dev/null; rm -rf "$TMP"' EXIT

# The dependency named in the header, asserted rather than assumed. `-m` is
# the part that matters and is not universal -- busybox's realpath has no such
# flag -- so the check exercises it instead of merely looking for the binary.
if ! realpath -m -- / >/dev/null 2>&1; then
    echo "ERROR realpath -m does not work here, and every fixture case below needs it:" >&2
    echo "      the script compares resolved paths and refuses when it cannot, so those" >&2
    echo "      cases would fail with a message about EDAC trees that has nothing to do" >&2
    echo "      with what broke. Install coreutils. Refusing to run." >&2
    exit 1
fi

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

# A copy with BOTH production defaults inside the tmpdir, run with no
# overrides at all.
#
# Everything else here passes RP_EDAC_ROOT, so `note` and `warn` are only ever
# watched emitting the test identity, and the identity cases below call
# set_run_identity without going near them. Between those two facts the
# production logging path is never executed: hard-code the test tag into
# `warn`, or drop MSG_PREFIX from it, and every case still passes while every
# real escalation on the host lands under a tag the runbook does not grep.
# That is this script's original bug with the arrow reversed, and it would
# have shipped invisibly.
#
# Redirecting the EDAC default as well as the state one is what makes the
# unconfigured run testable: it reads a fixture tree while believing it is
# production, which is the only way to watch production behaviour on a machine
# that may have no EDAC at all.
PROD_MC="$TMP/prod-mc"
SRC_PROD="$TMP/edac-production-simulated.sh"
sed -e "s#^PROD_STATE_DIR=/var/lib/rp-edac-check\$#PROD_STATE_DIR=$PROD_SIM#" \
    -e "s#^PROD_EDAC_ROOT=/sys/devices/system/edac/mc\$#PROD_EDAC_ROOT=$PROD_MC#" \
    "$SRC" >"$SRC_PROD"
if ! grep -qxF "PROD_EDAC_ROOT=$PROD_MC" "$SRC_PROD"; then
    echo "ERROR the harness could not redirect the production EDAC default in $SRC," >&2
    echo "      so the production-identity cases would read this machine's real" >&2
    echo "      counters. Refusing to run. Did PROD_EDAC_ROOT get renamed?" >&2
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
#
# Both priorities are spelled out, and the marker is anchored immediately
# after the one the line carries, so nothing may appear between them. A
# pattern with a wildcard in that gap accepts the prefix anywhere in the line
# -- including appended to the end of the message, which passes while
# defeating the point of having a prefix. Being first is the property: a
# reader scanning err lines has to see it before the sentence that alarms
# them, not after it. Nothing else in the file would notice that move.
#
# Listing err and info rather than matching any word also means a third
# priority has to be introduced here deliberately instead of arriving unseen.
tagged_as_test() { # tagged_as_test <logfile>
    local line
    while IFS= read -r line; do
        case "$line" in
            "rp-edac-check-test err: [TEST RUN -- "*) ;;
            "rp-edac-check-test info: [TEST RUN -- "*) ;;
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
    if ! command -v realpath >/dev/null; then
        echo "SKIP  state dir '$prod_form' pointing at production (no realpath here; the guard fails closed instead, which case 17 covers)"
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

# 17. When the ROOT cannot be resolved, the guard must refuse rather than fall
#     back to comparing them as text. Text comparison answers wrongly in the
#     direction that costs something -- `<prod>/.` is not `<prod>` as a string,
#     so the fixture would be waved through to overwrite the mark -- and a
#     check that says yes because it could not tell is the whole defect class
#     these guards exist to close.
#
#     The state directory here is scratch, so under the old fallback this run
#     would have been allowed and would have persisted. Refusing it is the
#     cost of failing closed, and is the point of the case.
n=$((n + 1))
mkdir -p "$TMP/badbin"
printf '#!/bin/sh\nexit 1\n' >"$TMP/badbin/realpath"
chmod +x "$TMP/badbin/realpath"
root="$TMP/root$n"
state="$TMP/state$n"
mkmc "$root" 0 99 0
mkdir -p "$state"
export LOGFILE="$TMP/log$n"
: >"$LOGFILE"
PATH="$TMP/badbin:$PATH" RP_EDAC_ROOT="$root" RP_EDAC_STATE_DIR="$state" bash "$SRC_SIM" 2>/dev/null
rc=$?
#     Matched on the root's own wording, not the phrase both refusals share.
#     With realpath broken and a fixture root, both classifications come back
#     unknown, so a loose match here is satisfied by the state refusal and this
#     case stops pinning the root one at all -- deleting the root check leaves
#     the suite green. The two refusals cover for each other in both
#     directions, so each case names the message it is actually about.
if [ "$rc" = 1 ] &&
    grep -q "err: .*could not be resolved to tell whether it is this host's own EDAC tree" "$LOGFILE" &&
    ! grep -q "correctable memory errors" "$LOGFILE" &&
    [ ! -e "$state/high-water" ] &&
    tagged_as_test "$LOGFILE"; then
    echo "PASS  unresolvable root -> refused by the root check (rc=$rc)"
else
    echo "FAIL  unresolvable root (rc=$rc, mark exists: $([ -e "$state/high-water" ] && echo yes || echo no)); log: $(cat "$LOGFILE")"
    FAILED=1
fi

# 18. The same refusal reached through the STATE path rather than the root one.
#     Case 17 hands the script a fixture root, so with realpath broken the
#     root's own classification fails first and answers for both -- the state
#     check is never the thing that fired. Naming the production EDAC tree
#     instead settles the root by literal comparison, needing no realpath, so
#     the unresolved state directory is what stops the run. Nothing reads /sys
#     here: the refusal happens before the tree is opened.
n=$((n + 1))
state="$TMP/state$n"
mkdir -p "$state"
export LOGFILE="$TMP/log$n"
: >"$LOGFILE"
PATH="$TMP/badbin:$PATH" RP_EDAC_ROOT=/sys/devices/system/edac/mc \
    RP_EDAC_STATE_DIR="$state" bash "$SRC_SIM" 2>/dev/null
rc=$?
if [ "$rc" = 1 ] &&
    grep -q "err: .*could not be resolved to tell whether it is the production state directory" "$LOGFILE" &&
    [ ! -e "$state/high-water" ] &&
    tagged_as_test "$LOGFILE"; then
    echo "PASS  unresolvable state dir alone -> refused by the state check (rc=$rc)"
else
    echo "FAIL  unresolvable state dir alone (rc=$rc); log: $(cat "$LOGFILE")"
    FAILED=1
fi

# 19-28. The identity itself, decided before anything is read.
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
eval "$(awk '/^state_is_production\(\) \{/,/^\}/' "$SRC")"
eval "$(awk '/^root_is_production\(\) \{/,/^\}/' "$SRC")"
eval "$(awk '/^set_run_identity\(\) \{/,/^\}/' "$SRC")"

identity() { # identity <desc> <want_tag> <want_prefix_substring|-> [root] [statedir]
    local desc=$1 want_tag=$2 want_pre=$3
    n=$((n + 1))
    TAG="(none)"
    MSG_PREFIX="(none)"
    PROD_STATE_DIR=/var/lib/rp-edac-check
    PROD_EDAC_ROOT=/sys/devices/system/edac/mc
    EDAC_ROOT=${4:-$PROD_EDAC_ROOT}
    STATE_DIR=${5:-$PROD_STATE_DIR}
    STATE="$STATE_DIR/high-water"
    if [ -n "${4:-}" ]; then RP_EDAC_ROOT=$4; else unset RP_EDAC_ROOT; fi
    if [ -n "${5:-}" ]; then RP_EDAC_STATE_DIR=$5; else unset RP_EDAC_STATE_DIR; fi
    # Computed the way production computes them, so these cases exercise the
    # real classification rather than values the harness chose.
    state_is_production
    STATE_IS_PROD=$?
    root_is_production
    ROOT_IS_PROD=$?
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

# 18. THE production case: no overrides, no prefix, the tag the runbook greps.
identity "no overrides -> production tag, no prefix" rp-edac-check -

# 19. A synthetic tree: the numbers are not this host's at all.
identity "synthetic EDAC root -> test tag names the root" \
    rp-edac-check-test "reading /fixture/mc, not this host's memory" /fixture/mc

# 20. Real counters, synthetic baseline. The numbers are this host's and the
#     delta is not, so the prefix must not claim the readings are fake.
identity "redirected state dir -> test tag names the baseline" \
    rp-edac-check-test "measuring against /fixture/state/high-water, not this host's baseline" \
    "" /fixture/state

# 21. A state override naming the production directory is NOT a test run. It
#     reads this host's real counters and rewrites its real mark, however it
#     was spelled, so calling it a test would file a genuine rising error
#     count under the tag the runbook never greps -- the same missed fault as
#     the bug this PR started from, arrived at from the opposite side.
identity "state dir = production dir -> production tag, no prefix" \
    rp-edac-check - "" /var/lib/rp-edac-check

# 22. And the same by an equivalent spelling, since that is how someone would
#     actually arrive here rather than by typing the canonical form.
identity "state dir resolving to production -> production tag, no prefix" \
    rp-edac-check - "" /var/lib/rp-edac-check/.

# 23a. A root override naming the production tree is NOT synthetic data. It
#      reads this host's real counters, so the prefix must not disclaim them --
#      that would dismiss a genuine fault as a fixture, which is the costlier
#      of the two ways this script can misdescribe itself. The scratch state
#      directory still makes it a test run, and that reason still stands.
identity "root = production tree, scratch state -> baseline reason still given" \
    rp-edac-check-test "measuring against /fixture/state/high-water, not this host's baseline" \
    /sys/devices/system/edac/mc /fixture/state

# And the negative half of the same case, which is the one that matters: the
# prefix must not ALSO claim the counters are synthetic. Asserted separately
# because `identity` checks for a substring being present, and what is wrong
# here is a different substring being present alongside it.
TAG="(none)"
MSG_PREFIX="(none)"
PROD_STATE_DIR=/var/lib/rp-edac-check
PROD_EDAC_ROOT=/sys/devices/system/edac/mc
EDAC_ROOT=/sys/devices/system/edac/mc
STATE_DIR=/fixture/state
STATE=/fixture/state/high-water
RP_EDAC_ROOT=/sys/devices/system/edac/mc
RP_EDAC_STATE_DIR=/fixture/state
state_is_production
STATE_IS_PROD=$?
root_is_production
ROOT_IS_PROD=$?
set_run_identity
n=$((n + 1))
case "$MSG_PREFIX" in
    *"not this host's memory"*)
        echo "FAIL  production tree named explicitly must not be disclaimed (prefix=[$MSG_PREFIX])"
        FAILED=1
        ;;
    *)
        echo "PASS  production tree named explicitly is not disclaimed as synthetic"
        ;;
esac

# 23b. The same root with no state override at all is simply the production
#      run, spelled the long way. Nothing about it is synthetic and nothing
#      needs refusing, so it carries the production identity.
identity "root = production tree, no state override -> production identity" \
    rp-edac-check - /sys/devices/system/edac/mc

# 23c. The same by a non-canonical spelling, which is the counterpart to the
#      state directory's `/.` case and was missing while that one existed.
#      Every other production-root case here uses the canonical path, so a
#      root comparison that stopped resolving -- and started matching raw
#      names -- would keep the suite green while treating this spelling as
#      synthetic: a real reading disclaimed as a fixture, and refused outright
#      if the state directory happened to be production's.
identity "root spelled non-canonically -> still production identity" \
    rp-edac-check - /sys/devices/system/edac/mc/.

# 24. Both overrides, as every full-script case above sets them: both reasons
#     are named.
n=$((n + 1))
TAG="(none)"
MSG_PREFIX="(none)"
PROD_STATE_DIR=/var/lib/rp-edac-check
PROD_EDAC_ROOT=/sys/devices/system/edac/mc
EDAC_ROOT=/fixture/mc
STATE_DIR=/fixture/state
STATE=/fixture/state/high-water
RP_EDAC_ROOT=/fixture/mc
RP_EDAC_STATE_DIR=/fixture/state
state_is_production
STATE_IS_PROD=$?
root_is_production
ROOT_IS_PROD=$?
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

# 24. An empty override is a production run, because `${RP_EDAC_ROOT:-...}`
#     falls back to the default for one. The identity has to agree with the
#     path that is actually read, or an empty variable would silently downgrade
#     the real check to a test-tagged one and hide a genuine fault.
#
#     This half only covers the classification, because it hands the function
#     paths that are already resolved -- the expansion it names is not executed
#     here at all. The last case runs it end to end for that reason; the two
#     are a pair and neither is sufficient alone.
n=$((n + 1))
TAG="(none)"
MSG_PREFIX="(none)"
PROD_STATE_DIR=/var/lib/rp-edac-check
PROD_EDAC_ROOT=/sys/devices/system/edac/mc
EDAC_ROOT=/sys/devices/system/edac/mc
STATE_DIR=/var/lib/rp-edac-check
STATE=/var/lib/rp-edac-check/high-water
RP_EDAC_ROOT=""
RP_EDAC_STATE_DIR=""
state_is_production
STATE_IS_PROD=$?
root_is_production
ROOT_IS_PROD=$?
set_run_identity
if [ "$TAG" = rp-edac-check ] && [ -z "$MSG_PREFIX" ]; then
    echo "PASS  empty overrides -> production tag, no prefix"
else
    echo "FAIL  empty overrides -> production tag, no prefix (tag=$TAG prefix=[$MSG_PREFIX])"
    FAILED=1
fi

# 25-26. The production identity, end to end through the real note/warn.
#
# The mirror of tagged_as_test, and the assertion that actually matters here:
# the production tag, and no test marker anywhere in the line. A regression
# that hard-coded the test tag would satisfy every other case in this file.
tagged_as_production() { # tagged_as_production <logfile>
    local line
    while IFS= read -r line; do
        case "$line" in
            *"[TEST RUN"*) return 1 ;;
        esac
        case "$line" in
            "rp-edac-check err: "* | "rp-edac-check info: "*) ;;
            *) return 1 ;;
        esac
    done <"$1"
    return 0
}

unset RP_EDAC_ROOT RP_EDAC_STATE_DIR

# 25. An unconfigured run that finds errors escalates under the production
#     tag, with no prefix, and keeps its mark.
n=$((n + 1))
rm -rf "$PROD_SIM" "$PROD_MC"
mkdir -p "$PROD_SIM"
mkmc "$PROD_MC" 0 4 0
printf '1 0 baseline\n' >"$PROD_SIM/high-water"
export LOGFILE="$TMP/log$n"
: >"$LOGFILE"
bash "$SRC_PROD" 2>/dev/null
rc=$?
if [ "$rc" = 0 ] &&
    grep -q "^rp-edac-check err: correctable memory errors: 4 total, up 3" "$LOGFILE" &&
    tagged_as_production "$LOGFILE" &&
    [ "$(cut -d' ' -f1,2 <"$PROD_SIM/high-water")" = "4 0" ]; then
    echo "PASS  unconfigured run escalates under the production tag (rc=$rc)"
else
    echo "FAIL  unconfigured run escalates under the production tag (rc=$rc, mark: $(cat "$PROD_SIM/high-water" 2>/dev/null)); log: $(cat "$LOGFILE")"
    FAILED=1
fi

# 26. And a healthy one stays silent, which is the contract that makes any
#     line under this tag worth reading.
n=$((n + 1))
rm -rf "$PROD_SIM" "$PROD_MC"
mkdir -p "$PROD_SIM"
mkmc "$PROD_MC" 0 0 0
export LOGFILE="$TMP/log$n"
: >"$LOGFILE"
bash "$SRC_PROD" 2>/dev/null
rc=$?
if [ "$rc" = 0 ] && [ ! -s "$LOGFILE" ] && [ -e "$PROD_SIM/high-water" ]; then
    echo "PASS  unconfigured healthy run is silent and still marks (rc=$rc)"
else
    echo "FAIL  unconfigured healthy run is silent and still marks (rc=$rc); log: $(cat "$LOGFILE")"
    FAILED=1
fi

# 27. The production tree named explicitly, with no state override: a
#     production run spelled the long way, and nothing to refuse. This is the
#     one case the guard's wording change actually loosened -- it used to be
#     rejected for having an override present at all -- so it is asserted end
#     to end rather than only through the identity function, which is where a
#     guard regression would show and an identity case would not.
n=$((n + 1))
rm -rf "$PROD_SIM" "$PROD_MC"
mkdir -p "$PROD_SIM"
mkmc "$PROD_MC" 0 2 0
printf '0 0 baseline\n' >"$PROD_SIM/high-water"
export LOGFILE="$TMP/log$n"
: >"$LOGFILE"
RP_EDAC_ROOT="$PROD_MC" bash "$SRC_PROD" 2>/dev/null
rc=$?
if [ "$rc" = 0 ] &&
    grep -q "^rp-edac-check err: correctable memory errors: 2 total, up 2" "$LOGFILE" &&
    tagged_as_production "$LOGFILE" &&
    [ "$(cut -d' ' -f1,2 <"$PROD_SIM/high-water")" = "2 0" ]; then
    echo "PASS  production tree named explicitly runs as production (rc=$rc)"
else
    echo "FAIL  production tree named explicitly runs as production (rc=$rc, mark: $(cat "$PROD_SIM/high-water" 2>/dev/null)); log: $(cat "$LOGFILE")"
    FAILED=1
fi

# 28. The re-baseline, under the production identity.
#
#     The cases above reach `warn` and silence; `note` has only ever been
#     watched under the test identity, so hard-coding the test tag into it
#     alone would pass everything else here. What that hides is specific and
#     badly timed: the reset line is what tells a reader the counters they are
#     looking at started over at the last boot, and the reader who needs it is
#     the one grepping this tag after an unexplained reboot. Losing it there
#     leaves a low count looking like a clean history rather than a cleared
#     one.
n=$((n + 1))
rm -rf "$PROD_SIM" "$PROD_MC"
mkdir -p "$PROD_SIM"
mkmc "$PROD_MC" 0 0 0
printf '58 2 baseline\n' >"$PROD_SIM/high-water"
export LOGFILE="$TMP/log$n"
: >"$LOGFILE"
bash "$SRC_PROD" 2>/dev/null
rc=$?
if [ "$rc" = 0 ] &&
    grep -q "^rp-edac-check info: counters reset (ce 58 -> 0, ue 2 -> 0)" "$LOGFILE" &&
    ! grep -q " err: " "$LOGFILE" &&
    tagged_as_production "$LOGFILE" &&
    [ "$(cut -d' ' -f1,2 <"$PROD_SIM/high-water")" = "0 0" ]; then
    echo "PASS  production re-baseline is info under the production tag (rc=$rc)"
else
    echo "FAIL  production re-baseline is info under the production tag (rc=$rc, mark: $(cat "$PROD_SIM/high-water" 2>/dev/null)); log: $(cat "$LOGFILE")"
    FAILED=1
fi

# 29. Empty overrides, end to end -- the `:-` fallbacks themselves.
#
#     The identity case for this hands `set_run_identity` the resolved paths
#     directly, so it never runs the `${RP_EDAC_ROOT:-...}` expansions it
#     exists to describe. Weaken those to `${RP_EDAC_ROOT-...}` and an empty
#     variable stops falling back: both paths become the empty string, neither
#     resolves, and the check refuses and exits 1 on every firing. The monitor
#     goes dark, and a suite that only ever supplied the resolved values would
#     have nothing to say about it.
#
#     An empty override is not contrived -- `Environment=RP_EDAC_ROOT=` in a
#     unit, or a wrapper exporting an unset shell variable, produces exactly
#     this, and the failure would be silent in the way that matters: a check
#     that is not running looks identical to one finding nothing.
n=$((n + 1))
rm -rf "$PROD_SIM" "$PROD_MC"
mkdir -p "$PROD_SIM"
mkmc "$PROD_MC" 0 6 0
printf '1 0 baseline\n' >"$PROD_SIM/high-water"
export LOGFILE="$TMP/log$n"
: >"$LOGFILE"
RP_EDAC_ROOT="" RP_EDAC_STATE_DIR="" bash "$SRC_PROD" 2>/dev/null
rc=$?
if [ "$rc" = 0 ] &&
    grep -q "^rp-edac-check err: correctable memory errors: 6 total, up 5" "$LOGFILE" &&
    tagged_as_production "$LOGFILE" &&
    [ "$(cut -d' ' -f1,2 <"$PROD_SIM/high-water")" = "6 0" ]; then
    echo "PASS  empty overrides fall back to production end to end (rc=$rc)"
else
    echo "FAIL  empty overrides fall back to production end to end (rc=$rc, mark: $(cat "$PROD_SIM/high-water" 2>/dev/null)); log: $(cat "$LOGFILE")"
    FAILED=1
fi

# 30. Real counters, scratch baseline -- the reverse pairing, end to end.
#
#     The runbook promises this mode stays supported, and until now only the
#     identity function was asked about it: no case ran the guard, scanned
#     counters and persisted a scratch mark. A regression that refused this
#     pairing, or that wrote the real mark instead of the scratch one, would
#     have passed while the documentation went on promising it.
#
#     Both halves are asserted because they fail differently: refusing it
#     breaks a documented workflow, while writing the wrong mark corrupts
#     production's baseline from a run that was supposed to be safe.
n=$((n + 1))
rm -rf "$PROD_SIM" "$PROD_MC"
mkdir -p "$PROD_SIM"
scratch="$TMP/scratch$n"
mkdir -p "$scratch"
mkmc "$PROD_MC" 0 9 0
printf '4 0 baseline\n' >"$PROD_SIM/high-water"
printf '2 0 scratch\n' >"$scratch/high-water"
export LOGFILE="$TMP/log$n"
: >"$LOGFILE"
RP_EDAC_STATE_DIR="$scratch" bash "$SRC_PROD" 2>/dev/null
rc=$?
if [ "$rc" = 0 ] &&
    grep -q "err: .*correctable memory errors: 9 total, up 7" "$LOGFILE" &&
    grep -q "not this host's baseline" "$LOGFILE" &&
    ! grep -q "not this host's memory" "$LOGFILE" &&
    tagged_as_test "$LOGFILE" &&
    [ "$(cut -d' ' -f1,2 <"$scratch/high-water")" = "9 0" ] &&
    [ "$(cat "$PROD_SIM/high-water")" = "4 0 baseline" ]; then
    echo "PASS  scratch state with the real root runs, marks scratch, leaves production (rc=$rc)"
else
    echo "FAIL  scratch state with the real root (rc=$rc; scratch: $(cat "$scratch/high-water" 2>/dev/null); production: $(cat "$PROD_SIM/high-water" 2>/dev/null)); log: $(cat "$LOGFILE")"
    FAILED=1
fi

# 31. A non-canonical production root, end to end. The state directory has had
#     both an identity case and a full-script one for its `/.` spelling since
#     the guard landed; the root had neither until now, and the identity case
#     alone would not notice a run that classified correctly and then behaved
#     as a test anyway.
n=$((n + 1))
rm -rf "$PROD_SIM" "$PROD_MC"
mkdir -p "$PROD_SIM"
mkmc "$PROD_MC" 0 3 0
printf '0 0 baseline\n' >"$PROD_SIM/high-water"
export LOGFILE="$TMP/log$n"
: >"$LOGFILE"
RP_EDAC_ROOT="$PROD_MC/." bash "$SRC_PROD" 2>/dev/null
rc=$?
if [ "$rc" = 0 ] &&
    grep -q "^rp-edac-check err: correctable memory errors: 3 total, up 3" "$LOGFILE" &&
    tagged_as_production "$LOGFILE" &&
    [ "$(cut -d' ' -f1,2 <"$PROD_SIM/high-water")" = "3 0" ]; then
    echo "PASS  non-canonical production root runs as production (rc=$rc)"
else
    echo "FAIL  non-canonical production root runs as production (rc=$rc, mark: $(cat "$PROD_SIM/high-water" 2>/dev/null)); log: $(cat "$LOGFILE")"
    FAILED=1
fi

echo "ran $n case(s)"
exit "$FAILED"
