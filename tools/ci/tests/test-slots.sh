#!/bin/bash
# Exercise the slot-table parse: rp_trim and load_slots.
#
# The contract under test, in one line: a well-formed table is returned
# normalised and in order, and EVERY other input is refused with a reason
# naming the offending slot — because this script destroys storage and
# deregisters runners by VMID, so a table it had only half understood would
# aim those at a guest nobody named.
#
# Fixtures here are SYNTHETIC BY POLICY, and more strictly than in this
# harness's siblings. Moving the table out of the script removed the pool's
# real VMIDs and slot names from this public repository altogether, so there
# is no longer an in-script array whose values a test could echo harmlessly.
# Pasting a live table in while chasing a failure on the hypervisor would put
# host inventory back into the repo that the refactor just took out of it.
# Invented names and VMIDs exercise the parse exactly as well.
#
# The harness never needs a Proxmox host: load_slots reaches nothing but the
# file at SLOTS_FILE, assigned to a tmpdir path directly. That works here
# because only the lifted functions run; the script derives that variable from
# the RP_SLOTS_FILE environment override at startup, and THAT is the knob a
# real deployment would use.
#
# It does need `python3`, which the labels check parses JSON with. Named here
# rather than stubbed, for the reason the ECC watch depends on `realpath`: the
# alternative is reimplementing a parser, and the script under test already
# requires python3 at startup to read the runner-group response. So this is
# hermetic with respect to infrastructure, which is the property that matters,
# rather than free of the host entirely.
#
# Functions are lifted out of the script with `awk` rather than sourced,
# because sourcing would run the top-level slot loops.
#
# shellcheck disable=SC2034
# SLOTS_FILE is read only by the functions eval'd in from the script under
# test, which shellcheck cannot see, so it looks unused here.
set -u -o pipefail

SRC=${1:?path to rp-runner-pool.sh}
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

SLOTS_FILE="$TMP/slots"

eval "$(awk '/^rp_trim\(\) \{/,/^\}/' "$SRC")"
eval "$(awk '/^load_slots\(\) \{/,/^\}/' "$SRC")"

FAILED=0

LINUX_LABELS='["self-hosted","Linux","X64","pool-label"]'
WIN_LABELS='["self-hosted","Windows","X64","pool-label-windows"]'

# Assert load_slots succeeds and prints exactly $2 (a newline-joined table).
accepts() {
  local what=$1 want=$2 got rc
  got=$(load_slots)
  rc=$?
  if [ "$rc" -eq 0 ] && [ "$got" = "$want" ]; then
    echo "PASS  $what"
  else
    echo "FAIL  $what"
    echo "      rc=$rc (wanted 0)"
    echo "      got=[$got]"
    echo "      want=[$want]"
    FAILED=1
  fi
}

# Assert load_slots refuses, and that its reason contains $2 — so a case
# cannot pass on some *other* refusal that happens to share the exit status.
# Several of these refusals would otherwise cover for each other.
refuses() {
  local what=$1 want=$2 got rc
  got=$(load_slots)
  rc=$?
  if [ "$rc" -ne 0 ] && [ "${got#*"$want"}" != "$got" ]; then
    echo "PASS  $what"
  else
    echo "FAIL  $what"
    echo "      rc=$rc (wanted non-zero)"
    echo "      reason=[$got]"
    echo "      wanted a reason containing [$want]"
    FAILED=1
  fi
}

# --- the happy paths -------------------------------------------------------

printf 'alpha|100|200|linux|%s\nbeta|101|201|windows|%s\n' \
  "$LINUX_LABELS" "$WIN_LABELS" >"$SLOTS_FILE"
accepts "a well-formed table is returned in order" \
  "alpha|100|200|linux|$LINUX_LABELS
beta|101|201|windows|$WIN_LABELS"

{
  echo "# a comment"
  echo ""
  echo "   "
  printf 'alpha|100|200|linux|%s\n' "$LINUX_LABELS"
  echo "   # an indented comment"
} >"$SLOTS_FILE"
accepts "comments, blank and whitespace-only lines are skipped" \
  "alpha|100|200|linux|$LINUX_LABELS"

printf '  alpha  |  100 |  200  | linux |  %s  \n' "$LINUX_LABELS" >"$SLOTS_FILE"
accepts "a table aligned into columns is trimmed to the same result" \
  "alpha|100|200|linux|$LINUX_LABELS"

# A table an editor left without a trailing newline must not lose its last
# slot: that would start a pool one slot short and look entirely healthy.
printf 'alpha|100|200|linux|%s\nbeta|101|201|linux|%s' \
  "$LINUX_LABELS" "$LINUX_LABELS" >"$SLOTS_FILE"
accepts "a final line with no trailing newline is still read" \
  "alpha|100|200|linux|$LINUX_LABELS
beta|101|201|linux|$LINUX_LABELS"

# --- the refusals ----------------------------------------------------------

rm -f "$SLOTS_FILE"
refuses "an absent table is fatal, not an empty pool" "no slot table at"

mkdir -p "$SLOTS_FILE"
refuses "a directory at the path is refused, not read as empty" "not a readable file"
rmdir "$SLOTS_FILE"

: >"$SLOTS_FILE"
refuses "an empty table declares no slots" "declares no slots"

printf '# only a comment\n\n' >"$SLOTS_FILE"
refuses "a table of nothing but comments declares no slots" "declares no slots"

printf 'alpha|100|200|linux\n' >"$SLOTS_FILE"
refuses "a line missing its labels does not parse" "does not parse"

printf 'alpha|100|200|linux|%s|extra\n' "$LINUX_LABELS" >"$SLOTS_FILE"
refuses "a line with a sixth field does not parse" "does not parse"

printf '   |100|200|linux|%s\n' "$LINUX_LABELS" >"$SLOTS_FILE"
refuses "a name of only whitespace does not parse" "does not parse"

printf 'alpha|one hundred|200|linux|%s\n' "$LINUX_LABELS" >"$SLOTS_FILE"
refuses "a non-numeric template is not a VMID" "which is not a VMID"

printf 'alpha|100|two hundred|linux|%s\n' "$LINUX_LABELS" >"$SLOTS_FILE"
refuses "a non-numeric clone is not a VMID" "which is not a VMID"

printf 'alpha|200|200|linux|%s\n' "$LINUX_LABELS" >"$SLOTS_FILE"
refuses "a slot whose template and clone are one VMID is refused" \
  "both template and clone"

printf 'alpha|100|200|freebsd|%s\n' "$LINUX_LABELS" >"$SLOTS_FILE"
refuses "an unknown guest OS is refused" "only linux and windows are known"

printf 'alpha|100|200|linux|proxmox-ephemeral\n' >"$SLOTS_FILE"
refuses "labels that are not a JSON array are refused" "not a JSON array"

# Array-shaped but not parseable: the case a shape check would wave through,
# leaving it to fail at registration once per clone cycle instead.
printf 'alpha|100|200|linux|[self-hosted, Linux]\n' >"$SLOTS_FILE"
refuses "labels shaped like an array but invalid JSON are refused" \
  "not a JSON array"

# Valid JSON, wrong type: the request body wants an array.
printf 'alpha|100|200|linux|{"label":"pool"}\n' >"$SLOTS_FILE"
refuses "labels that are valid JSON but not an array are refused" \
  "not a JSON array"

# A name carrying whitespace could never match its own STATIC_NET_FILE line,
# whose fields are whitespace-separated — so the slot would quietly run on
# DHCP while the host's config said it was pinned.
printf 'two words|100|200|linux|%s\n' "$LINUX_LABELS" >"$SLOTS_FILE"
refuses "a slot name containing whitespace is refused" "contains whitespace"

# Two slots on one name would pin both to one address through STATIC_NET_FILE
# and make the pool's own logs ambiguous.
printf 'alpha|100|200|linux|%s\nalpha|100|201|linux|%s\n' \
  "$LINUX_LABELS" "$LINUX_LABELS" >"$SLOTS_FILE"
refuses "a repeated slot name is refused" "more than once"

# The one that matters most: two loops sharing a clone VMID would each destroy
# the other's guest mid-job, forever.
printf 'alpha|100|200|linux|%s\nbeta|100|200|linux|%s\n' \
  "$LINUX_LABELS" "$LINUX_LABELS" >"$SLOTS_FILE"
refuses "a clone VMID given to two slots is refused" "more than one slot"

# A refusal must not depend on the bad line coming first: the accumulators the
# duplicate checks read are built as the parse walks, so a table that is valid
# until its last line is the case that would expose an ordering bug.
printf 'alpha|100|200|linux|%s\nbeta|101|201|linux|%s\ngamma|102|200|linux|%s\n' \
  "$LINUX_LABELS" "$LINUX_LABELS" "$LINUX_LABELS" >"$SLOTS_FILE"
refuses "a duplicate VMID on the last line is still caught" "more than one slot"

exit "$FAILED"
