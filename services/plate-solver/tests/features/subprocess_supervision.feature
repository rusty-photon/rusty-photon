Feature: Subprocess supervision (timeout escalation, single-flight queueing)

  Every solve is bounded by a wall-clock deadline. On expiry the wrapper
  signals the child gracefully (SIGTERM on Unix, CTRL_BREAK_EVENT on
  Windows), waits a fixed 2-second grace period, then force-kills
  (SIGKILL / TerminateProcess). The wrapper always waits the child
  fully before returning; no orphaned child processes.

  Overlapping requests queue behind a single-flight semaphore (default
  capacity 1). Queue wait time is not counted against the per-request
  deadline.

  Both escalation outcomes share the frozen `solve_timeout` error code,
  so the wire discriminator between them is the `message` field:
  "(terminated)" when the child answered the graceful signal within the
  grace period, "(killed)" when it did not and was force-killed. That is
  what these scenarios assert, and it is why they carry no upper bound on
  the response time. The outcomes lie 2 s apart, which is less than the
  spread a contended CI runner puts on a loopback round trip, so no
  wall-clock ceiling tells them apart; and a ceiling is checked only
  after the response arrives, which is the one case where the wrapper
  demonstrably did not hang.

  Background:
    Given the wrapper is running with mock_astap as its solver

  Scenario: Hung child responding to graceful signal returns solve_timeout (terminated)
    Given mock_astap is configured for "hang" mode
    And a writable FITS path
    When I POST to /api/v1/solve with that fits_path and timeout "100ms"
    Then the response status is 504
    And the response field "error" is "solve_timeout"
    And the response field "message" contains "(terminated)"

  # @unix-only: Windows cannot reliably demonstrate the ignore-graceful-
  # signal precondition (the mock's SetConsoleCtrlHandler vs the wrapper's
  # CTRL_BREAK_EVENT is subject to console-attach quirks), so the child
  # dies before the 2s grace elapses -- the wrapper then reports
  # "(terminated)" and both assertions below fail. Mirrors the
  # #[cfg(unix)] gate on the equivalent test in
  # supervision_integration.rs; the wrapper's force-kill contract holds on
  # both platforms regardless.
  @unix
  Scenario: Hung child ignoring graceful signal is force-killed after grace
    Given mock_astap is configured for "ignore_sigterm" mode
    And a writable FITS path
    When I POST to /api/v1/solve with that fits_path and timeout "100ms"
    Then the response status is 504
    And the response field "error" is "solve_timeout"
    And the response field "message" contains "(killed)"
    And the response time is at least 2000 milliseconds

  # @unix-only: serialization is observed via per-child spawn-time files,
  # but on Windows a hung child's spawn file is intermittently lost when the
  # wrapper terminates it via CTRL_BREAK_EVENT. Both solves still run (both
  # return 504), so the capacity-1 semaphore itself is fine — only the
  # server-side observation is racy. The semaphore is platform-agnostic
  # tokio code, fully covered on Unix. (This step was already reworked once
  # for Windows spawn-file write-dropping; the residual race is Windows's.)
  @unix
  Scenario: Single-flight semaphore serializes overlapping solves
    Given mock_astap is configured for "hang" mode
    And a writable FITS path
    When I POST two concurrent solve requests with timeout "100ms" each
    Then both responses have status 504
    And the two solves were serialized by the single-flight semaphore
