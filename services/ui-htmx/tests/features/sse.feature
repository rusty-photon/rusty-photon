@browser
Feature: Server-Sent-Events streaming the server-bytes layers cannot observe
  These @browser scenarios drive a test-only /fixtures/sse route set (compiled only
  under the `test-sse` cargo feature — it ships nothing) plus the vendored htmx SSE
  extension to prove two things only a real browser can establish: that async
  server-pushed DOM updates land over a live stream, and that an open SSE stream —
  which never closes on the shutdown signal (docs/services/ui-htmx.md, "The SSE
  proxy") — still permits a graceful, coverage-flushing BFF shutdown when the
  browser is quit first. Like the rest of the browser layer they are advisory and
  run behind UI_BROWSER_TESTS=1.

  # One EventSource (sse-connect) feeds TWO regions via named events (sse-swap).
  # There are no server "bytes" for P1/P2 to assert — the updates arrive over the
  # live stream after the page loads — so only the browser proves both regions
  # update from the single connection.
  Scenario: Two regions update from a single SSE connection
    Given the ui-htmx BFF is running
    When I load the "/fixtures/sse" fixture in a browser
    Then the "#region-a" region shows "alpha pushed"
    And the "#region-b" region shows "beta pushed"

  # The decisive teardown/coverage proof. With the browser holding an open SSE
  # stream, quitting the browser first drops the connection so the BFF's
  # graceful shutdown completes promptly and runs its atexit coverage flush. The
  # wrong order would block the BFF on the held stream until the 5s SIGKILL grace
  # and silently zero its coverage (testing.md §5.4) — the SSE case has no
  # in-process escape hatch, since the connection is held out-of-process.
  Scenario: An open SSE stream still allows a graceful shutdown when the browser quits first
    Given the ui-htmx BFF is running
    When I load the "/fixtures/sse" fixture in a browser
    And the SSE stream has pushed "alpha pushed" into the "#region-a" region
    And I quit the browser and then stop the BFF
    Then the BFF shuts down gracefully before the 5s SIGKILL grace elapses
