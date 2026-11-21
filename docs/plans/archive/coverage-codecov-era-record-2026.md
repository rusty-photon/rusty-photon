# coverage: the Codecov era and its lessons, to 2026-08-30

A record, moved whole from `docs/skills/coverage.md` on 2026-11-21: what
Codecov, the coverage vendor before Coveralls, taught the coverage gate,
and where the coverage history of that time stayed. It stood under
[coverage.md § History: the Codecov era (through 2026-08-30)](../../skills/coverage.md);
that skill states the three lessons as facts of the gate today — under
§ The gating policy, the badge paragraph of § 2 and § Gotchas — with
Coveralls the sole coverage vendor. "The Coveralls decrease threshold" the
text names is coverage.md's § The gating policy.

## History: the Codecov era (through 2026-08-30)

Codecov was the vendor before Coveralls, and three of its lessons are kept so
nobody re-derives them:

- Its required `codecov/patch` check enforced diff-coverage ≥ the base's
  project coverage (`target: auto`) — the same ratchet intent the Coveralls
  decrease threshold now carries. **When the Coveralls migration merged, the
  `codecov/patch` entry had to leave the `main_protection` ruleset**, or every
  PR would block on a check that never reports again.
- `codecov/project` stopped posting when the repo moved to the
  `rusty-photon` org (2026-08-02): the free tier does not include Project
  Coverage. It was never a configuration problem.
- The 21 per-service flag badges spent 08-22→08-30 all rendering the
  identical repo-wide number: the codecov CLI ran without `--disable-search`,
  so the `combined-coverage.info` file created for Coveralls was auto-added
  to **every** flag upload. Per-service badges were removed with the
  migration (Coveralls has no per-flag badges); per-file detail lives in the
  Coveralls UI instead.

Pre-transfer history (through commit `bf00fe3c`) is frozen in the
`ivonnyssen/rusty-photon` Codecov project; 2026-08-02→2026-08-21 has no
coverage data anywhere. Nothing should link either Codecov project any more.
