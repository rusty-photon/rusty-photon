@serial
Feature: Calibrator flats workflow document (end-to-end)
  session-runner is the generic workflow orchestrator: a run is started
  at its own POST /runs, it loads the named workflow document, validates
  it against rp's live tool catalog, drives the session with MCP tool
  calls, and reports the outcome on GET /runs/{id}.

  These scenarios execute the shipped calibrator_flats.json first-party
  document across the same three-process topology as the calibrator-flats
  service's suite (OmniSim + rp + session-runner). That Rust orchestrator
  is the behavioral oracle: the document must produce the same events, the
  same frame counts, and the same cleanup.

  Scenario: The calibrator flats document captures flats and completes the session
    Given a running Alpaca simulator
    And a flat plan of 2 "Luminance" flats and 2 "Red" flats
    And rp is running with a camera, filter wheel, cover calibrator, and session-runner
    When a run is started
    And the workflow document runs to completion
    Then the run should report "complete"

  Scenario: The calibrator flats document emits exposure events for each flat
    Given a running Alpaca simulator
    And a test webhook receiver subscribed to "exposure_complete"
    And a flat plan of 2 "Luminance" flats and 2 "Red" flats
    And rp is running with a camera, filter wheel, cover calibrator, and session-runner
    When a run is started
    And the workflow document runs to completion
    Then the test webhook receiver should have received at least 4 "exposure_complete" events
    And the run should report "complete"

  Scenario: The calibrator flats document captures flats on a filterless rig
    Given a running Alpaca simulator
    And the cover starts open
    And a test webhook receiver subscribed to "exposure_complete"
    And a flat plan of 3 "OSC" flats with no filter wheel
    And rp is running with a camera, cover calibrator, and session-runner
    When a run is started
    And the workflow document runs to completion
    Then the test webhook receiver should have received at least 3 "exposure_complete" events
    And the run should report "complete"
    And the cover should be open

  Scenario: A cover that was closed at session start stays closed
    Given a running Alpaca simulator
    And the cover starts closed
    And a flat plan of 2 "OSC" flats with no filter wheel
    And rp is running with a camera, cover calibrator, and session-runner
    When a run is started
    And the workflow document runs to completion
    Then the run should report "complete"
    And the cover should be closed
