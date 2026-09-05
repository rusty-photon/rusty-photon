@serial
Feature: Flat-field tools served through rp
  calibrator-flats is a tool provider: rp dials it at startup, merges
  train_flats, take_flats and get_flat_training into its catalog, and
  proxies calls to it. Every tool takes a train_id and resolves the
  train's camera, filter wheel and cover calibrator through rp's own
  get_train_info; the provider then drives the rig by calling rp's
  primitive tools as an MCP client. train_flats learns the exposure and
  panel brightness that produce a 50 % flat per filter and writes a
  record per converged filter; an unconverged filter is reported in
  the result and writes nothing. take_flats checks every requested
  filter against the store before touching anything and captures at
  the trained timing, measuring each frame after the fact and warning
  when its median leaves the tolerance band. Both put the cover back
  the way they found it and turn the panel off on every exit — success,
  error and cancellation alike — and a reopen that rp refuses for
  safety is a warning, not a failure.

  These scenarios start all three processes (OmniSim, calibrator-flats,
  rp with the provider registered) and call the tools through rp's
  proxy, exactly as a session-runner document would.

  Scenario: The flats tools appear in rp's catalog ungated
    Given a running Alpaca simulator
    And rp is running with an imaging train on the simulator and calibrator-flats registered as a tool provider
    And an MCP client connected to rp
    When the MCP client lists available tools
    Then the tool list should include "train_flats"
    And the tool list should include "take_flats"
    And the tool list should include "get_flat_training"
    And the safety status should not list "train_flats" as gated
    And the safety status should not list "take_flats" as gated
    And the safety status should not list "get_flat_training" as gated

  # Tolerance 1.0 accepts any measured median: the first exposure
  # converges, and the contract under test is the record — its shape,
  # the camera facts it pins, and that get_flat_training reads it back
  # as trained against the live camera.
  Scenario: train_flats records a converged filter
    Given a running Alpaca simulator
    And the flats provider is configured with tolerance "1.0"
    And rp is running with an imaging train on the simulator and calibrator-flats registered as a tool provider
    And an MCP client connected to rp
    When the MCP client calls "train_flats" with {"train_id": "main", "filters": ["Luminance"]}
    Then the tool call should succeed
    And the tool result "/trained" should have 1 entries
    And the tool result "/trained/0/filter" should be "Luminance"
    And the tool result "/trained/0/camera_id" should be "main-cam"
    And the tool result "/trained/0/train_id" should be "main"
    And the tool result "/unconverged" should have 0 entries
    And the calibrator panel should be off
    When the MCP client calls "get_flat_training" with {"train_id": "main"}
    Then the tool call should succeed
    And the tool result "/records" should have 1 entries
    And the tool result "/records/0/status" should be "trained"
    And the tool result "/records/0/record/filter" should be "Luminance"

  # Tolerance 0.0 converges only on an exact hit and one iteration per
  # brightness level gives the ladder nothing to work with: the filter
  # ends unconverged, is reported as such — a normal result, not a tool
  # error — and no record exists for it afterwards.
  Scenario: An unconverged filter is reported and writes no record
    Given a running Alpaca simulator
    And the flats provider is configured with tolerance "0.0"
    And the flats provider is configured with max_iterations "1"
    And rp is running with an imaging train on the simulator and calibrator-flats registered as a tool provider
    And an MCP client connected to rp
    When the MCP client calls "train_flats" with {"train_id": "main", "filters": ["Red"]}
    Then the tool call should succeed
    And the tool result "/trained" should have 0 entries
    And the tool result "/unconverged" should have 1 entries
    And the tool result "/unconverged/0/filter" should be "Red"
    When the MCP client calls "get_flat_training" with {"train_id": "main", "filter": "Red"}
    Then the tool call should succeed
    And the tool result "/records" should have 0 entries

  Scenario: take_flats refuses an untrained filter before actuating
    Given a running Alpaca simulator
    And the cover starts closed
    And rp is running with an imaging train on the simulator and calibrator-flats registered as a tool provider
    And an MCP client connected to rp
    When the MCP client calls "take_flats" with {"train_id": "main", "count": 2, "filters": ["Luminance"]}
    Then the tool call should return an error
    And the error message should contain "Luminance untrained"
    And the error message should contain "run train_flats first"
    And the calibrator panel should be off
    And the cover should be closed

  Scenario: take_flats refuses a stale record naming the changed field
    Given a running Alpaca simulator
    And a stored flat training record for train "main" filter "Luminance" trained on camera "retired-cam"
    And rp is running with an imaging train on the simulator and calibrator-flats registered as a tool provider
    And an MCP client connected to rp
    When the MCP client calls "take_flats" with {"train_id": "main", "count": 1, "filters": ["Luminance"]}
    Then the tool call should return an error
    And the error message should contain "Luminance stale"
    And the error message should contain "camera_id changed from retired-cam to main-cam"
    And the calibrator panel should be off
    When the MCP client calls "get_flat_training" with {"train_id": "main"}
    Then the tool call should succeed
    And the tool result "/records/0/status" should be "stale"

  # A warning band of 0.0 marks every frame whose median is not exactly
  # the 50 % target, so the verification path is exercised on a
  # simulator whose flats never hit it to the ADU. The frames are still
  # captured and counted: verification is advisory.
  Scenario: take_flats captures the requested frames and warns on an out-of-range median
    Given a running Alpaca simulator
    And the flats provider is configured with tolerance "1.0"
    And the flats provider is configured with flat_warn_tolerance "0.0"
    And a test webhook receiver subscribed to "exposure_complete"
    And rp is running with an imaging train on the simulator and calibrator-flats registered as a tool provider
    And an MCP client connected to rp
    When the MCP client calls "train_flats" with {"train_id": "main", "filters": ["Luminance"]}
    Then the tool call should succeed
    When the MCP client calls "take_flats" with {"train_id": "main", "count": 2, "filters": ["Luminance"]}
    Then the tool call should succeed
    And the tool result "/total_frames" should be 2
    And the tool result "/filters/0/filter" should be "Luminance"
    And the tool result "/filters/0/frames" should be 2
    And the tool result "/filters/0/out_of_range" should have 2 entries
    And the tool result "/warnings" should have 2 entries
    And the test webhook receiver should have received at least 2 "exposure_complete" events
    And the calibrator panel should be off

  Scenario: A filterless train trains and takes flats under the train id alone
    Given a running Alpaca simulator
    And the flats provider is configured with tolerance "1.0"
    And rp is running with a filterless imaging train on the simulator and calibrator-flats registered as a tool provider
    And an MCP client connected to rp
    When the MCP client calls "train_flats" with {"train_id": "main"}
    Then the tool call should succeed
    And the tool result "/trained" should have 1 entries
    And the tool result "/trained/0/filter" should be null
    When the MCP client calls "take_flats" with {"train_id": "main", "count": 1}
    Then the tool call should succeed
    And the tool result "/total_frames" should be 1
    And the tool result "/filters/0/filter" should be null

  Scenario: Passing filters for a filterless train is an error naming the train
    Given a running Alpaca simulator
    And rp is running with a filterless imaging train on the simulator and calibrator-flats registered as a tool provider
    And an MCP client connected to rp
    When the MCP client calls "train_flats" with {"train_id": "main", "filters": ["Luminance"]}
    Then the tool call should return an error
    And the error message should contain "train 'main' has no filter wheel"

  Scenario: A filter the wheel does not have is an error listing the wheel's filters
    Given a running Alpaca simulator
    And rp is running with an imaging train on the simulator and calibrator-flats registered as a tool provider
    And an MCP client connected to rp
    When the MCP client calls "train_flats" with {"train_id": "main", "filters": ["Ha"]}
    Then the tool call should return an error
    And the error message should contain "filter 'Ha' is not on train 'main'"
    And the error message should contain "Luminance, Red, Green, Blue"

  # The caller going away is the cancellation: rp cancels the proxied
  # call and forwards notifications/cancelled to the provider, whose
  # cleanup then runs on a token the cancellation cannot reach. Fifty
  # simulator flats take well over a minute, so a panel that is off
  # within 20 s of the disconnect is a run that stopped early.
  Scenario: A cancelled take_flats turns the panel off and restores an open cover
    Given a running Alpaca simulator
    And the cover starts open
    And the flats provider is configured with tolerance "1.0"
    And rp is running with an imaging train on the simulator and calibrator-flats registered as a tool provider
    And an MCP client connected to rp
    When the MCP client calls "train_flats" with {"train_id": "main", "filters": ["Luminance"]}
    Then the tool call should succeed
    And the cover should be open
    When a second MCP client starts "take_flats" with {"train_id": "main", "count": 50, "filters": ["Luminance"]} in the background
    And the calibrator panel is lit
    And the second MCP client disconnects
    Then the calibrator panel should be off within 20 seconds
    And the cover should be open within 30 seconds

  # Conditions turn unsafe before the call: the ungated flats tools
  # still run behind a closed cover, and only the reopen — open_cover,
  # which rp gates — is refused. The flats succeed, the refusal is a
  # warning, and the cover correctly stays closed.
  Scenario: A refused open_cover after an unsafe transition is a warning, not a failure
    Given a running Alpaca simulator
    And a safety monitor on the simulator
    And the cover starts open
    And the flats provider is configured with tolerance "1.0"
    And rp is running with an imaging train on the simulator and calibrator-flats registered as a tool provider
    And an MCP client connected to rp
    When the safety monitor reports unsafe
    And the safety status reports overall "unsafe" within 5 seconds
    And the MCP client calls "train_flats" with {"train_id": "main", "filters": ["Luminance"]}
    Then the tool call should succeed
    And the tool result "/trained" should have 1 entries
    And the tool result "/cover_restored" should be false
    And the tool result "/warnings" should contain an entry mentioning "open_cover"
    And the calibrator panel should be off
    And the cover should be closed
