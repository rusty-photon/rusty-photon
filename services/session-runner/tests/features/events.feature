@serial
Feature: Event subscription (SSE)
  session-runner consumes rp's SSE event stream (/api/events/subscribe)
  for the whole life of a session: the subscription opens before the first
  instruction runs, so an event emitted while an earlier instruction ran
  still satisfies a later until_event wait. A wait whose event never
  arrives raises a workflow error at its timeout, and that failure ends
  the session rather than hanging it.

  The fixture documents these scenarios execute live in
  tests/fixtures/workflows/ — they pin the wait timeouts these outcomes
  depend on (5m for the satisfied wait, 2s for the expiring one).

  Scenario: An until_event wait is satisfied by an event emitted during an earlier instruction
    Given a running Alpaca simulator
    And rp is running with a camera and session-runner running the "wait_for_exposure_event" workflow
    When a run is started
    Then the run ends within 60 seconds

  Scenario: An until_event wait whose event never arrives fails the run at its timeout
    Given a running Alpaca simulator
    And rp is running with a camera and session-runner running the "wait_for_missing_event" workflow
    When a run is started
    Then the run ends within 60 seconds
