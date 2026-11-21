@requires-astap
Feature: Real-ASTAP smoke (mock-honesty backstop)

  Three scenarios exercise the wrapper against the operator's real
  ASTAP install. Gated by the `@requires-astap` tag — they fire only
  when `ASTAP_BINARY` is set in the environment. The nightly
  cross-platform smoke workflow sets that var via the `install-astap`
  action; the pull-request jobs do not, so they skip these scenarios.

  Two M 101 scenarios — one blind, one with explicit hints — solve the
  same fixture and record per-scenario wall-clock duration. The gap
  between the two is what the hints buy, and the smoke workflow guards
  it (recorded into the CSV file named by the `PLATE_SOLVER_PERF_CSV`
  env var when set; no-op when unset, so local BDD runs aren't burdened).

  The third scenario asserts the wrapper surfaces `solve_failed` on
  ASTAP's expected failure mode (degenerate input with no stars).

  See `docs/services/plate-solver.md` § Real-ASTAP Coverage Cadence for
  the fixture, the timings and their guard.

  Scenario: Real ASTAP solves the M 101 fixture blind (no hints)
    # `m101_known.fits` is a 1024×1024 centered crop of a real NINA
    # capture (FSQ106 + QHY600M, B filter, 120 s, 2025-05-20 session),
    # downsized from ~117 MB to ~2 MB while keeping enough stars to
    # solve (88 stars, 4/4 quad match). The fixture's pointing
    # breadcrumbs (RA, DEC, OBJCTRA, OBJCTDEC, OBJECT) are stripped,
    # so "no hint flags on the request" produces a truly blind
    # solve — ASTAP walks the search spiral from an unbiased start
    # position. On the reference Linux box this takes
    # ~48 s vs ~0.06 s with explicit hints (the next scenario);
    # cross-platform numbers land in the CSV artifact. The assertion
    # tolerance (0.01°) is far wider than the observed solve scatter,
    # so the test is robust against version drift in ASTAP's
    # quad-search ordering.
    Given the wrapper is running with the real ASTAP_BINARY as its solver
    And the m101_known.fits fixture is on disk
    # 300 s = generous enough that the slower matrix legs (macOS, Windows)
    # don't time out before producing a real measurement. The reference
    # Linux box completes in ~48 s.
    When I POST to /api/v1/solve with that fits_path and timeout "300s"
    Then the response status is 200
    And the response field "ra_center" is approximately 210.8099 within 0.01 degrees
    And the response field "dec_center" is approximately 54.3469 within 0.01 degrees
    And the response field "solver" contains "astap" case-insensitively
    And I record the solve duration as "m101_blind"

  Scenario: Real ASTAP solves the M 101 fixture with explicit hints
    # Same fixture, same expected solution — but the request body
    # carries ra_hint/dec_hint/fov_hint_deg/search_radius_deg. ASTAP's
    # search radius drops from 180° (blind default) to 3° around the
    # supplied start position, producing a ~0.06 s solve. The hint
    # values match the operator-curl recipe in
    # docs/services/plate-solver.md §"Hint sources and search-radius
    # defaults" so the canonical numbers stay in one place.
    Given the wrapper is running with the real ASTAP_BINARY as its solver
    And the m101_known.fits fixture is on disk
    When I POST to /api/v1/solve with that fits_path and these hints:
      | field             | value    |
      | ra_hint           | 210.8099 |
      | dec_hint          | 54.3469  |
      | fov_hint_deg      | 0.42     |
      | search_radius_deg | 3.0      |
    Then the response status is 200
    And the response field "ra_center" is approximately 210.8099 within 0.01 degrees
    And the response field "dec_center" is approximately 54.3469 within 0.01 degrees
    And the response field "solver" contains "astap" case-insensitively
    And I record the solve duration as "m101_hinted"

  Scenario: Real ASTAP returns solve_failed on a degenerate FITS
    Given the wrapper is running with the real ASTAP_BINARY as its solver
    And the degenerate_no_stars.fits fixture is on disk
    When I POST to /api/v1/solve with that fits_path
    Then the response status is 422
    And the response field "error" is "solve_failed"
