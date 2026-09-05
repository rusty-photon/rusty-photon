@serial
Feature: CoverCalibrator tools
  rp exposes CoverCalibrator device operations as MCP tools. These control
  flat panel light sources and dust covers. close_cover and open_cover
  manage the dust cover. calibrator_on and calibrator_off manage the light
  source. All operations block until the device reaches the target state.
  Every tool takes exactly one of calibrator_id or train_id (the cover
  calibrator first in that optical train's device list), and every result
  carries the resolved calibrator_id and trains, the optical trains the
  cover or panel affects.

  Scenario: close_cover closes the cover successfully
    Given a running Alpaca simulator
    And rp is running with a cover calibrator on the simulator
    And an MCP client connected to rp
    When the MCP client calls "close_cover" with calibrator "flat-panel"
    Then the tool call should succeed

  Scenario: open_cover opens the cover successfully
    Given a running Alpaca simulator
    And rp is running with a cover calibrator on the simulator
    And an MCP client connected to rp
    When the MCP client calls "close_cover" with calibrator "flat-panel"
    And the MCP client calls "open_cover" with calibrator "flat-panel"
    Then the tool call should succeed

  Scenario: calibrator_on turns on the light at default brightness
    Given a running Alpaca simulator
    And rp is running with a cover calibrator on the simulator
    And an MCP client connected to rp
    When the MCP client calls "calibrator_on" with calibrator "flat-panel"
    Then the tool call should succeed

  Scenario: calibrator_on with explicit brightness succeeds
    Given a running Alpaca simulator
    And rp is running with a cover calibrator on the simulator
    And an MCP client connected to rp
    When the MCP client calls "calibrator_on" with calibrator "flat-panel" and brightness 50
    Then the tool call should succeed

  Scenario: calibrator_off turns off the light
    Given a running Alpaca simulator
    And rp is running with a cover calibrator on the simulator
    And an MCP client connected to rp
    When the MCP client calls "calibrator_on" with calibrator "flat-panel"
    And the MCP client calls "calibrator_off" with calibrator "flat-panel"
    Then the tool call should succeed

  Scenario: Tool catalog includes CoverCalibrator tools
    Given a running Alpaca simulator
    And rp is running with a cover calibrator on the simulator
    And an MCP client connected to rp
    When the MCP client lists available tools
    Then the tool list should include "close_cover"
    And the tool list should include "open_cover"
    And the tool list should include "calibrator_on"
    And the tool list should include "calibrator_off"

  Scenario: close_cover with nonexistent calibrator returns error
    Given a running Alpaca simulator
    And rp is running with a cover calibrator on the simulator
    And an MCP client connected to rp
    When the MCP client calls "close_cover" with calibrator "nonexistent"
    Then the tool call should return an error
    And the error message should contain "calibrator not found"

  Scenario: close_cover with disconnected calibrator returns error
    Given rp is running with a cover calibrator at "http://localhost:1" device 0
    And an MCP client connected to rp
    When the MCP client calls "close_cover" with calibrator "flat-panel"
    Then the tool call should return an error
    And the error message should contain "calibrator not connected"

  Scenario: close_cover with missing calibrator_id returns error
    Given a running Alpaca simulator
    And rp is running with a cover calibrator on the simulator
    And an MCP client connected to rp
    When the MCP client calls "close_cover" with no calibrator_id
    Then the tool call should return an error
    And the error message should contain "calibrator_id"

  # --- Train addressing (calibrator-flats-provider plan, D4) ---
  # Every calibrator tool takes exactly one of calibrator_id or train_id;
  # train_id resolves the calibrator first in that train's device list.
  # Every result carries the resolved calibrator_id and trains, the
  # optical trains containing it — a closed cover blinds every camera
  # behind it and a lit panel floods them.

  Scenario: close_cover by train_id closes the train's calibrator
    Given a running Alpaca simulator
    And rp is running with a camera and a cover calibrator on the simulator in an imaging train
    And an MCP client connected to rp
    When the MCP client calls "close_cover" with train "main"
    Then the tool call should succeed
    And the tool result "calibrator_id" should be "flat-panel"
    And the tool result "status" should be "closed"
    And the tool result list "trains" should be exactly "main"

  Scenario: A shared calibrator reports both trains on close
    Given a running Alpaca simulator
    And rp is running with a cover calibrator on the simulator shared by the trains "main" and "guide"
    And an MCP client connected to rp
    When the MCP client calls "close_cover" with calibrator "flat-panel"
    Then the tool call should succeed
    And the tool result list "trains" should be exactly "main,guide"

  Scenario: get_cover_state by train_id reads the train's calibrator
    Given a running Alpaca simulator
    And rp is running with a camera and a cover calibrator on the simulator in an imaging train
    And an MCP client connected to rp
    When the MCP client calls "close_cover" with train "main"
    And the MCP client calls "get_cover_state" with train "main"
    Then the tool call should succeed
    And the tool result "calibrator_id" should be "flat-panel"
    And the tool result "cover_state" should be "Closed"

  Scenario: calibrator_on by train_id lights the train's panel
    Given a running Alpaca simulator
    And rp is running with a camera and a cover calibrator on the simulator in an imaging train
    And an MCP client connected to rp
    When the MCP client calls "calibrator_on" with train "main"
    And the MCP client calls "calibrator_off" with train "main"
    Then the tool call should succeed
    And the tool result "calibrator_id" should be "flat-panel"
    And the tool result "status" should be "off"

  Scenario: A train without a cover calibrator is an error naming the train
    Given a running Alpaca simulator
    And rp is running with a camera in an imaging train and a cover calibrator outside every train on the simulator
    And an MCP client connected to rp
    When the MCP client calls "close_cover" with train "main"
    Then the tool call should return an error
    And the error message should contain "train 'main' has no cover calibrator"

  Scenario: close_cover with both calibrator_id and train_id is rejected
    Given a running Alpaca simulator
    And rp is running with a camera and a cover calibrator on the simulator in an imaging train
    And an MCP client connected to rp
    When the MCP client calls "close_cover" with both calibrator "flat-panel" and train "main"
    Then the tool call should return an error
    And the error message should contain "train_id is mutually exclusive with calibrator_id"
