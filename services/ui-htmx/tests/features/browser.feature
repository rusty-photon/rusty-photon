@browser
Feature: Real-browser behavior
  These scenarios drive a real headless Firefox via WebDriver to prove the one
  thing the server-output layers cannot: that the vendored htmx.min.js actually
  loads and executes the declared swaps in a browser engine (obligation P3).
  They are advisory and gated behind UI_BROWSER_TESTS=1, and run on a single
  environment — the server-bytes layers (P1 correctness, P2 OS-invariance) carry
  the cross-OS guarantee, so behavior proven correct here holds on every OS
  without a browser on every OS.

  Scenario: The stream page renders in a real browser
    Given a running rp orchestrator with an empty roster
    And a BFF pointed at rp
    When I load the stream page in a browser
    Then the browser renders the activity stream

  # rp replays its retained history to a fresh subscriber, so the card arrives
  # whether the flip lands before or after the browser's EventSource connects.
  Scenario: An rp event lands in the feed through htmx's SSE swap
    Given a running rp with a stub safety monitor and an empty roster
    And a BFF pointed at rp
    When I load the stream page in a browser
    And the safety monitor reports unsafe on rp
    Then the browser's feed shows a card mentioning "Safety: UNSAFE"

  # Tier 0 step 3 (plan §9): quitting the browser before stopping the BFF lets the
  # BFF shut down gracefully, so it runs its atexit handler and flushes coverage.
  # Reversing the order would block the BFF on the browser's held connection until
  # the 5s SIGKILL grace and silently zero its BDD coverage (testing.md §5.4).
  Scenario: The BFF flushes coverage on graceful shutdown with a browser attached
    Given a running rp orchestrator with an empty roster
    And a BFF pointed at rp
    When I load the stream page in a browser
    And I quit the browser and then stop the BFF
    Then the BFF shuts down gracefully before the 5s SIGKILL grace elapses

  # Tier 0 step 4 (plan §9): the worst case — geckodriver dies without telling
  # Firefox to quit (a panic/timeout, or the Firefox<152 SIGTERM bug). The
  # kill-the-tree reaper (killpg of geckodriver's process group) must still leave
  # zero orphans, and failure-triage artifacts must land at an absolute path first.
  # Linux-only: the orphan scan reads /proc. Like the rest of @browser it runs on
  # Linux CI + dev; macOS/Windows browser support is plan §9 step 5 (deferred), and
  # forcing UI_BROWSER_TESTS=1 there fails loud rather than silently passing.
  Scenario: A crashed browser session is reaped with no orphaned processes
    Given a running rp orchestrator with an empty roster
    And a BFF pointed at rp
    When I load the stream page in a browser
    And the geckodriver process is killed and the session is reaped
    Then the reaper leaves no orphaned browser processes
    And the failure artifacts were captured at an absolute path before the reap
