@serial
Feature: Guide-train focus via PHD2 metrics
  rp's auto_focus is the guiding train's sweep, and it never captures
  through the guide camera: addressed with the guiding train's id, it
  moves the train's terminal focuser and samples PHD2's per-frame HFD
  from the guider service's metrics window — the median of
  frames_per_step fresh frames per position — then fits the hyperbolic
  V-curve, the defocus curve the focus-model provider's capture sweep
  fits too; it reports the fit's fit_r_squared and enforces no
  threshold. It requires an active guide loop (PHD2 only emits
  GuideStep while guiding), and corrections stay active for the whole
  sweep. On success the focuser moves to the fitted minimum and one
  more sample set collected there confirms it, falling back to the
  lowest sweep sample when it measures worse than the tolerance
  allows; a fit that fails moves the focuser back to where the sweep
  started. An imaging train is refused: the focus-model provider's
  focus_train focuses it. The sweep moves only a focuser no imaging
  train holds: one an imaging train holds is focused through the
  imaging camera alone, the guide camera taken as parfocal with it, so
  the sweep refuses it before anything moves, naming the imaging
  train. The stub guider in these scenarios serves perfectly flat HFD,
  so the fit deterministically reports no minimum (a monotonic_curve
  error) — the success-path payload is pinned by unit tests over
  scripted V-curves instead.

  The guide focus watch (equipment.mount.guiding.focus_watch) turns
  a degrading HFD trend into events, never actions: rp emits
  guide_focus_degraded once the trailing median exceeds baseline
  times degrade_ratio, and guide_focus_escalation when the episode
  is still degraded escalation_deadline later. Orchestrators wire
  those events to a refocus; rp never moves a focuser on its own
  initiative.

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

  Scenario: auto_focus refuses an imaging train, naming focus_train
    Given a running Alpaca simulator
    And a stub guider returning canned guiding stats
    And rp is running with a focuser on the simulator shared by imaging train "main" and guiding train "guide"
    And an MCP client connected to rp
    When the MCP client calls auto_focus with train "main"
    Then the tool call should return an error
    And the error message should contain "train 'main' is an imaging train"
    And the error message should contain "focus_train"
    And the stub guider should not have received a "/guiding/metrics" request

  Scenario: A guiding train without a metric block requires per-call geometry
    Given a running Alpaca simulator
    And a stub guider returning canned guiding stats
    And rp is running with a focuser on the simulator in guiding train "guide" without an auto_focus block
    And an MCP client connected to rp
    When the MCP client calls auto_focus with train "guide"
    Then the tool call should return an error
    And the error message should contain "step_size"

  # An unmotorised OAG: the guiding train's only focuser is the imaging
  # train's, which only the imaging camera focuses.
  Scenario: Guide-train auto_focus refuses a focuser an imaging train holds, before anything moves
    Given a running Alpaca simulator
    And a stub guider returning canned guiding stats
    And rp is running with a focuser on the simulator shared by imaging train "main" and guiding train "guide"
    And an MCP client connected to rp
    When the MCP client calls auto_focus with train "guide"
    Then the tool call should return an error
    And the error message should contain "guide-train auto_focus: focuser 'main-focuser' is in imaging train 'main', which focuses it"
    And the stub guider should not have received a "/guiding/metrics" request

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
