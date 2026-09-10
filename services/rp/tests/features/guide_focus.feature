@serial
Feature: Guide-train focus via PHD2 metrics
  Auto-focus on the guiding train never captures through the guide
  camera: addressed with the guiding train's id, auto_focus moves the
  train's terminal focuser and samples PHD2's per-frame HFD from the
  guider service's metrics window — the median of frames_per_step
  fresh frames per position — then fits the same V-curve as the
  capture sweep. It requires an active guide loop (PHD2 only emits
  GuideStep while guiding), and corrections stay active for the whole
  sweep. On success the focuser moves to the fitted minimum and one
  more sample set collected there confirms it, with the same
  tolerance and fallback to the lowest sweep sample as the capture
  sweep; a fit that fails moves the focuser back to where the sweep
  started. The capture-only parameters, the sparse gate
  (min_star_fraction) included, are rejected for this train.
  refocus_train expansions run a guiding-train step the same way,
  always last and never under paused corrections. The stub guider in
  these scenarios serves perfectly flat HFD, so the fit
  deterministically reports no minimum (a monotonic_curve error) —
  the success-path payload is pinned by unit tests over scripted
  V-curves instead.

  The guide focus watch (equipment.mount.guiding.focus_watch) turns
  a degrading HFD trend into events, never actions: rp emits
  guide_focus_degraded once the trailing median exceeds baseline
  times degrade_ratio, and guide_focus_escalation when the episode
  is still degraded escalation_deadline later. Orchestrators wire
  those events to refocus_train; rp never moves a focuser on its
  own initiative.

  Scenario: Guide-train auto_focus sweeps the focuser against PHD2 metrics and reports a flat curve honestly
    Given a running Alpaca simulator
    And a stub guider returning canned guiding stats
    And a test webhook receiver subscribed to the events "focus_started, focus_failed"
    And rp is running with a focuser on the simulator in guiding train "guide" with a metric auto_focus block
    And an MCP client connected to rp
    When the MCP client calls auto_focus with train "guide"
    Then the tool call should return an error
    And the error message should contain "monotonic"
    And the stub guider should have received at least 5 "/guiding/metrics" requests
    And the test webhook receiver should receive a "focus_started" event
    And the "focus_started" event payload field "method" should be "phd2_hfd"
    And the test webhook receiver should receive a "focus_failed" event
    When the MCP client calls "get_focuser_position" with focuser "main-focuser"
    Then the get_focuser_position result position should be 25000

  Scenario: Guide-train auto_focus requires an active guide loop
    Given a running Alpaca simulator
    And a stub guider reporting guiding inactive
    And rp is running with a focuser on the simulator in guiding train "guide" with a metric auto_focus block
    And an MCP client connected to rp
    When the MCP client calls auto_focus with train "guide"
    Then the tool call should return an error
    And the error message should contain "requires active guiding"

  Scenario: Capture-only sweep parameters are rejected for the guiding train
    Given a running Alpaca simulator
    And a stub guider returning canned guiding stats
    And rp is running with a focuser on the simulator in guiding train "guide" with a metric auto_focus block
    And an MCP client connected to rp
    When the MCP client calls auto_focus with train "guide" and duration "3s"
    Then the tool call should return an error
    And the error message should contain "capture-based"

  Scenario: The sparse-sample gate is rejected for the guiding train
    Given a running Alpaca simulator
    And a stub guider returning canned guiding stats
    And rp is running with a focuser on the simulator in guiding train "guide" with a metric auto_focus block
    And an MCP client connected to rp
    When the MCP client calls auto_focus with train "guide" and min_star_fraction 0.2
    Then the tool call should return an error
    And the error message should contain "capture-based"

  Scenario: A guiding train without a metric block requires per-call geometry
    Given a running Alpaca simulator
    And a stub guider returning canned guiding stats
    And rp is running with a focuser on the simulator in guiding train "guide" without an auto_focus block
    And an MCP client connected to rp
    When the MCP client calls auto_focus with train "guide"
    Then the tool call should return an error
    And the error message should contain "step_size"

  Scenario: refocus_train runs the guiding train's metric step without pausing corrections
    Given a running Alpaca simulator
    And a stub guider returning canned guiding stats
    And rp is running with a focuser on the simulator in guiding train "guide" with a metric auto_focus block
    And an MCP client connected to rp
    When the MCP client calls "refocus_train" with train "guide"
    Then the tool call should return an error
    And the error message should contain "step 1 (focuser 'main-focuser' in train 'guide') failed:"
    And the stub guider should not have received a pause request

  Scenario: refocus_train with a guiding-train step refuses while guiding is idle
    Given a running Alpaca simulator
    And a stub guider reporting guiding inactive
    And rp is running with a focuser on the simulator in guiding train "guide" with a metric auto_focus block
    And an MCP client connected to rp
    When the MCP client calls "refocus_train" with train "guide"
    Then the tool call should return an error
    And the error message should contain "requires active guiding"

  Scenario: The guide focus watch turns a degrading HFD trend into events naming the guiding train
    Given a running Alpaca simulator
    And a stub guider with the HFD script "2.0,3.0"
    And a test webhook receiver subscribed to the events "guide_focus_degraded, guide_focus_escalation"
    And rp is running with guiding train "guide" and a guide focus watch of window 3, poll interval "250ms", and escalation deadline "1s"
    Then the test webhook receiver should receive a "guide_focus_degraded" event
    And the "guide_focus_degraded" event payload field "train_id" should be "guide"
    And the "guide_focus_degraded" event payload should contain a "baseline_hfd"
    And the "guide_focus_degraded" event payload should contain a "current_hfd"
    And the test webhook receiver should receive a "guide_focus_escalation" event
    And the "guide_focus_escalation" event payload field "train_id" should be "guide"

  Scenario: A stable HFD trend never fires the watch
    Given a stub guider with the HFD script "2.0"
    And a test webhook receiver subscribed to the events "guide_focus_degraded, guide_focus_escalation"
    And rp is running with a guide focus watch of window 3, poll interval "250ms", and escalation deadline "1s"
    Then the test webhook receiver should not have received a "guide_focus_degraded" event

  Scenario: start_guiding warns when the guide camera is rotator-coupled but PHD2 has no rotator
    Given a running Alpaca simulator
    And a stub guider returning canned guiding stats
    And a test webhook receiver subscribed to the events "guide_started, guide_rotator_unmodeled"
    And rp is running with a rotator on the simulator inside guiding train "guide"
    And an MCP client connected to rp
    When the MCP client calls "start_guiding" with no arguments
    Then the test webhook receiver should receive a "guide_rotator_unmodeled" event
    And the "guide_rotator_unmodeled" event payload field "rotator_id" should be "main-rotator"

  Scenario: No rotator warning when PHD2 has the rotator connected
    Given a running Alpaca simulator
    And a stub guider with a connected PHD2 rotator
    And a test webhook receiver subscribed to the events "guide_started, guide_rotator_unmodeled"
    And rp is running with a rotator on the simulator inside guiding train "guide"
    And an MCP client connected to rp
    When the MCP client calls "start_guiding" with no arguments
    Then the test webhook receiver should receive a "guide_started" event
    And the test webhook receiver should not have received a "guide_rotator_unmodeled" event
