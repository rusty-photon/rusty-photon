#!/bin/bash
# Exercise the pinned-addressing path: slot_static_net (the host file parse),
# static_mac (the address-to-MAC derivation), and apply_static_net (what
# actually lands on the clone).
#
# The contract under test, in one line: a pinned Linux slot gets exactly the
# configured address behind a MAC derived from it, a pinned Windows slot gets
# only that MAC (its address is the router's fixed lease for it), a slot the
# host does not pin is left on DHCP with qm never consulted, and a pin that
# cannot land says why instead of half-landing.
#
# Fixtures here are SYNTHETIC BY POLICY. This repository is public, and these
# tests are the obvious place for the pool's real addressing to get pasted in
# while chasing a failure on the hypervisor. Do not: addresses are host
# inventory, and the documentation range 192.0.2.0/24 exercises the code
# exactly as well.
#
# The harness never needs a Proxmox host: `qm` is stubbed, and the host file
# the parse reads is a tmpdir path — assigned to the STATIC_NET_FILE variable
# directly, which works here because only the lifted functions run; the
# script itself derives that variable from the RP_STATIC_NET_FILE environment
# override at startup, and THAT is the knob a real deployment would use. It
# must never grow a fallback that shells out to a real `qm`. `sed` runs real,
# because the net0 rewrite IS a sed expression and stubbing it would test the
# stub.
#
# Functions are lifted out of the script with `awk` rather than sourced,
# because sourcing would run the top-level slot loops.
#
# shellcheck disable=SC2034,SC2317,SC2329
# Structural to this harness, as in its siblings: the globals and the `qm`
# stub are read and called only by the functions eval'd in from the script
# under test, which shellcheck cannot see, so all of them look unused or
# unreachable to it.
set -u -o pipefail

SRC=${1:?path to rp-runner-pool.sh}
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

STATIC_NET_FILE="$TMP/static-net"

eval "$(awk '/^slot_static_net\(\) \{/,/^\}/' "$SRC")"
eval "$(awk '/^static_mac\(\) \{/,/^\}/' "$SRC")"
eval "$(awk '/^apply_static_net\(\) \{/,/^\}/' "$SRC")"

# The qm stub records through files, not variables: apply_static_net invokes
# `qm set` inside a command substitution, so a variable written by the stub
# would die with that subshell and the assertion would read the initial value.
QM_NET0_LINE="net0: virtio=BC:24:11:AA:BB:CC,bridge=vmbr1,firewall=1,tag=67"
qm() {
  echo "$*" >>"$TMP/qm-calls"
  case $1 in
    config)
      if [ -e "$TMP/qm-config-fail" ]; then
        echo "Configuration file 'nodes/x/qemu-server/9100.conf' does not exist" >&2
        return 1
      fi
      printf 'boot: order=scsi0\n'
      [ -n "$QM_NET0_LINE" ] && printf '%s\n' "$QM_NET0_LINE"
      printf 'ostype: l26\n'
      ;;
    set)
      shift
      printf '%s\n' "$*" >"$TMP/qm-set-args"
      if [ -e "$TMP/qm-set-fail" ]; then
        echo "update VM 9100: some qm refusal" >&2
        return 1
      fi
      ;;
  esac
}

FAILED=0
pass() { echo "PASS  $1"; }
fail() {
  echo "FAIL  $1"
  FAILED=1
}

# check_rc <desc> <want-rc> <want-output-substring> -- <cmd...>
# Empty substring means "any output, or none".
check_rc() {
  local desc=$1 want_rc=$2 want_out=$3 got rc
  shift 4
  got=$("$@")
  rc=$?
  if [ "$rc" != "$want_rc" ]; then
    fail "$desc (rc $rc, wanted $want_rc; output '$got')"
    return
  fi
  case "$got" in
    *"$want_out"*) pass "$desc" ;;
    *) fail "$desc (output '$got' lacks '$want_out')" ;;
  esac
}

reset_state() {
  rm -f "$TMP/qm-calls" "$TMP/qm-set-args" "$TMP/qm-set-fail" \
    "$TMP/qm-config-fail" "$STATIC_NET_FILE"
  QM_NET0_LINE="net0: virtio=BC:24:11:AA:BB:CC,bridge=vmbr1,firewall=1,tag=67"
}

# ---- static_mac: the derivation and its refusals ------------------------

check_rc "derives the MAC from the low three octets" 0 "BE:24:11:00:02:07" \
  -- static_mac 192.0.2.7/24
check_rc "prefix is optional to the derivation itself" 0 "BE:24:11:00:02:07" \
  -- static_mac 192.0.2.7
check_rc "octet 255 is a boundary, not a refusal" 0 "BE:24:11:FF:FF:FF" \
  -- static_mac 10.255.255.255/16
check_rc "a leading-zero octet is decimal, not octal" 0 "BE:24:11:00:02:07" \
  -- static_mac 192.0.002.7/24
check_rc "octet 256 is refused" 1 "" -- static_mac 10.0.0.256/16
check_rc "three octets are refused" 1 "" -- static_mac 10.0.0/16
check_rc "five octets are refused" 1 "" -- static_mac 192.0.2.7.9/24
check_rc "a non-numeric octet is refused" 1 "" -- static_mac abc.0.2.7/24
check_rc "an empty address is refused" 1 "" -- static_mac /24

# ---- slot_static_net: the parse -----------------------------------------

reset_state
check_rc "no file means not pinned" 1 "" -- slot_static_net runner-linux1

cat >"$STATIC_NET_FILE" <<'EOF'
# host inventory, synthetic for the harness
runner-linux11 192.0.2.11/24 192.0.2.1 192.0.2.1

runner-linux1 192.0.2.7/24 192.0.2.1 192.0.2.2
runner-linux1 192.0.2.99/24 192.0.2.1 192.0.2.1
EOF
check_rc "a pinned slot gets its fields back" 0 "192.0.2.7/24 192.0.2.1 192.0.2.2" \
  -- slot_static_net runner-linux1
check_rc "the first matching line wins over a duplicate" 0 "192.0.2.7/24" \
  -- slot_static_net runner-linux1
check_rc "name matching is exact, not prefix" 0 "192.0.2.11/24" \
  -- slot_static_net runner-linux11
check_rc "an unlisted slot is not pinned" 1 "" -- slot_static_net runner-win

printf 'runner-linux1 192.0.2.7/24 192.0.2.1\n' >"$STATIC_NET_FILE"
check_rc "a missing field is malformed, not unpinned" 2 "does not parse" \
  -- slot_static_net runner-linux1

printf 'runner-linux1 192.0.2.7/24 192.0.2.1 192.0.2.1 surplus\n' >"$STATIC_NET_FILE"
check_rc "a surplus field is malformed, not ignored" 2 "does not parse" \
  -- slot_static_net runner-linux1

printf 'runner-linux1 192.0.2.7 192.0.2.1 192.0.2.1\n' >"$STATIC_NET_FILE"
check_rc "an address without a prefix is named as the mistake" 2 "without a /prefix" \
  -- slot_static_net runner-linux1

reset_state
mkdir "$TMP/static-dir"
STATIC_NET_FILE="$TMP/static-dir"
check_rc "a directory at the path is loud, not a silent DHCP fallback" 2 "not a readable file" \
  -- slot_static_net runner-linux1
STATIC_NET_FILE="$TMP/static-net"

reset_state
ln -s "$TMP/nowhere" "$STATIC_NET_FILE"
check_rc "a dangling symlink at the path is loud, not absent" 2 "not a readable file" \
  -- slot_static_net runner-linux1

# Root reads through any mode bits, so this case cannot execute there. Skip
# loudly rather than let a root run report a PASS it never earned — the same
# policy as the sibling harnesses' chmod-based cases.
if [ "$(id -u)" -eq 0 ]; then
  echo "SKIP  an unreadable file is loud, not a silent DHCP fallback (running as root)"
else
  reset_state
  printf 'runner-linux1 192.0.2.7/24 192.0.2.1 192.0.2.1\n' >"$STATIC_NET_FILE"
  chmod 000 "$STATIC_NET_FILE"
  check_rc "an unreadable file is loud, not a silent DHCP fallback" 2 "not a readable file" \
    -- slot_static_net runner-linux1
  chmod 644 "$STATIC_NET_FILE"
fi

# ---- apply_static_net: what lands on the clone --------------------------

reset_state
check_rc "an unpinned slot applies nothing" 1 "" -- apply_static_net runner-linux1 9100 linux
if [ ! -e "$TMP/qm-calls" ]; then
  pass "and qm was never consulted for it"
else
  fail "and qm was never consulted for it (calls: $(cat "$TMP/qm-calls"))"
fi

reset_state
printf 'runner-linux1 192.0.2.7/24 192.0.2.1 192.0.2.2\n' >"$STATIC_NET_FILE"
check_rc "a pinned slot reports what it pinned" 0 "pinned 192.0.2.7/24 via BE:24:11:00:02:07" \
  -- apply_static_net runner-linux1 9100 linux
args=$(cat "$TMP/qm-set-args" 2>/dev/null)
case "$args" in
  *"--net0 virtio=BE:24:11:00:02:07,bridge=vmbr1,firewall=1,tag=67"*)
    pass "net0 keeps the template's bridge, firewall and tag around the new MAC" ;;
  *) fail "net0 keeps the template's bridge, firewall and tag around the new MAC (got '$args')" ;;
esac
case "$args" in
  *"--ipconfig0 ip=192.0.2.7/24,gw=192.0.2.1"*) pass "cloud-init gets the address and gateway" ;;
  *) fail "cloud-init gets the address and gateway (got '$args')" ;;
esac
case "$args" in
  *"--nameserver 192.0.2.2"*) pass "cloud-init gets the nameserver" ;;
  *) fail "cloud-init gets the nameserver (got '$args')" ;;
esac

reset_state
printf 'runner-linux1 192.0.2.7/24 192.0.2.1 192.0.2.1\n' >"$STATIC_NET_FILE"
QM_NET0_LINE="net0: e1000=BC:24:11:AA:BB:CC,bridge=vmbr0"
check_rc "the rewrite keys on the MAC shape, not the NIC model" 0 "via BE:24:11:00:02:07" \
  -- apply_static_net runner-linux1 9100 linux
args=$(cat "$TMP/qm-set-args" 2>/dev/null)
case "$args" in
  *"--net0 e1000=BE:24:11:00:02:07,bridge=vmbr0"*) pass "and the model passes through untouched" ;;
  *) fail "and the model passes through untouched (got '$args')" ;;
esac

reset_state
printf 'runner-linux1 192.0.2.7/24 192.0.2.1 192.0.2.1\n' >"$STATIC_NET_FILE"
QM_NET0_LINE=""
check_rc "a config with no net0 line refuses with that reason" 2 "no net0 line" \
  -- apply_static_net runner-linux1 9100 linux
if [ ! -e "$TMP/qm-set-args" ]; then
  pass "and qm set never ran for it"
else
  fail "and qm set never ran for it (args: $(cat "$TMP/qm-set-args"))"
fi

reset_state
printf 'runner-linux1 192.0.2.7/24 192.0.2.1 192.0.2.1\n' >"$STATIC_NET_FILE"
: >"$TMP/qm-config-fail"
check_rc "a failed qm config surfaces qm's own words, not a net0 claim" 2 "does not exist" \
  -- apply_static_net runner-linux1 9100 linux
got=$(apply_static_net runner-linux1 9100 linux)
case "$got" in
  *net0*) fail "and the failure is not blamed on net0 (got '$got')" ;;
  *"reading the clone's config failed"*) pass "and the failure is not blamed on net0" ;;
  *) fail "and the failure is not blamed on net0 (got '$got')" ;;
esac

reset_state
printf 'runner-linux1 192.0.2.7/24 192.0.2.1 192.0.2.1\n' >"$STATIC_NET_FILE"
QM_NET0_LINE="net0: virtio=oops,bridge=vmbr1"
check_rc "a net0 with no MAC-shaped token refuses rather than guessing" 2 "found no MAC to rewrite" \
  -- apply_static_net runner-linux1 9100 linux

reset_state
printf 'runner-linux1 192.0.2.7/24 192.0.2.1 192.0.2.1\n' >"$STATIC_NET_FILE"
QM_NET0_LINE=$'net0: virtio=BC:24:11:AA:BB:CC,bridge=vmbr1\nnet0: virtio=BC:24:11:DD:EE:FF,bridge=vmbr1'
check_rc "duplicate net0 lines refuse rather than choosing one" 2 "more than one net0 line" \
  -- apply_static_net runner-linux1 9100 linux
if [ ! -e "$TMP/qm-set-args" ]; then
  pass "and qm set never ran on the ambiguity"
else
  fail "and qm set never ran on the ambiguity (args: $(cat "$TMP/qm-set-args"))"
fi

reset_state
printf 'runner-win 192.0.2.9/24 192.0.2.1 192.0.2.1\n' >"$STATIC_NET_FILE"
check_rc "a pinned windows slot reports the MAC and the router's part, address bare" 0 \
  "pinned mac BE:24:11:00:02:09; expecting the router to serve 192.0.2.9 for it" \
  -- apply_static_net runner-win 9200 windows
got=$(apply_static_net runner-win 9200 windows)
case "$got" in
  */24*) fail "and the CIDR suffix stays out of the copyable value (got '$got')" ;;
  *) pass "and the CIDR suffix stays out of the copyable value" ;;
esac
args=$(cat "$TMP/qm-set-args" 2>/dev/null)
case "$args" in
  *"--net0 virtio=BE:24:11:00:02:09,bridge=vmbr1,firewall=1,tag=67"*)
    pass "windows gets the MAC rewrite with the template's flags intact" ;;
  *) fail "windows gets the MAC rewrite with the template's flags intact (got '$args')" ;;
esac
case "$args" in
  *--ipconfig0* | *--nameserver*)
    fail "and no cloud-init arguments, which nothing in that guest consumes (got '$args')" ;;
  *) pass "and no cloud-init arguments, which nothing in that guest consumes" ;;
esac

reset_state
printf 'runner-linux1 192.0.2.7/24 192.0.2.1 192.0.2.1\n' >"$STATIC_NET_FILE"
check_rc "an unknown guest os refuses instead of defaulting to a branch" 2 "unknown guest os 'plan9'" \
  -- apply_static_net runner-linux1 9100 plan9
check_rc "a missing guest os refuses the same way" 2 "unknown guest os" \
  -- apply_static_net runner-linux1 9100
if [ ! -e "$TMP/qm-set-args" ]; then
  pass "and qm set never ran for either"
else
  fail "and qm set never ran for either (args: $(cat "$TMP/qm-set-args"))"
fi

reset_state
printf 'runner-linux1 192.0.2.7/24 192.0.2.1 192.0.2.1\n' >"$STATIC_NET_FILE"
: >"$TMP/qm-set-fail"
check_rc "a refused qm set surfaces qm's own words" 2 "some qm refusal" \
  -- apply_static_net runner-linux1 9100 linux

reset_state
printf 'runner-linux1 192.0.2.7/24 192.0.2.1\n' >"$STATIC_NET_FILE"
check_rc "a malformed entry propagates as malformed, not as unpinned" 2 "does not parse" \
  -- apply_static_net runner-linux1 9100 linux
if [ ! -e "$TMP/qm-calls" ]; then
  pass "and qm was never consulted for it either"
else
  fail "and qm was never consulted for it either (calls: $(cat "$TMP/qm-calls"))"
fi

exit "$FAILED"
