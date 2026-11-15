@wip @serial
Feature: Guide-train focus on the guider's metrics
  rp's auto_focus is the guiding train's sweep, and it never captures
  through the guide camera: addressed with the guiding train's id, it
  moves the train's terminal focuser and samples the median HFD of
  fresh guide frames from the guider's GET /api/v1/guiding/metrics at
  each position, then fits the hyperbolic V-curve. The guider measures
  those frames only in a run, so the sweep requires the guider's stats
  to report the state guiding, and it is refused in every other state
  before anything moves. The guide corrections stay active for the
  whole sweep: rp pauses nothing, stops nothing and asks for no camera
  lease. focus_started names the guiding train, no camera, and the
  method guider_hfd. A fit that fails moves the focuser back to where
  the sweep started.

  The guide focus watch, the guiding config's focus_watch, turns a
  degrading HFD trend into events and never into actions. It polls the
  guider's metrics while the guider reports the state guiding and is
  idle otherwise. rp emits guide_focus_degraded once the trailing
  median exceeds the baseline times degrade_ratio, and
  guide_focus_escalation when the episode is still degraded
  escalation_deadline later, each naming the guiding train.

  The stub guider's metrics report fresh frames at one flat HFD unless
  a scenario gives it a script, so a sweep's fit reports no minimum, a
  monotonic_curve error. A script names the HFD of every frame in one
  metrics answer after another, and its last value goes on for every
  answer after it.

  Scenario: The sweep samples the guider's metrics, reports a flat curve honestly and puts the focuser back
    Given a running Alpaca simulator
    And a stub guider in state "guiding"
    And a test webhook receiver subscribed to the events "focus_started, focus_failed"
    And rp is running with a focuser on the simulator in guiding train "guide" with a metric auto_focus block
    And an MCP client connected to rp
    When the MCP client calls auto_focus with train "guide"
    Then the tool call should return an error
    And the error message should contain "monotonic"
    And the stub guider should have received at least 5 "/guiding/metrics" requests
    And the test webhook receiver should receive a "focus_started" event
    And the "focus_started" event payload field "method" should be "guider_hfd"
    And the "focus_started" event payload field "train_id" should be "guide"
    And the "focus_started" event payload field "camera_id" should be the JSON null
    And the test webhook receiver should receive a "focus_failed" event
    And the stub guider should not have received a pause request
    And the stub guider should not have received a "/guiding/stop" request
    And the stub guider should not have received a "/camera/lease" request
    When the MCP client calls "get_focuser_position" with focuser "main-focuser"
    Then the get_focuser_position result position should be 25000

  Scenario Outline: The sweep is refused while the guider is <state>, before anything moves
    Given a running Alpaca simulator
    And a stub guider in state "<state>"
    And rp is running with a focuser on the simulator in guiding train "guide" with a metric auto_focus block
    And an MCP client connected to rp
    When the MCP client calls auto_focus with train "guide"
    Then the tool call should return an error
    And the error message should contain "guide-train auto_focus requires active guiding"
    When the MCP client calls "get_focuser_position" with focuser "main-focuser"
    Then the get_focuser_position result position should be 25000

    Examples:
      | state       |
      | idle        |
      | looping     |
      | calibrating |
      | paused      |
      | star_lost   |

  Scenario: The guide focus watch turns a degrading HFD trend into events naming the guiding train
    Given a running Alpaca simulator
    And a stub guider in state "guiding"
    And the stub guider serves the HFD script "2.0,3.0"
    And a test webhook receiver subscribed to the events "guide_focus_degraded, guide_focus_escalation"
    And rp is running with guiding train "guide" and a guide focus watch of window 3, poll interval "250ms", and escalation deadline "1s"
    Then the test webhook receiver should receive a "guide_focus_degraded" event
    And the "guide_focus_degraded" event payload field "train_id" should be "guide"
    And the "guide_focus_degraded" event payload should contain a "baseline_hfd"
    And the "guide_focus_degraded" event payload should contain a "current_hfd"
    And the test webhook receiver should receive a "guide_focus_escalation" event
    And the "guide_focus_escalation" event payload field "train_id" should be "guide"

  Scenario: The guide focus watch is idle while the guider is not guiding
    Given a running Alpaca simulator
    And a stub guider in state "paused"
    And the stub guider serves the HFD script "2.0,3.0"
    And a test webhook receiver subscribed to the events "guide_focus_degraded, guide_focus_escalation"
    And rp is running with guiding train "guide" and a guide focus watch of window 3, poll interval "250ms", and escalation deadline "1s"
    Then no "guide_focus_degraded" event arrives within 3000 ms
