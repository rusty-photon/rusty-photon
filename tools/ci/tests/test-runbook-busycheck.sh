#!/bin/bash
# The busy-check in the runbook decides whether an operator runs `qm stop` on a
# clone that may be working a job.
#
# What is copied here verbatim is its GUARD: the marker read, and the decision
# to refuse or to go on. That decision is the part with teeth -- a marker
# holding a PREFIX of a runner id names a different runner, so a confident
# answer about somebody else's clone reads as permission to kill a live job.
#
# The action branch is deliberately NOT exercised, and this is the limit of
# what these cases prove. It calls the GitHub API with the host's PAT, so
# running it would need a network, a credential and a live registration, and
# would stop this being a test; `probe` prints what the snippet would do
# instead. **A change to the runbook's curl or python is not covered here** --
# only a change to what decides whether they run at all.
#
# Fixtures are SYNTHETIC BY POLICY. This repository is public, and a test is
# exactly where a live runner id gets pasted while chasing a failure on the
# hypervisor. Do not: registrations are host state, and invented values
# exercise the guard exactly as well.
#
# Unlike its sibling harnesses this one reads no script -- the snippet under
# test lives in docs/skills/proxmox-runner-pool.md, and the copy below has to
# be kept in step with it by hand. It still takes the script path as an
# argument so every test in this package is invoked identically.
set -u

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
M="$TMP/9100.injected"

probe() { # the runbook's guard, verbatim apart from the marker path
  local rid=""
  IFS= read -r rid 2>/dev/null <"$M" || rid=""
  case "$rid" in '' | *[!0-9]*) rid="" ;; esac
  if [ -z "$rid" ]; then
    echo "refuses"
  else
    echo "queries $rid"
  fi
}

FAILED=0
check() { # check <desc> <want>
  local got
  got=$(probe)
  if [ "$got" = "$2" ]; then
    echo "PASS  $1"
  else
    echo "FAIL  $1 (got '$got', wanted '$2')"
    FAILED=1
  fi
}

printf '4242\n' >"$M"
check "complete marker -> asks about that runner" "queries 4242"

printf '4242' >"$M" # interrupted in-place write: a PREFIX of some id
check "unterminated marker -> refuses, does not ask about a stranger" "refuses"

: >"$M"
check "empty marker -> refuses" "refuses"

rm -f "$M"
check "missing marker -> refuses" "refuses"

printf 'abc\n' >"$M"
check "non-numeric marker -> refuses" "refuses"

exit "$FAILED"
