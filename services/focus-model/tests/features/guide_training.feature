@serial
Feature: Training runs on a guiding train
  A training run is focus_train without shared, or
  determine_filter_offsets, addressed to the guiding train. It sweeps
  the guiding train's own focuser and captures through the guide
  camera under rp's train lease, which holds the guider's camera lease
  and with it the guider idle. The provider sizes the lease from the
  sweep and takes it before the guiding handshake and the first move.
  rp checks, as it grants the lease, that the guider is idle and that
  the camera the guider exposes on is the train's: a guider in any
  other state, or one that names another camera, refuses the run at
  the acquire before anything pauses or moves, and the provider relays
  rp's refusal after it has recorded the run. A run whose estimate is
  longer than a lease may last is refused before the lease is asked
  for. The lease is released after the put-back on every path, and
  rp gives the camera lease back by its token with it. focus_train
  takes one lease for the whole run, its retries included;
  determine_filter_offsets takes one for each sweep. A guiding train
  whose terminal focuser an imaging train also has is focused by that
  train's sweep, so a training run on it is refused before anything
  moves.

  The guiding train's camera here is the simulator's camera, a roster
  camera of rp's like any other, so the sweep takes real frames. They
  are starless (the provider's train block pins a detection threshold
  no simulator frame clears), so every sweep ends in not_enough_stars
  and puts the focuser back. The guider is the stub guider, which
  grants its camera lease in the state idle only, to one token at a
  time, and records every request.

  Scenario Outline: A training run on a guiding train whose focuser the imaging train has is refused before anything moves
    Given rp's data_directory is pinned to a fresh tempdir
    And a running Alpaca simulator
    And a stub guider in state "idle"
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

  Scenario: A training run holds one camera lease across both its attempts and gives it back after the put-back
    Given rp's data_directory is pinned to a fresh tempdir
    And a running Alpaca simulator
    And a stub guider in state "idle"
    And the stub guider's camera is the simulator's camera
    And the focus provider is configured for train "guide" with max_attempts "2"
    And rp is running with a guiding train on its own focuser behind the simulator's camera and focus-model registered as a tool provider
    And an MCP client connected to rp
    And the focuser is at position 25000
    When the MCP client calls "focus_train" with {"train_id": "guide"}
    Then the tool call should return an error
    And the error message should contain "not enough stars"
    And the error message should contain "attempts: 2"
    And the focuser should be back at position 25000 within 60 seconds
    And the stub guider should have been asked for 1 camera lease
    And every camera lease the stub guider was asked for should be released by its token
    And the stub guider should hold no camera lease
    And a focus run should be recorded for train "guide" within 10 seconds

  Scenario: A training run is refused at the lease while the guider is guiding, before anything pauses or moves
    Given rp's data_directory is pinned to a fresh tempdir
    And a running Alpaca simulator
    And a stub guider in state "guiding"
    And the stub guider's camera is the simulator's camera
    And rp is running with a guiding train on its own focuser behind the simulator's camera and focus-model registered as a tool provider
    And an MCP client connected to rp
    And the focuser is at position 25000
    When the MCP client calls "focus_train" with {"train_id": "guide"}
    Then the tool call should return an error
    And the error message should contain "the guider refused the camera lease"
    And the error message should contain "guiding"
    And the focuser should still be at position 25000
    And the stub guider should hold no camera lease
    And the stub guider should not have received a pause request
    And a focus run should be recorded for train "guide" within 10 seconds

  Scenario: A training run is refused at the lease when the guider exposes on another camera than the train's, naming both ids
    Given rp's data_directory is pinned to a fresh tempdir
    And a running Alpaca simulator
    And a stub guider in state "idle"
    And the stub guider reports the camera unique_id "another-camera"
    And rp is running with a guiding train on its own focuser behind the simulator's camera and focus-model registered as a tool provider
    And an MCP client connected to rp
    And the focuser is at position 25000
    When the MCP client calls "focus_train" with {"train_id": "guide"}
    Then the tool call should return an error
    And the error message should contain "another-camera"
    And the error message should name the simulator camera's unique id
    And the focuser should still be at position 25000
    And the stub guider should have been asked for 0 camera leases
    And a focus run should be recorded for train "guide" within 10 seconds

  Scenario: A training run too long for one lease is refused naming the numbers before the lease is asked for
    Given rp's data_directory is pinned to a fresh tempdir
    And a running Alpaca simulator
    And a stub guider in state "idle"
    And the stub guider's camera is the simulator's camera
    And the focus provider is configured for train "guide" with duration "60s"
    And the focus provider is configured for train "guide" with frames_per_step "5"
    And the focus provider is configured for train "guide" with max_attempts "5"
    And rp is running with a guiding train on its own focuser behind the simulator's camera and focus-model registered as a tool provider
    And an MCP client connected to rp
    And the focuser is at position 25000
    When the MCP client calls "focus_train" with {"train_id": "guide"}
    Then the tool call should return an error
    And the error message should contain "longer than the 10m a lease may last"
    And the focuser should still be at position 25000
    And the stub guider should have been asked for 0 camera leases

  Scenario: The offsets procedure takes one camera lease for each sweep, gives each back and puts the wheel back
    Given rp's data_directory is pinned to a fresh tempdir
    And a running Alpaca simulator
    And a stub guider in state "idle"
    And the stub guider's camera is the simulator's camera
    And the focus provider is configured for train "guide" with max_attempts "1"
    And rp is running with a guiding train on its own focuser and filter wheel behind the simulator's camera and focus-model registered as a tool provider
    And an MCP client connected to rp
    When the MCP client calls "determine_filter_offsets" with {"train_id": "guide", "rounds": 1}
    Then the tool call should return an error
    And the error message should contain "no filter was measured against 'Luminance'"
    And the stub guider should have been asked for 3 camera leases
    And every camera lease the stub guider was asked for should be released by its token
    And the stub guider should hold no camera lease
    And the simulator's filter wheel should be at position 0
