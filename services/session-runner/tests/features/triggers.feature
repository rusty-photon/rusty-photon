@serial
Feature: Triggers (the reactive overlay)
  Triggers fire while the procedure tree runs, but only at safe points: a
  trigger action never preempts an in-flight instruction. These scenarios
  observe firings through rp's SSE stream — every trigger action calls
  set_filter, and the emitted filter_switch frame's stream sequence number
  proves ordering against the exposure events.

  The fixture documents live in tests/fixtures/workflows/; each pins the
  capture counts and gate settings the expected event counts depend on.

  Scenario: A trigger action runs between exposures, never during one
    Given a running Alpaca simulator
    And rp is running with a camera and session-runner running the "trigger_between_exposures" workflow
    And an SSE client is watching rp's event stream
    When a run is started
    Then the run ends within 60 seconds
    And the SSE stream should show exactly 2 "filter_switch" events
    And no "filter_switch" event should fall between an "exposure_started" and its "exposure_complete"

  Scenario: A once trigger fires at most once per session
    Given a running Alpaca simulator
    And rp is running with a camera and session-runner running the "trigger_once" workflow
    And an SSE client is watching rp's event stream
    When a run is started
    Then the run ends within 60 seconds
    And the SSE stream should show exactly 1 "filter_switch" event

  Scenario: A cooldown suppresses firings inside its window
    Given a running Alpaca simulator
    And rp is running with a camera and session-runner running the "trigger_cooldown" workflow
    And an SSE client is watching rp's event stream
    When a run is started
    Then the run ends within 60 seconds
    And the SSE stream should show exactly 1 "filter_switch" event

  Scenario: A poll trigger fires from a polled tool result
    Given a running Alpaca simulator
    And rp is running with a camera and session-runner running the "trigger_poll" workflow
    And an SSE client is watching rp's event stream
    When a run is started
    Then the run ends within 60 seconds
    And the SSE stream should show exactly 1 "filter_switch" event
