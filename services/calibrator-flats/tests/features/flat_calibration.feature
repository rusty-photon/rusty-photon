@serial
Feature: Calibrator flat field workflow (end-to-end)
  The calibrator-flats orchestrator is a real service that connects to rp
  as an MCP client. A run is started at its own POST /runs; it closes
  the cover, turns on the calibrator, iteratively finds the optimal
  exposure time per filter, captures flat frames, then turns off the
  calibrator and opens the cover, and reports the outcome on its own
  GET /status.

  These tests start all three processes (OmniSim, rp, calibrator-flats)
  and verify the full workflow end-to-end.

  Scenario: Calibrator-flats orchestrator captures flats and completes the run
    Given a running Alpaca simulator
    And the calibrator-flats service is configured for 2 "Luminance" flats and 2 "Red" flats
    And rp is running with a camera, filter wheel, cover calibrator, and the calibrator-flats orchestrator
    When a run is started
    And the calibrator-flats run ends
    Then the calibrator-flats status should report "complete"

  Scenario: Calibrator-flats orchestrator emits exposure events for each flat
    Given a running Alpaca simulator
    And a test webhook receiver subscribed to "exposure_complete"
    And the calibrator-flats service is configured for 2 "Luminance" flats and 2 "Red" flats
    And rp is running with a camera, filter wheel, cover calibrator, and the calibrator-flats orchestrator
    When a run is started
    And the calibrator-flats run ends
    Then the test webhook receiver should have received at least 4 "exposure_complete" events
    And the calibrator-flats status should report "complete"

  Scenario: Calibrator-flats orchestrator captures flats on a filterless rig
    Given a running Alpaca simulator
    And the cover starts open
    And a test webhook receiver subscribed to "exposure_complete"
    And the calibrator-flats service is configured for 3 "OSC" flats with no filter wheel
    And rp is running with a camera, cover calibrator, and the calibrator-flats orchestrator
    When a run is started
    And the calibrator-flats run ends
    Then the test webhook receiver should have received at least 3 "exposure_complete" events
    And the calibrator-flats status should report "complete"
    And the cover should be open

  Scenario: A cover that was closed at session start stays closed
    Given a running Alpaca simulator
    And the cover starts closed
    And the calibrator-flats service is configured for 2 "OSC" flats with no filter wheel
    And rp is running with a camera, cover calibrator, and the calibrator-flats orchestrator
    When a run is started
    And the calibrator-flats run ends
    Then the calibrator-flats status should report "complete"
    And the cover should be closed
