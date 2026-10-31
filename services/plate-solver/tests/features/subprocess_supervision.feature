Feature: Subprocess supervision (timeout escalation, single-flight queueing)

  Every solve is bounded by a wall-clock deadline. On expiry the wrapper
  signals the child's process tree gracefully (SIGTERM to its process
  group on Unix, CTRL_BREAK_EVENT to its console process group on
  Windows), waits a fixed 2-second grace period, then force-kills what is
  left of the tree (SIGKILL to the group / TerminateJobObject). The
  wrapper always reaps the child before returning; no orphaned child
  processes.

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
  # #[cfg(unix)] gate on the equivalent test in runner_integration.rs;
  # the wrapper's force-kill contract holds on both platforms regardless.
  @unix
  Scenario: Hung child ignoring graceful signal is force-killed after grace
    Given mock_astap is configured for "ignore_sigterm" mode
    And a writable FITS path
    When I POST to /api/v1/solve with that fits_path and timeout "100ms"
    Then the response status is 504
    And the response field "error" is "solve_timeout"
    And the response field "message" contains "(killed)"
    And the response time is at least 2000 milliseconds

  # The scenario fixes the order of the two solves instead of racing them.
  # mock_astap's "hold" mode keeps the first solve running until the scenario
  # releases it, so the second request is sent while the first holds the
  # permit. Nothing can answer the second request before the release, and the
  # release comes from the scenario, so no check below can fail because a
  # runner is slow:
  #
  # - Before the release, the second request is unanswered. Had the semaphore
  #   let it through, its solve would have timed out at 100 ms, well inside
  #   the 500 ms wait; had its timeout counted the time spent queued, it would
  #   have been answered from the queue.
  # - After the release, the second solve starts and gets its whole 100 ms:
  #   its child is terminated at its own deadline, and the answer comes at
  #   least 100 ms after the release.
  # - Only the first solve was ever held: a second solve started while the
  #   first was still held would have recorded itself as held too.
  Scenario: Single-flight semaphore serializes overlapping solves
    Given mock_astap is configured for "hold" mode
    And a writable FITS path
    When I POST a solve request with timeout "60s" in the background
    And mock_astap has started that solve
    And I POST a second solve request with timeout "100ms" in the background
    And 500 milliseconds pass
    Then the second request has not been answered
    When mock_astap is released
    Then the first request is answered with status 200
    And the second request is answered with status 504
    And the response field "message" contains "(terminated)"
    And the second request was answered at least 100 milliseconds after the release
    And mock_astap held only one solve
