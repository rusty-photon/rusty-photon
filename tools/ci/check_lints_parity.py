#!/usr/bin/env python3
"""Assert the concrete `[lints]` copies match the workspace table.

The dual-homed FFI crates cannot use `[lints] workspace = true`: the nightly
publish-readiness check builds each family copied out of the workspace
(scripts/verify-publishable-crate.sh), where inheritance has nothing to
resolve against. On its L7 rung (docs/plans/archive/workspace-lints.md §L7) each
family instead adopts a concrete, verbatim copy of the root
`[workspace.lints]` table and joins OPTED_IN below; this check keeps the
copies honest with two rules:

- An OPTED_IN manifest must carry the whole workspace table, verbatim —
  a missing tool table is drift, not a smaller opt-in.
- Any other member with a concrete `[lints]` table (the shape qhyccd-rs
  carried before its rung: an `unexpected_cfgs`-only mirror that kept the
  crate publishable standalone) must mirror the workspace verbatim at
  whole-tool granularity: every `[lints.<tool>]` it declares must equal the
  workspace's `[workspace.lints.<tool>]` exactly.

Runs in the `stable / clippy` job, so the policy and its copies are enforced
by the same required PR gate. Also runnable locally with no arguments — the
repo root is derived from this file's location, and `tomllib` is stdlib on
Python >= 3.11 (an older interpreter gets a named floor error, not a
traceback).
"""

from __future__ import annotations

import sys

try:
    import tomllib
except ModuleNotFoundError:  # stdlib only since Python 3.11
    sys.exit("::error::check_lints_parity.py needs Python >= 3.11 (tomllib)")

from pathlib import Path

MECHANISM = "docs/plans/archive/workspace-lints.md §L7"

# Dual-homed manifests that have completed their L7 rung and must carry the
# full workspace table. Each family joins here in the PR that lands its copy.
OPTED_IN = frozenset(
    {
        "crates/qhyccd-rs",
        "crates/qhyccd-rs/libqhyccd-sys",
        "crates/svbony-rs",
        "crates/svbony-rs/libsvbony-sys",
        "crates/zwo-rs",
        "crates/zwo-rs/libzwo-sys",
    }
)


def load_toml(path: Path) -> dict:
    with path.open("rb") as f:
        return tomllib.load(f)


def tool_drift(member: str, tool: str, copy_tool: object, ref_tool: object) -> list[str]:
    """Name each differing entry of one `[lints.<tool>]` table."""
    if not isinstance(copy_tool, dict) or not isinstance(ref_tool, dict):
        return [f"  [lints.{tool}]: {copy_tool!r} vs workspace {ref_tool!r}"]
    lines = []
    for key in sorted(set(copy_tool) | set(ref_tool)):
        if copy_tool.get(key) != ref_tool.get(key):
            have = copy_tool.get(key, "<absent>")
            want = ref_tool.get(key, "<absent>")
            lines.append(f"  {tool}.{key}: {have!r} vs workspace {want!r}")
    return lines


def check_member(member: str, lints: object, reference: dict, full_copy: bool) -> list[str]:
    """Return drift lines for one member's concrete `[lints]` table."""
    if not isinstance(lints, dict):
        return [f"{member}: [lints] is {lints!r}, expected a table"]
    lines = []
    tools = (set(lints) | set(reference)) if full_copy else set(lints)
    for tool in sorted(tools):
        copy_tool = lints.get(tool)
        ref_tool = reference.get(tool)
        if copy_tool == ref_tool:
            continue
        if copy_tool is None:
            lines.append(f"  [lints.{tool}]: missing (an opted-in copy carries every tool table)")
            continue
        lines.extend(tool_drift(member, tool, copy_tool, ref_tool))
    if lines:
        what = "full workspace copy" if full_copy else "whole-tool mirror"
        lines.insert(0, f"{member}: [lints] differs from [workspace.lints] ({what} expected)")
    return lines


def main() -> int:
    root_dir = Path(__file__).resolve().parents[2]
    workspace = load_toml(root_dir / "Cargo.toml").get("workspace", {})

    reference = workspace.get("lints")
    if not isinstance(reference, dict) or not reference:
        print(f"::error::{root_dir / 'Cargo.toml'} has no [workspace.lints] table")
        return 1

    members = workspace.get("members", [])
    missing_roster = sorted(OPTED_IN - set(members))
    if missing_roster:
        print(f"::error::OPTED_IN entries are not workspace members: {missing_roster}")
        return 1

    verified: list[str] = []
    mirrors: list[str] = []
    offenders = 0
    failures: list[str] = []
    for member in members:
        manifest = root_dir / member / "Cargo.toml"
        opted_in = member in OPTED_IN
        if not manifest.exists():
            if opted_in:
                offenders += 1
                failures.append(f"{member}: manifest not found at {manifest}")
            continue
        lints = load_toml(manifest).get("lints")
        if opted_in and lints in (None, {"workspace": True}):
            offenders += 1
            failures.append(
                f"{member}: opted in ({MECHANISM}) but carries no concrete [lints] copy"
            )
            continue
        if lints is None or lints == {"workspace": True}:
            continue
        drift = check_member(member, lints, reference, full_copy=opted_in)
        if drift:
            offenders += 1
            failures.extend(drift)
        elif opted_in:
            verified.append(member)
        else:
            mirrors.append(member)

    if failures:
        for line in failures:
            print(line)
        print(
            f"::error::{offenders} manifest(s) out of lockstep with "
            f"[workspace.lints] (details above). Edit the root Cargo.toml "
            f"table and mirror it verbatim in each copy in the same change "
            f"({MECHANISM})."
        )
        return 1

    print(f"{len(verified)} opted-in full cop(ies) in lockstep with [workspace.lints]:")
    for member in verified:
        print(f"  {member}")
    if mirrors:
        print(f"{len(mirrors)} pre-rung whole-tool mirror(s) in lockstep:")
        for member in mirrors:
            print(f"  {member}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
