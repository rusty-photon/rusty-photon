@serial
Feature: Guide-train focus: what the sweep refuses, and a steady guide focus watch
  rp's auto_focus is the guiding train's sweep on the guider's metrics
  (guide_metric_focus.feature). An imaging train is refused: the
  focus-model provider's focus_train focuses it. The sweep moves only a
  focuser no imaging train holds: one an imaging train holds is focused
  through the imaging camera alone, the guide camera taken as parfocal
  with it, so the sweep refuses it before anything moves, naming the
  imaging train. A guiding train without a metric auto_focus block
  needs the sweep's geometry in the call. Each of these refusals comes
  before the sweep reads the guider's metrics.

  The guide focus watch (equipment.mount.guiding.focus_watch) turns a
  degrading HFD trend into events and never into actions, so a trend
  that holds steady fires nothing.

  Scenario: auto_focus refuses an imaging train, naming focus_train
    Given a running Alpaca simulator
    And a stub guider in state "guiding"
    And rp is running with a focuser on the simulator shared by imaging train "main" and guiding train "guide"
    And an MCP client connected to rp
    When the MCP client calls auto_focus with train "main"
    Then the tool call should return an error
    And the error message should contain "train 'main' is an imaging train"
    And the error message should contain "focus_train"
    And the stub guider should not have received a "/guiding/metrics" request

  Scenario: A guiding train without a metric block requires per-call geometry
    Given a running Alpaca simulator
    And a stub guider in state "guiding"
    And rp is running with a focuser on the simulator in guiding train "guide" without an auto_focus block
    And an MCP client connected to rp
    When the MCP client calls auto_focus with train "guide"
    Then the tool call should return an error
    And the error message should contain "step_size"

  # An unmotorised OAG: the guiding train's only focuser is the imaging
  # train's, which only the imaging camera focuses.
  Scenario: Guide-train auto_focus refuses a focuser an imaging train holds, before anything moves
    Given a running Alpaca simulator
    And a stub guider in state "guiding"
    And rp is running with a focuser on the simulator shared by imaging train "main" and guiding train "guide"
    And an MCP client connected to rp
    When the MCP client calls auto_focus with train "guide"
    Then the tool call should return an error
    And the error message should contain "guide-train auto_focus: focuser 'main-focuser' is in imaging train 'main', which focuses it"
    And the stub guider should not have received a "/guiding/metrics" request

  Scenario: A stable HFD trend never fires the watch
    Given a stub guider in state "guiding"
    And the stub guider serves the HFD script "2.0"
    And a test webhook receiver subscribed to the events "guide_focus_degraded, guide_focus_escalation"
    And rp is running with a guide focus watch of window 3, poll interval "250ms", and escalation deadline "1s"
    Then no "guide_focus_degraded" event arrives within 3000 ms
