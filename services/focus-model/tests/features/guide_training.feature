@serial
Feature: Training runs on a guiding train
  A training run is focus_train without shared, or
  determine_filter_offsets, addressed to the guiding train. It sweeps
  the guiding train's own focuser and captures through the guide
  camera under rp's train lease, which holds rp's guide camera guard:
  rp grants the lease only with the guider idle, reads the guider's
  state again before every capture through the guide camera, and
  starts no guiding while the lease lasts. The provider sizes the
  lease from the sweep and takes it before the guiding handshake and
  the first move, so a guider in any state but idle refuses the run at
  the acquire before anything pauses or moves, and the provider relays
  rp's refusal after it has recorded the run. A run whose estimate is
  longer than a lease may last is refused before the lease is asked
  for. Under the lease the guider is idle, so the handshake pauses
  nothing and the guider is sent nothing but reads. The lease is
  released after the put-back on every path, before the call answers,
  so a start of guiding sent after the answer goes through.
  focus_train takes one lease for the whole run, its retries included;
  determine_filter_offsets takes one for each sweep, and gives it back
  before the next sweep asks for its own: rp lets one lease hold a
  camera at a time, so a lease not given back would refuse the next
  sweep's. A guiding train whose
  terminal focuser an imaging train also has is focused by that
  train's sweep, so a training run on it is refused before anything
  moves.

  The guiding train's camera here is the simulator's camera, a roster
  camera of rp's like any other, so the sweep takes real frames. They
  are starless (the provider's train block pins a detection threshold
  no simulator frame clears), so every sweep ends in not_enough_stars
  and puts the focuser back. The guider is the stub guider, which
  answers the state reads of the guider's contract and records every
  request. Its camera is the simulator's camera, so the check rp makes
  of the guider's devices before a start passes.

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

  Scenario: A training run sends the guider nothing but reads through both its attempts, and a start of guiding after it goes through
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
    And the stub guider should have received state reads and no other request
    And a focus run should be recorded for train "guide" within 10 seconds
    When the MCP client calls "start_guiding" with {}
    Then the tool result at "/state" should be the JSON "guiding"

  Scenario: A training run is refused at the lease while the guider is guiding, with rp's refusal, before anything pauses or moves
    Given rp's data_directory is pinned to a fresh tempdir
    And a running Alpaca simulator
    And a stub guider in state "guiding"
    And the stub guider's camera is the simulator's camera
    And rp is running with a guiding train on its own focuser behind the simulator's camera and focus-model registered as a tool provider
    And an MCP client connected to rp
    And the focuser is at position 25000
    When the MCP client calls "focus_train" with {"train_id": "guide"}
    Then the tool call should return an error
    And the error message should contain "acquire_train_lease: the guider is guiding; rp uses the guide camera 'guide-cam' only while the guider is idle"
    And the focuser should still be at position 25000
    And the stub guider should have received state reads and no other request
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
    And the stub guider should have received no request

  Scenario: The offsets procedure gives each sweep's train lease back before the next sweep, sends the guider nothing but reads, and leaves the wheel back and the guide camera free
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
    And the stub guider should have received state reads and no other request
    And the simulator's filter wheel should be at position 0
    When the MCP client calls "get_focus_runs" with {"train_id": "guide"}
    Then the tool result at "/total" should be the JSON 3
    When the MCP client calls "start_guiding" with {}
    Then the tool result at "/state" should be the JSON "guiding"
