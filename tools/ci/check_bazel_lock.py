#!/usr/bin/env python3
"""Refuse a commit whose MODULE.bazel.lock no longer matches the Cargo files.

Bazel's `crate_universe` reads `Cargo.lock` and every workspace
`Cargo.toml`, the root's included, and `MODULE.bazel.lock` records a SHA-256
of each of those files as an input of the crate extension. Any byte change to one of them
leaves the lock stale: a new dependency, a version bump, a feature, a
comment. CI builds with `--lockfile_mode=error`, so a stale lock fails
every Bazel leg during module resolution, before anything compiles.

This compares the hashes the staged lock records against the staged files,
so the miss is caught at commit time rather than on the PR. It also refuses
a lock that recorded one of crate_universe's own environment variables as
set: a repin run with `CARGO_BAZEL_REPIN` (or a sibling) exported in the
shell writes that into the lock, and CI, which never sets them, rejects it.
The fix it asks for is `scripts/repin-bazel-lock.sh`, which refreshes the
lock and stages it. See docs/skills/pre-push.md, "Refreshing
MODULE.bazel.lock".

It checks only what the Cargo files feed into the lock. A `MODULE.bazel`
edit can also leave the lock stale, and only CI's `--lockfile_mode=error`
catches that.

Runs from the pre-commit hook (.cargo-husky/hooks/pre-commit). Also runnable
locally with no arguments. It reads the index, not the working tree,
because the index is what the commit will contain: stage your changes first.
"""

from __future__ import annotations

import hashlib
import json
import re
import subprocess
import sys
from collections.abc import Iterator
from pathlib import Path

LOCK = "MODULE.bazel.lock"
# A main-repository file input of a module extension, as Bazel records it:
# a SHA-256, or ENOENT for a file that was absent when the lock was written.
FILE_INPUT = re.compile(r"^FILE:@@//(\S+) (\S+)$")
SHA256 = re.compile(r"^[0-9a-f]{64}$")
# crate_universe's own environment inputs. CI sets none of them, so a lock
# that recorded one as set was written in a shell CI does not have.
CRATE_ENV_INPUT = re.compile(r"^ENV:(CARGO_BAZEL_\w+|REPIN) (.*)$")
UNSET = "\\0"


def staged(path: str) -> bytes | None:
    """The staged content of `path`, or None when the index has no such file."""
    result = subprocess.run(
        ["git", "show", f":{path}"],
        capture_output=True,
        check=False,
    )
    return result.stdout if result.returncode == 0 else None


def strings(node: object) -> Iterator[str]:
    """Every string anywhere in a parsed JSON document."""
    if isinstance(node, str):
        yield node
    elif isinstance(node, dict):
        for value in node.values():
            yield from strings(value)
    elif isinstance(node, list):
        for value in node:
            yield from strings(value)


def sha256(content: bytes) -> str:
    return hashlib.sha256(content).hexdigest()


def main() -> int:
    lock = staged(LOCK)
    if lock is None:
        print(f"check_bazel_lock: {LOCK} is not in the index", file=sys.stderr)
        return 1
    try:
        document = json.loads(lock)
    except ValueError:
        print(
            f"check_bazel_lock: {LOCK} is not valid JSON; a merge or rebase "
            "conflict left in it? Take either side, then run "
            "scripts/repin-bazel-lock.sh.",
            file=sys.stderr,
        )
        return 1

    files: list[tuple[str, str]] = []
    env_set: list[str] = []
    for value in strings(document):
        if match := FILE_INPUT.match(value):
            files.append((match.group(1), match.group(2)))
        elif (match := CRATE_ENV_INPUT.match(value)) and match.group(2) != UNSET:
            env_set.append(match.group(1))

    # A lock that records no inputs is not a lock that is up to date: it is
    # one this script can no longer read, most likely after a Bazel upgrade
    # changed the format. Passing it would turn the check off without a word.
    if not files:
        print(
            f"check_bazel_lock: found no file inputs in {LOCK}. "
            "Its format may have changed; update tools/ci/check_bazel_lock.py.",
            file=sys.stderr,
        )
        return 1

    toplevel = Path(
        subprocess.run(
            ["git", "rev-parse", "--show-toplevel"],
            capture_output=True,
            text=True,
            check=True,
        ).stdout.strip()
    )

    # A fingerprint this script does not know cannot be compared, and
    # skipping it would pass a lock nobody checked. Refuse it the same way as
    # a lock with no inputs at all.
    unknown = [
        f"{path} {recorded}"
        for path, recorded in files
        if recorded != "ENOENT" and not SHA256.match(recorded)
    ]
    if unknown:
        print(
            f"check_bazel_lock: {LOCK} records file inputs in a form this "
            "check does not know; update tools/ci/check_bazel_lock.py:",
            file=sys.stderr,
        )
        for entry in unknown:
            print(f"  {entry}", file=sys.stderr)
        return 1

    changed: list[str] = []
    unstaged: list[str] = []
    for path, recorded in sorted(set(files)):
        content = staged(path)
        if recorded == "ENOENT":
            if content is not None:
                changed.append(f"{path} (added)")
            continue
        if content is not None and sha256(content) == recorded:
            continue
        # The repin hashes the working tree. If that copy is the one the lock
        # recorded, the lock is right and the file is what is not staged,
        # a new member's untracked manifest included; running the repin again
        # would change nothing.
        on_disk = toplevel / path
        if on_disk.is_file() and sha256(on_disk.read_bytes()) == recorded:
            unstaged.append(path)
        elif content is None:
            changed.append(f"{path} (not in the index)")
        else:
            changed.append(path)

    if not (changed or unstaged or env_set):
        return 0

    if changed:
        print(
            f"{LOCK} is stale. These files changed since it was last refreshed:",
            file=sys.stderr,
        )
        for path in changed:
            print(f"  {path}", file=sys.stderr)
    if unstaged:
        print(
            f"{LOCK} matches the working-tree copy of these, not the staged "
            "one:",
            file=sys.stderr,
        )
        for path in unstaged:
            print(f"  {path}", file=sys.stderr)
        print(
            "If the working tree is what you mean to commit, `git add` them. "
            "If the staged copy is, put it back in the working tree and run "
            "scripts/repin-bazel-lock.sh.",
            file=sys.stderr,
        )
    if env_set:
        print(
            f"{LOCK} was refreshed with these set in the environment, "
            f"and CI runs without them: {', '.join(sorted(set(env_set)))}",
            file=sys.stderr,
        )
    if changed or env_set:
        print(
            "Run scripts/repin-bazel-lock.sh, which refreshes and stages it, "
            "then commit again.",
            file=sys.stderr,
        )
    return 1


if __name__ == "__main__":
    sys.exit(main())
