@serial
# Test position convention: focus targets in this feature land within
# ~500 steps of OmniSim's default focuser position (25000). At
# OmniSim's simulated slew rate (~400 steps/sec), a 20k-step move
# costs ~50s; the BDD before(scenario) hook resets the focuser to
# the default before every scenario, so a round-number target like
# 5000 turns auto_focus's multi-step sweeps into a multi-minute
# wait. See docs/references/omnisim.md (Focuser section). Keep new
# auto_focus scenarios anchored near 25000 unless the test
# specifically depends on the absolute value.
Feature: Auto-focus compound tool
  The auto_focus MCP tool drives a V-curve focus sweep using move_focuser,
  capture, and measure_basic internally. It captures one frame at each
  position in the grid current_position ± half_width (in step_size
  increments), measures HFR for each via measure_basic, rejects as
  "sparse" every sample whose star_count is below min_star_fraction
  (default 0.1) of the sweep's largest star_count, fits a parabola to
  the accepted samples weighted by per-frame star_count, and moves the
  focuser to the fitted vertex. There it captures one more frame, the
  confirmation: accepted when the frame has stars, passes the same
  gate, and measures at most (1 + confirmation_tolerance) times the
  lowest accepted sweep sample (default tolerance 0.25); rejected
  otherwise, in which case the focuser moves to that lowest sample's
  position instead and the result says confirmed: false. The result
  reports the fit's weighted R² (fit_r_squared), the confirmation
  frame, and final_position / final_hfr — where the focuser ended and
  what was measured there — beside the fitted best_position /
  best_hfr. The sweep grid is clamped to the operator-supplied
  min_position / max_position bounds — points outside the bounds are
  dropped, not coerced. The tool errors before any motion when input
  parameters are missing or invalid (min_star_fraction outside
  [0, 1) and a negative confirmation_tolerance included), when the
  requested sweep would exceed the safety cap on grid size, when
  devices are unreachable, or when the clamped grid has fewer than
  min_fit_points positions. After the sweep it errors, and first
  moves the focuser back to its starting position, when fewer than
  min_fit_points samples are accepted, or when the parabolic fit
  produces no meaningful minimum inside the sampled range — that
  last case fires when the leading coefficient `a` is non-positive
  (concave-down or flat), when the design matrix is singular
  (essentially flat HFR over the sweep), or when `a > 0` but the
  fitted vertex falls outside the sampled grid (the visible curve is
  monotonic over the sampled range even if a true minimum exists
  somewhere off-grid). auto_focus does not write a section on any
  single exposure document — the per-frame image_analysis section is
  written by the embedded measure_basic call as it normally would be,
  and the compound result is returned via MCP plus a focus_complete
  event. The simulator's frames carry no detectable stars, so every
  sweep in these scenarios ends in not_enough_stars — the fit,
  gate, and confirmation outcomes are pinned by unit tests over
  synthetic frames and scripted samples instead.

  Scenario: Tool catalog includes auto_focus
    Given a running Alpaca simulator
    And rp is running with a camera and a focuser on the simulator
    And an MCP client connected to rp
    When the MCP client lists available tools
    Then the tool list should include "auto_focus"

  Scenario: auto_focus with nonexistent camera returns error
    Given a running Alpaca simulator
    And rp is running with a camera and a focuser on the simulator
    And an MCP client connected to rp
    When the MCP client calls auto_focus with camera "nonexistent" and focuser "main-focuser"
    Then the tool call should return an error
    And the error message should contain "camera not found"

  Scenario: auto_focus with nonexistent focuser returns error
    Given a running Alpaca simulator
    And rp is running with a camera and a focuser on the simulator
    And an MCP client connected to rp
    When the MCP client calls auto_focus with camera "main-cam" and focuser "nonexistent"
    Then the tool call should return an error
    And the error message should contain "focuser not found"

  Scenario: auto_focus with disconnected focuser returns error
    Given rp is running with a camera on the simulator and an unreachable focuser
    And an MCP client connected to rp
    When the MCP client calls auto_focus with camera "main-cam" and focuser "main-focuser"
    Then the tool call should return an error
    And the error message should contain "focuser not connected"

  Scenario: auto_focus with disconnected camera returns error
    Given rp is running with a focuser on the simulator and an unreachable camera
    And an MCP client connected to rp
    When the MCP client calls auto_focus with camera "main-cam" and focuser "main-focuser"
    Then the tool call should return an error
    And the error message should contain "camera not connected"

  Scenario Outline: auto_focus rejects calls missing required parameters
    Given a running Alpaca simulator
    And rp is running with a camera and a focuser on the simulator
    And an MCP client connected to rp
    When the MCP client calls auto_focus omitting "<missing_param>"
    Then the tool call should return an error
    And the error message should contain "<missing_param>"

    Examples:
      | missing_param |
      | camera_id     |
      | focuser_id    |
      | duration      |
      | step_size     |
      | half_width    |
      | min_area      |
      | max_area      |

  Scenario: auto_focus rejects step_size of 0
    Given a running Alpaca simulator
    And rp is running with a camera and a focuser on the simulator
    And an MCP client connected to rp
    When the MCP client calls auto_focus with step_size 0
    Then the tool call should return an error
    And the error message should contain "step_size"

  Scenario: auto_focus rejects half_width of 0
    Given a running Alpaca simulator
    And rp is running with a camera and a focuser on the simulator
    And an MCP client connected to rp
    When the MCP client calls auto_focus with half_width 0
    Then the tool call should return an error
    And the error message should contain "half_width"

  Scenario: auto_focus rejects min_fit_points below 3
    Given a running Alpaca simulator
    And rp is running with a camera and a focuser on the simulator
    And an MCP client connected to rp
    When the MCP client calls auto_focus with min_fit_points 2
    Then the tool call should return an error
    And the error message should contain "min_fit_points"

  Scenario: auto_focus rejects sweep grid too small after focuser bounds clamp
    Given a running Alpaca simulator
    And rp is running with a camera and a focuser on the simulator with bounds 24900..25100
    And an MCP client connected to rp
    When the MCP client calls "move_focuser" with focuser "main-focuser" to position 25000
    And the MCP client calls auto_focus with focuser "main-focuser" camera "main-cam" duration "100ms" step_size 100 half_width 500 min_area 5 max_area 65536
    Then the tool call should return an error
    And the error message should contain "min_fit_points"

  Scenario Outline: auto_focus rejects a gate or tolerance outside its range
    Given a running Alpaca simulator
    And rp is running with a camera and a focuser on the simulator
    And an MCP client connected to rp
    When the MCP client calls auto_focus with <parameter> set to <value>
    Then the tool call should return an error
    And the error message should contain "<parameter>"

    Examples:
      | parameter              | value |
      | min_star_fraction      | 1.0   |
      | min_star_fraction      | -0.1  |
      | confirmation_tolerance | -0.5  |

  Scenario: auto_focus persists every sweep frame and reports a starless sweep as not_enough_stars
    Given rp's data_directory is pinned to a fresh tempdir
    And a running Alpaca simulator
    And rp is running with a camera and a focuser on the simulator
    And an MCP client connected to rp
    When the MCP client calls "move_focuser" with focuser "main-focuser" to position 25000
    And the MCP client calls auto_focus with focuser "main-focuser" camera "main-cam" duration "100ms" step_size 100 half_width 200 min_area 5 max_area 65536
    Then the tool call should return an error
    And the error message should contain "not enough stars"
    And 5 FITS files should exist in the pinned data directory
    And every sidecar JSON in the pinned data directory should contain an "image_analysis" section
    And no sidecar JSON in the pinned data directory should contain an "auto_focus" section

  Scenario: A sweep that fails after moving returns the focuser to its starting position
    Given a running Alpaca simulator
    And rp is running with a camera and a focuser on the simulator
    And an MCP client connected to rp
    When the MCP client calls "move_focuser" with focuser "main-focuser" to position 25000
    And the MCP client calls auto_focus with focuser "main-focuser" camera "main-cam" duration "100ms" step_size 100 half_width 200 min_area 5 max_area 65536
    Then the tool call should return an error
    And the error message should contain "not enough stars"
    When the MCP client calls "get_focuser_position" with focuser "main-focuser"
    Then the get_focuser_position result position should be 25000

  # --- Train addressing (rp.md § Optical Trains): train_id resolves the
  # train's terminal camera + terminal focuser, and per-call sweep
  # parameters fall back field by field to the train's auto_focus
  # config block. The standard block used below pins duration 100ms,
  # step_size 100, half_width 200, min_area 5, max_area 65536 — a
  # 5-point grid around the focuser's current position.

  Scenario: auto_focus via train addressing uses the train's devices and config block
    Given rp's data_directory is pinned to a fresh tempdir
    And a running Alpaca simulator
    And rp is running with a camera and a focuser on the simulator in train "main" with the standard auto_focus block
    And an MCP client connected to rp
    When the MCP client calls auto_focus with train "main"
    Then 5 FITS files should exist in the pinned data directory

  Scenario: Per-call sweep parameters override the train's auto_focus block
    Given rp's data_directory is pinned to a fresh tempdir
    And a running Alpaca simulator
    And rp is running with a camera and a focuser on the simulator in train "main" with the standard auto_focus block
    And an MCP client connected to rp
    When the MCP client calls auto_focus with train "main" and step_size 50
    Then 9 FITS files should exist in the pinned data directory

  Scenario: auto_focus rejects train_id combined with an explicit device id
    Given rp is running with an offline focuser train without an auto_focus block
    And an MCP client connected to rp
    When the MCP client calls auto_focus with train "main" and camera "main-cam"
    Then the tool call should return an error
    And the error message should contain "mutually exclusive"

  Scenario: auto_focus with an unknown train returns an error
    Given rp is running with an offline focuser train without an auto_focus block
    And an MCP client connected to rp
    When the MCP client calls auto_focus with train "nonexistent"
    Then the tool call should return an error
    And the error message should contain "train not found"

  Scenario: auto_focus on a train without a focuser returns an error
    Given rp is running with an offline camera-only train
    And an MCP client connected to rp
    When the MCP client calls auto_focus with train "main"
    Then the tool call should return an error
    And the error message should contain "no focuser"

  Scenario: Train addressing without a config block still requires the sweep parameters
    Given rp is running with an offline focuser train without an auto_focus block
    And an MCP client connected to rp
    When the MCP client calls auto_focus with train "main"
    Then the tool call should return an error
    And the error message should contain "duration"
