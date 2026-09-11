@serial
Feature: Focus tools served through rp
  focus-model is a tool provider: rp dials it at startup, merges
  focus_train, get_sweep_plan, get_focus_model, get_focus_runs,
  set_focus_offsets and reset_focus_model into its catalog, and proxies
  calls to it. Every tool takes a train_id and resolves the train's
  terminal focuser, camera, filter wheel and optical facts through rp's
  own get_train_info; the provider then drives the sweep by calling
  rp's primitive tools as an MCP client. The sweep is sized from the
  critical focus zone of the optics at the filter's wavelength, its
  start is predicted from the remembered focus plus the filter offset
  and temperature terms the record can supply, and every run — the
  ones that fail included — is recorded with its curve points, its
  prediction and its outcome. A sweep that fails after every attempt
  puts the focuser back where the call found it. Because focus_train
  is declared a focus tool in the registration, rp brackets each call
  with the focus event triple it emits around its own sweeps.

  The simulator's frames carry no detectable stars, so every sweep in
  these scenarios ends in not_enough_stars: the scenarios assert the
  deterministic artifacts — the derived sweep, the prediction, the
  grid walk, the retry, the put-back, the record, the events and the
  guider handshake — and leave the fitted outcome to the unit tests
  over recorded curves.

  These scenarios start all three processes (OmniSim, focus-model, rp
  with the provider registered) and call the tools through rp's proxy,
  exactly as a session-runner document would.

  Scenario: The focus tools appear in rp's catalog ungated
    Given a running Alpaca simulator
    And rp is running with a focus train on the simulator and focus-model registered as a tool provider
    And an MCP client connected to rp
    When the MCP client lists available tools
    Then the tool list should include "focus_train"
    And the tool list should include "get_sweep_plan"
    And the tool list should include "get_focus_model"
    And the tool list should include "get_focus_runs"
    And the tool list should include "set_focus_offsets"
    And the tool list should include "reset_focus_model"
    And the safety status should not list "focus_train" as gated
    And the safety status should not list "get_sweep_plan" as gated

  # The reference rig: 500 mm at f/5, a 5.6 µm camera and a 2.5 µm/step
  # focuser at 550 nm. CFZ = 4.88 × 0.55 × 25 = 67.1 µm = 26.84 steps;
  # the blur geometry predicts 3.125 px per 100 steps; 2.5" seeing at
  # 2.31"/px is a 0.54 px focused HFR, so the sweep reaches 4× that at
  # a half width of 68, and nine samples across it step by 17.
  Scenario: get_sweep_plan derives the sweep from the train's optics
    Given a running Alpaca simulator
    And the focuser is configured with microns_per_step 2.5
    And rp is running with a focus train on the simulator and focus-model registered as a tool provider
    And an MCP client connected to rp
    When the MCP client calls "get_sweep_plan" with {"train_id": "main"}
    Then the tool call should succeed
    And the tool result at "/source" should be the JSON "derived"
    And the tool result at "/half_width" should be the JSON 68
    And the tool result at "/step_size" should be the JSON 17
    And the tool result at "/points" should be the JSON 9
    And the tool result at "/wavelength_nm" should be the JSON 550.0
    And the tool result at "/predicted_slope" should be the JSON 3.125
    And the tool result at "/measured_slope" should be the JSON null
    And the tool result at "/configured" should be the JSON null

  # No microns_per_step in the config: the focuser's ASCOM StepSize,
  # 20 µm on the simulator, is the fact instead. The coarser step
  # shrinks the half width to 9 and the CFZ floor of 3.355/2 lifts the
  # step off the even spread of 3.
  Scenario: The sweep falls back to the focuser's reported step size
    Given a running Alpaca simulator
    And rp is running with a focus train on the simulator and focus-model registered as a tool provider
    And an MCP client connected to rp
    When the MCP client calls "get_sweep_plan" with {"train_id": "main"}
    Then the tool call should succeed
    And the tool result at "/half_width" should be the JSON 9
    And the tool result at "/step_size" should be the JSON 3
    And the tool result at "/optics/microns_per_step" should be the JSON 20.0

  Scenario: A narrowband filter is sized at its own wavelength
    Given a running Alpaca simulator
    And the focuser is configured with microns_per_step 2.5
    And the filter wheel's "Ha" filter is configured at 656 nm
    And rp is running with a focus train on the simulator and focus-model registered as a tool provider
    And an MCP client connected to rp
    When the MCP client calls "get_sweep_plan" with {"train_id": "main", "filter": "Ha"}
    Then the tool call should succeed
    And the tool result at "/wavelength_nm" should be the JSON 656.0

  Scenario: A configured sweep overrides the derivation and reports its source
    Given a running Alpaca simulator
    And the focus provider is configured for train "main" with step_size "40"
    And the focus provider is configured for train "main" with half_width "200"
    And rp is running with a focus train on the simulator and focus-model registered as a tool provider
    And an MCP client connected to rp
    When the MCP client calls "get_sweep_plan" with {"train_id": "main"}
    Then the tool call should succeed
    And the tool result at "/source" should be the JSON "configured"
    And the tool result at "/step_size" should be the JSON 40
    And the tool result at "/half_width" should be the JSON 200
    And the tool result at "/configured/step_size" should be the JSON 40

  Scenario: One override leaves the other derived
    Given a running Alpaca simulator
    And the focuser is configured with microns_per_step 2.5
    And the focus provider is configured for train "main" with half_width "200"
    And rp is running with a focus train on the simulator and focus-model registered as a tool provider
    And an MCP client connected to rp
    When the MCP client calls "get_sweep_plan" with {"train_id": "main"}
    Then the tool call should succeed
    And the tool result at "/source" should be the JSON "mixed"
    And the tool result at "/half_width" should be the JSON 200
    And the tool result at "/step_size" should be the JSON 50

  Scenario: A train without an aperture and without a configured sweep names the missing fact
    Given a running Alpaca simulator
    And rp is running with a focus train without an aperture and focus-model registered as a tool provider
    And an MCP client connected to rp
    When the MCP client calls "get_sweep_plan" with {"train_id": "main"}
    Then the tool call should return an error
    And the error message should contain "aperture_mm is unknown"
    And the error message should contain "trains.main.step_size"

  Scenario: A filter the wheel does not have is refused naming the wheel's filters
    Given a running Alpaca simulator
    And rp is running with a focus train on the simulator and focus-model registered as a tool provider
    And an MCP client connected to rp
    When the MCP client calls "focus_train" with {"train_id": "main", "filter": "OIII"}
    Then the tool call should return an error
    And the error message should contain "filter 'OIII' is not on train 'main'"
    And the error message should contain "Luminance, Ha"

  Scenario: An unknown train is rp's own error, relayed
    Given a running Alpaca simulator
    And rp is running with a focus train on the simulator and focus-model registered as a tool provider
    And an MCP client connected to rp
    When the MCP client calls "focus_train" with {"train_id": "nope"}
    Then the tool call should return an error
    And the error message should contain "train not found: nope"

  # A starless sweep walks the grid, fails to fit, and — with one
  # attempt configured — puts the focuser back where the call found it
  # before erroring. The error carries the attempt count, the
  # prediction and the samples, so the run is diagnosable without
  # re-measuring a frame.
  Scenario: A starless sweep restores the starting position and reports its curve
    Given rp's data_directory is pinned to a fresh tempdir
    And a running Alpaca simulator
    And the focus provider is configured for train "main" with max_attempts "1"
    And rp is running with a focus train on the simulator and focus-model registered as a tool provider
    And an MCP client connected to rp
    And the focuser is at position 25000
    When the MCP client calls "focus_train" with {"train_id": "main"}
    Then the tool call should return an error
    And the error message should contain "not enough stars"
    And the error message should contain "attempts: 1"
    And the error message should contain "curve_points"
    And the focuser should be back at position 25000 within 60 seconds

  Scenario: A failed sweep is repeated before it errors
    Given rp's data_directory is pinned to a fresh tempdir
    And a running Alpaca simulator
    And the focus provider is configured for train "main" with max_attempts "2"
    And rp is running with a focus train on the simulator and focus-model registered as a tool provider
    And an MCP client connected to rp
    When the MCP client calls "focus_train" with {"train_id": "main"}
    Then the tool call should return an error
    And the error message should contain "attempts: 2"

  # rp brackets a focus tool named in the registration's focus_tools
  # map with the same event triple it emits around its own sweeps, so a
  # night watching the stream sees one focus vocabulary whoever ran the
  # sweep.
  Scenario: rp brackets the call with the focus events
    Given rp's data_directory is pinned to a fresh tempdir
    And a running Alpaca simulator
    And a test webhook receiver subscribed to "focus_started"
    And a test webhook receiver subscribed to "focus_failed"
    And the focus provider is configured for train "main" with max_attempts "1"
    And rp is running with a focus train on the simulator and focus-model registered as a tool provider
    And an MCP client connected to rp
    When the MCP client calls "focus_train" with {"train_id": "main"}
    Then the tool call should return an error
    And the test webhook receiver should have received at least 1 "focus_started" event
    And the test webhook receiver should have received at least 1 "focus_failed" event

  # Every run is recorded, the failed ones most of all: the morning
  # after, the operator reads the outcome, the samples the sweep
  # measured and the prediction it made.
  Scenario: A failed run is recorded with its curve points and its outcome
    Given rp's data_directory is pinned to a fresh tempdir
    And a running Alpaca simulator
    And the focus provider is configured for train "main" with max_attempts "1"
    And rp is running with a focus train on the simulator and focus-model registered as a tool provider
    And an MCP client connected to rp
    When the MCP client calls "focus_train" with {"train_id": "main"}
    Then the tool call should return an error
    When the MCP client calls "get_focus_runs" with {"train_id": "main"}
    Then the tool call should succeed
    And the tool result at "/total" should be the JSON 1
    And the tool result at "/runs/0/outcome" should be the JSON "not_enough_stars"
    And the tool result at "/runs/0/position" should be the JSON null
    And the tool result "/runs/0/curve_points" should have 7 entries
    And the tool result at "/runs/0/prediction/start" should be the JSON null
    When the MCP client calls "get_focus_model" with {"train_id": "main"}
    Then the tool call should succeed
    And the tool result at "/model" should be the JSON "fresh"
    And the tool result at "/runs_recorded" should be the JSON 1
    And the tool result "/last_good" should have 0 entries

  # A seeded last good focus is the anchor: the provider moves there
  # before the sweep and says so in the run it records.
  Scenario: A remembered focus predicts the start of the next sweep
    Given rp's data_directory is pinned to a fresh tempdir
    And a running Alpaca simulator
    And a stored focus model for train "main" with a last good position of 25200
    And the focus provider is configured for train "main" with max_attempts "1"
    And rp is running with a focus train on the simulator and focus-model registered as a tool provider
    And an MCP client connected to rp
    And the focuser is at position 25000
    When the MCP client calls "focus_train" with {"train_id": "main"}
    Then the tool call should return an error
    When the MCP client calls "get_focus_runs" with {"train_id": "main"}
    Then the tool call should succeed
    And the tool result at "/runs/0/prediction/start" should be the JSON 25200
    And the tool result at "/runs/0/prediction/moved" should be the JSON true
    And the tool result at "/runs/0/prediction/missing/0" should be the JSON "temperature_coefficient"

  # The identity fields catch a camera swap: the record predicts
  # nothing, says which field moved on, and the next run replaces it.
  Scenario: A record trained on another camera is stale and the next run resets it
    Given rp's data_directory is pinned to a fresh tempdir
    And a running Alpaca simulator
    And a stored focus model for train "main" trained on camera "retired-cam"
    And the focus provider is configured for train "main" with max_attempts "1"
    And rp is running with a focus train on the simulator and focus-model registered as a tool provider
    And an MCP client connected to rp
    When the MCP client calls "get_focus_model" with {"train_id": "main"}
    Then the tool call should succeed
    And the tool result at "/model" should be the JSON "stale: camera_id changed from retired-cam to main-cam"
    When the MCP client calls "focus_train" with {"train_id": "main"}
    Then the tool call should return an error
    When the MCP client calls "get_focus_runs" with {"train_id": "main"}
    Then the tool call should succeed
    And the tool result at "/total" should be the JSON 1
    And the tool result at "/runs/0/prediction/start" should be the JSON null
    When the MCP client calls "get_focus_model" with {"train_id": "main"}
    Then the tool call should succeed
    And the tool result at "/model" should be the JSON "fresh"
    And the tool result at "/camera_id" should be the JSON "main-cam"

  Scenario: Offsets are entered by hand and validated against the wheel
    Given a running Alpaca simulator
    And rp is running with a focus train on the simulator and focus-model registered as a tool provider
    And an MCP client connected to rp
    When the MCP client calls "set_focus_offsets" with {"train_id": "main", "reference": "Luminance", "offsets": {"Ha": 46}}
    Then the tool call should succeed
    And the tool result at "/reference_filter" should be the JSON "Luminance"
    And the tool result at "/offsets/Ha" should be the JSON 46
    And the tool result at "/offsets/Luminance" should be the JSON 0
    When the MCP client calls "set_focus_offsets" with {"train_id": "main", "reference": "Luminance", "offsets": {"OIII": 12}}
    Then the tool call should return an error
    And the error message should contain "filter 'OIII' is not on train 'main'"
    When the MCP client calls "get_focus_model" with {"train_id": "main"}
    Then the tool call should succeed
    And the tool result at "/offsets/Ha" should be the JSON 46

  # A re-homed focuser invalidates the measurements the identity
  # fields cannot see; the offsets are differences between filters and
  # survive it.
  Scenario: A reset drops the measurements and keeps the offsets
    Given rp's data_directory is pinned to a fresh tempdir
    And a running Alpaca simulator
    And a stored focus model for train "main" with a last good position of 25200
    And the focus provider is configured for train "main" with max_attempts "1"
    And rp is running with a focus train on the simulator and focus-model registered as a tool provider
    And an MCP client connected to rp
    When the MCP client calls "set_focus_offsets" with {"train_id": "main", "reference": "Luminance", "offsets": {"Ha": 46}}
    Then the tool call should succeed
    When the MCP client calls "focus_train" with {"train_id": "main"}
    Then the tool call should return an error
    When the MCP client calls "reset_focus_model" with {"train_id": "main"}
    Then the tool call should succeed
    And the tool result "/dropped" should have 3 entries
    And the tool result at "/model/runs_recorded" should be the JSON 0
    And the tool result "/model/last_good" should have 0 entries
    And the tool result at "/model/offsets/Ha" should be the JSON 46

  Scenario: get_focus_runs answers the newest runs first and refuses a zero limit
    Given rp's data_directory is pinned to a fresh tempdir
    And a running Alpaca simulator
    And the focus provider is configured for train "main" with max_attempts "1"
    And rp is running with a focus train on the simulator and focus-model registered as a tool provider
    And an MCP client connected to rp
    When the MCP client calls "focus_train" with {"train_id": "main"}
    Then the tool call should return an error
    When the MCP client calls "focus_train" with {"train_id": "main", "filter": "Ha"}
    Then the tool call should return an error
    When the MCP client calls "get_focus_runs" with {"train_id": "main", "limit": 1}
    Then the tool call should succeed
    And the tool result at "/total" should be the JSON 2
    And the tool result "/runs" should have 1 entries
    And the tool result at "/runs/0/filter" should be the JSON "Ha"
    When the MCP client calls "get_focus_runs" with {"train_id": "main", "filter": "Luminance"}
    Then the tool call should succeed
    And the tool result at "/total" should be the JSON 1
    When the MCP client calls "get_focus_runs" with {"train_id": "main", "limit": 0}
    Then the tool call should return an error
    And the error message should contain "limit must be at least 1"

  # The wheel moves before the sweep when the call names another
  # filter, and the run is recorded under that filter's name.
  Scenario: A filter argument moves the wheel and names the run
    Given rp's data_directory is pinned to a fresh tempdir
    And a running Alpaca simulator
    And the focus provider is configured for train "main" with max_attempts "1"
    And rp is running with a focus train on the simulator and focus-model registered as a tool provider
    And an MCP client connected to rp
    When the MCP client calls "focus_train" with {"train_id": "main", "filter": "Ha"}
    Then the tool call should return an error
    And the simulator's filter wheel should be at position 1
    When the MCP client calls "get_focus_runs" with {"train_id": "main"}
    Then the tool call should succeed
    And the tool result at "/runs/0/filter" should be the JSON "Ha"

  # The caller going away is the cancellation: rp cancels the proxied
  # call and forwards notifications/cancelled to the provider, whose
  # put-back then runs on a token the cancellation cannot reach. The
  # recorded outcome is what separates a cancelled sweep from one that
  # merely failed — both put the focuser back — so the run is asserted
  # first and the position after it, by which time the put-back has
  # already run.
  Scenario: A cancelled sweep is recorded as cancelled and puts the focuser back
    Given rp's data_directory is pinned to a fresh tempdir
    And a running Alpaca simulator
    And the focus provider is configured for train "main" with duration "3s"
    And the focus provider is configured for train "main" with max_attempts "1"
    And rp is running with a focus train on the simulator and focus-model registered as a tool provider
    And an MCP client connected to rp
    And the focuser is at position 25000
    When a second MCP client starts "focus_train" with {"train_id": "main"} in the background
    And the focuser has moved away from position 25000
    And the second MCP client disconnects
    Then a focus run should be recorded for train "main" within 120 seconds
    And the tool result at "/runs/0/outcome" should be the JSON "cancelled"
    And the focuser should be back at position 25000 within 30 seconds

  # A capture step that moves a focuser the guiding train shares is
  # run with guide corrections paused, and they are resumed even
  # though the sweep failed.
  Scenario: A guide-coupled sweep pauses and resumes guiding
    Given rp's data_directory is pinned to a fresh tempdir
    And a running Alpaca simulator
    And a stub guider returning canned guiding stats
    And the focus provider is configured for train "main" with max_attempts "1"
    And rp is running with a focus train sharing its focuser with an offline guiding train and focus-model registered as a tool provider
    And an MCP client connected to rp
    When the MCP client calls "focus_train" with {"train_id": "main"}
    Then the tool call should return an error
    And the stub guider should have received a pause request with full false
    And the stub guider should have received a resume request

  Scenario: The handshake is skipped when the guider reports no active loop
    Given rp's data_directory is pinned to a fresh tempdir
    And a running Alpaca simulator
    And a stub guider reporting guiding inactive
    And the focus provider is configured for train "main" with max_attempts "1"
    And rp is running with a focus train sharing its focuser with an offline guiding train and focus-model registered as a tool provider
    And an MCP client connected to rp
    When the MCP client calls "focus_train" with {"train_id": "main"}
    Then the tool call should return an error
    And the stub guider should not have received a pause request

  # A shared walk runs rp's refocus plan step by step. The first
  # capture step fails against the starless simulator, so the sequence
  # stops there — and the focuser that step moved is put back.
  Scenario: A shared walk stops at the failing step
    Given rp's data_directory is pinned to a fresh tempdir
    And a running Alpaca simulator
    And a stub guider returning canned guiding stats
    And the focus provider is configured for train "main" with max_attempts "1"
    And rp is running with a focus train sharing its focuser with an offline guiding train and focus-model registered as a tool provider
    And an MCP client connected to rp
    And the focuser is at position 25000
    When the MCP client calls "focus_train" with {"train_id": "main", "shared": true}
    Then the tool call should return an error
    And the error message should contain "not enough stars"
    And the focuser should be back at position 25000 within 60 seconds
