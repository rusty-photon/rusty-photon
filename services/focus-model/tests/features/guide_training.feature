@serial
Feature: Training runs on a guiding train
  A training run is focus_train without shared, or
  determine_filter_offsets, addressed to the guiding train. It sweeps
  the guiding train's own focuser and captures through the PHD2 camera
  facade under rp's train lease. The provider sizes the lease from the
  sweep and takes it before the guiding handshake and the first move.
  Every capture, focuser move and filter change of the run carries the
  lease's id, the put-back included, and the lease is released after
  the put-back on every path. focus_train takes one lease for the whole
  run, its retries included; determine_filter_offsets takes one for
  each sweep. A guiding train whose terminal focuser an imaging train
  also has is focused by that train's sweep, so a training run on it is
  refused before anything moves.

  The camera facade here is a stub that answers the sweep-lease Actions
  itself and passes every other Camera call through to the simulator's
  camera, so the sweep takes real frames. They are starless (the
  provider's train block pins a detection threshold no simulator frame
  clears), so every sweep ends in not_enough_stars and puts the focuser
  back.

  Scenario Outline: A training run on a guiding train whose focuser the imaging train has is refused before anything moves
    Given rp's data_directory is pinned to a fresh tempdir
    And a running Alpaca simulator
    And a stub guider returning canned guiding stats
    And rp is running with a focus train sharing its focuser with an offline guiding train and focus-model registered as a tool provider
    And an MCP client connected to rp
    And the focuser is at position 25000
    When the MCP client calls "<tool>" with {"train_id": "guide"}
    Then the tool call should return an error
    And the error message should contain "guiding train 'guide' has no focuser of its own: its terminal focuser 'main-focuser' is in train 'main', whose sweep focuses it"
    And the focuser should still be at position 25000

    Examples:
      | tool                     |
      | focus_train              |
      | determine_filter_offsets |

  Scenario: A training run sweeps under one train lease and releases it after the put-back
    Given rp's data_directory is pinned to a fresh tempdir
    And a running Alpaca simulator
    And a stub guider returning canned guiding stats
    And a stub camera facade over the simulator's camera
    And the focus provider is configured for train "guide" with max_attempts "2"
    And rp is running with a guiding train on its own focuser behind the stub camera facade and focus-model registered as a tool provider
    And an MCP client connected to rp
    And the focuser is at position 25000
    When the MCP client calls "focus_train" with {"train_id": "guide"}
    Then the tool call should return an error
    And the error message should contain "not enough stars"
    And the error message should contain "attempts: 2"
    And the focuser should be back at position 25000 within 60 seconds
    And the stub camera facade should have been asked for 1 sweep lease
    And every sweep lease the stub camera facade was asked for should be released by its token
    And the stub camera facade should hold no sweep lease
    And a focus run should be recorded for train "guide" within 10 seconds

  Scenario: A training run is refused at the lease while PHD2 is guiding, before anything moves
    Given rp's data_directory is pinned to a fresh tempdir
    And a running Alpaca simulator
    And a stub guider returning canned guiding stats
    And a stub camera facade over the simulator's camera
    And the stub camera facade reports PHD2 as "Guiding"
    And rp is running with a guiding train on its own focuser behind the stub camera facade and focus-model registered as a tool provider
    And an MCP client connected to rp
    And the focuser is at position 25000
    When the MCP client calls "focus_train" with {"train_id": "guide"}
    Then the tool call should return an error
    And the error message should contain "PHD2 is Guiding"
    And the focuser should still be at position 25000
    And the stub camera facade should hold no sweep lease
    And a focus run should be recorded for train "guide" within 10 seconds

  Scenario: A guiding train whose camera is not the camera facade is refused at the lease
    Given rp's data_directory is pinned to a fresh tempdir
    And a running Alpaca simulator
    And a stub guider returning canned guiding stats
    And rp is running with a guiding train on its own focuser behind the simulator's camera and focus-model registered as a tool provider
    And an MCP client connected to rp
    And the focuser is at position 25000
    When the MCP client calls "focus_train" with {"train_id": "guide"}
    Then the tool call should return an error
    And the error message should contain "does not offer the sweep lease"
    And the focuser should still be at position 25000

  Scenario: A training run too long for one lease is refused naming the numbers before the lease is asked for
    Given rp's data_directory is pinned to a fresh tempdir
    And a running Alpaca simulator
    And a stub guider returning canned guiding stats
    And a stub camera facade over the simulator's camera
    And the focus provider is configured for train "guide" with duration "60s"
    And the focus provider is configured for train "guide" with frames_per_step "5"
    And the focus provider is configured for train "guide" with max_attempts "5"
    And rp is running with a guiding train on its own focuser behind the stub camera facade and focus-model registered as a tool provider
    And an MCP client connected to rp
    And the focuser is at position 25000
    When the MCP client calls "focus_train" with {"train_id": "guide"}
    Then the tool call should return an error
    And the error message should contain "longer than the 10m a lease may last"
    And the focuser should still be at position 25000
    And the stub camera facade should have been asked for 0 sweep leases

  Scenario: The offsets procedure takes one train lease for each sweep
    Given rp's data_directory is pinned to a fresh tempdir
    And a running Alpaca simulator
    And a stub guider returning canned guiding stats
    And a stub camera facade over the simulator's camera
    And the focus provider is configured for train "guide" with max_attempts "1"
    And rp is running with a guiding train on its own focuser and filter wheel behind the stub camera facade and focus-model registered as a tool provider
    And an MCP client connected to rp
    When the MCP client calls "determine_filter_offsets" with {"train_id": "guide", "rounds": 1}
    Then the tool call should return an error
    And the error message should contain "no filter was measured against 'Luminance'"
    And the stub camera facade should have been asked for 2 sweep leases
    And every sweep lease the stub camera facade was asked for should be released by its token
    And the stub camera facade should hold no sweep lease
    And the simulator's filter wheel should be at position 0
