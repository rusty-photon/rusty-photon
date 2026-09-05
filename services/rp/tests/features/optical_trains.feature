@serial
Feature: Optical trains configuration
  equipment.optical_trains models each camera's light path as an ordered
  list of roster device ids, objective side first, terminating in a
  camera. Membership expresses coupling and position expresses optical
  order; rp derives focus pairing, rotation effects, and the exposure
  document's optics block from the lists. Guiding is mount-scoped: the
  guider service is configured at equipment.mount.guiding, and a train
  with purpose "guiding" requires that block. Per-field invariants
  (purpose enum, focal-length positivity) are rejected at parse with
  HTTP 400; cross-array graph rules are validated on PUT /api/config as
  HTTP 200 status "invalid" with dotted error paths, leaving the file
  untouched. The retired pre-train keys (top-level guider,
  cameras[].focal_length_mm, focusers[]/filter_wheels[].camera_id) are
  unknown fields and fail at parse.

  Tools address trains as an alternative spelling of their device ids:
  capture and center_on_target take camera_id or train_id (exactly one;
  the train's terminal camera), set_filter takes filter_wheel_id or
  train_id (the train's sole filter wheel — none or several is an
  error naming the train), and the five calibrator tools take
  calibrator_id or train_id (the cover calibrator first in the train's
  list — a train without one is an error naming the train). A cover
  calibrator may only be the first device of a train and a train holds
  at most one; the same calibrator may be first in several trains.
  get_train_info describes a train's resolved members without touching
  a device. Device-id addressing stays first-class.

  Scenario: Configured optical trains round-trip through GET /api/config
    Given a temp rp config with the reference optical trains
    And rp is started with that config file
    When I GET /api/config
    Then the config response status should be 200
    And the fetched config field "/equipment/optical_trains/0/id" should be "main"
    And the fetched config field "/equipment/optical_trains/0/purpose" should be "imaging"
    And the fetched config field "/equipment/optical_trains/1/purpose" should be "guiding"
    And the fetched config field "/equipment/mount/guiding/url" should be "http://127.0.0.1:1"

  Scenario: PUT /api/config accepts the reference optical trains unchanged
    Given a temp rp config with the reference optical trains
    And rp is started with that config file
    When I GET /api/config
    And I PUT /api/config with the fetched config unchanged
    Then the config response status should be 200
    And the apply status should be "ok"

  Scenario: A train device missing from the roster is rejected
    Given a temp rp config with the reference optical trains
    And rp is started with that config file
    When I GET /api/config
    And I PUT /api/config with the fetched config after setting "/equipment/optical_trains" to:
      """
      [ { "id": "main", "devices": ["ghost-focuser", "main-cam"] } ]
      """
    Then the config response status should be 200
    And the apply status should be "invalid"
    And the apply errors should name path "equipment.optical_trains.0.devices.0"

  Scenario: A train not terminating in a camera is rejected
    Given a temp rp config with the reference optical trains
    And rp is started with that config file
    When I GET /api/config
    And I PUT /api/config with the fetched config after setting "/equipment/optical_trains" to:
      """
      [ { "id": "main", "devices": ["main-focuser"] } ]
      """
    Then the config response status should be 200
    And the apply status should be "invalid"
    And the apply errors should name path "equipment.optical_trains.0.devices.0"

  Scenario: A train with no devices is rejected
    Given a temp rp config with the reference optical trains
    And rp is started with that config file
    When I GET /api/config
    And I PUT /api/config with the fetched config after setting "/equipment/optical_trains" to:
      """
      [ { "id": "main", "devices": [] } ]
      """
    Then the config response status should be 200
    And the apply status should be "invalid"
    And the apply errors should name path "equipment.optical_trains.0.devices"

  Scenario: A camera before the end of a train is rejected
    Given a temp rp config with the reference optical trains
    And rp is started with that config file
    When I GET /api/config
    And I PUT /api/config with the fetched config after setting "/equipment/optical_trains" to:
      """
      [ { "id": "main", "devices": ["main-cam", "guide-cam"] } ]
      """
    Then the config response status should be 200
    And the apply status should be "invalid"
    And the apply errors should name path "equipment.optical_trains.0.devices.0"

  Scenario: A camera terminating two trains is rejected
    Given a temp rp config with the reference optical trains
    And rp is started with that config file
    When I GET /api/config
    And I PUT /api/config with the fetched config after setting "/equipment/optical_trains" to:
      """
      [ { "id": "main",   "devices": ["main-focuser", "main-cam"] },
        { "id": "second", "devices": ["guide-focuser", "main-cam"] } ]
      """
    Then the config response status should be 200
    And the apply status should be "invalid"
    And the apply errors should name path "equipment.optical_trains.1.devices.1"

  Scenario: A device repeated within one train is rejected
    Given a temp rp config with the reference optical trains
    And rp is started with that config file
    When I GET /api/config
    And I PUT /api/config with the fetched config after setting "/equipment/optical_trains" to:
      """
      [ { "id": "main", "devices": ["main-focuser", "main-focuser", "main-cam"] } ]
      """
    Then the config response status should be 200
    And the apply status should be "invalid"
    And the apply errors should name path "equipment.optical_trains.0.devices.1"

  Scenario: Duplicate train ids are rejected
    Given a temp rp config with the reference optical trains
    And rp is started with that config file
    When I GET /api/config
    And I PUT /api/config with the fetched config after setting "/equipment/optical_trains" to:
      """
      [ { "id": "main", "devices": ["main-focuser", "main-cam"] },
        { "id": "main", "devices": ["guide-focuser", "guide-cam"] } ]
      """
    Then the config response status should be 200
    And the apply status should be "invalid"
    And the apply errors should name path "equipment.optical_trains.1.id"

  Scenario: A second guiding train is rejected
    Given a temp rp config with the reference optical trains
    And rp is started with that config file
    When I GET /api/config
    And I PUT /api/config with the fetched config after setting "/equipment/optical_trains" to:
      """
      [ { "id": "main",  "purpose": "guiding", "devices": ["main-focuser", "main-cam"] },
        { "id": "guide", "purpose": "guiding", "devices": ["guide-focuser", "guide-cam"] } ]
      """
    Then the config response status should be 200
    And the apply status should be "invalid"
    And the apply errors should name path "equipment.optical_trains.1.purpose"

  Scenario: A guiding train without mount guiding configuration is rejected
    Given a temp rp config with the reference optical trains
    And rp is started with that config file
    When I GET /api/config
    And I PUT /api/config with the fetched config after setting "/equipment/mount/guiding" to "null"
    Then the config response status should be 200
    And the apply status should be "invalid"
    And the apply errors should name path "equipment.optical_trains.1.purpose"

  Scenario: Contradictory shared-device order across trains is rejected
    Given a temp rp config with the reference optical trains
    And rp is started with that config file
    When I GET /api/config
    And I PUT /api/config with the fetched config after setting "/equipment/optical_trains" to:
      """
      [ { "id": "main",  "devices": ["main-focuser", "guide-focuser", "main-cam"] },
        { "id": "guide", "devices": ["guide-focuser", "main-focuser", "guide-cam"] } ]
      """
    Then the config response status should be 200
    And the apply status should be "invalid"
    And the apply errors should name path "equipment.optical_trains"

  Scenario: A non-positive train focal length is rejected at parse
    Given a temp rp config with the reference optical trains
    And rp is started with that config file
    When I GET /api/config
    And I PUT /api/config with the fetched config after setting "/equipment/optical_trains/0/focal_length_mm" to "-100.0"
    Then the config response status should be 400
    And the config response body should contain "focal_length_mm must be a positive finite number"

  Scenario: An unknown train purpose is rejected at parse
    Given a temp rp config with the reference optical trains
    And rp is started with that config file
    When I GET /api/config
    And I PUT /api/config with the fetched config after setting "/equipment/optical_trains/0/purpose" to "solar"
    Then the config response status should be 400
    And the config response body should contain "unknown variant `solar`"

  Scenario Outline: A train auto_focus block with a non-positive sweep field is rejected at parse
    Given a temp rp config with the reference optical trains
    And rp is started with that config file
    When I GET /api/config
    And I PUT /api/config with the fetched config after setting "/equipment/optical_trains/0/auto_focus/<field>" to "<value>"
    Then the config response status should be 400
    And the config response body should contain "<named>"

    Examples:
      | field      | value | named                                          |
      | step_size  | 0     | auto_focus.step_size must be a positive integer  |
      | half_width | -5    | auto_focus.half_width must be a positive integer |

  Scenario Outline: Retired pre-train config keys are rejected as unknown fields
    Given a temp rp config with the reference optical trains
    And rp is started with that config file
    When I GET /api/config
    And I PUT /api/config with the fetched config after inserting "<pointer>" set to "<value>"
    Then the config response status should be 400
    And the config response body should contain "<named>"

    Examples:
      | pointer                              | value                        | named           |
      | /guider                              | {"url": "http://127.0.0.1:1"} | guider          |
      | /equipment/cameras/0/focal_length_mm | 1000.0                       | focal_length_mm |
      | /equipment/focusers/0/camera_id      | "main-cam"                   | camera_id       |
      | /equipment/filter_wheels/0/camera_id | "main-cam"                   | camera_id       |

  Scenario: Capture derives the optics block from the camera's train focal length
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator in an imaging train with focal length 1000.0
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 1000 ms
    And I fetch the document for the captured document_id
    Then the document response status should be 200
    And the document optics focal length should be 1000.0
    And the document optics pixel scale should equal 206.265 times pixel size over focal length

  Scenario: Capture through a camera outside any train carries no optics block
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 1000 ms
    And I fetch the document for the captured document_id
    Then the document response status should be 200
    And the document body should not contain "optics"

  Scenario Outline: An auto_focus block field that does not fit the train's purpose is rejected
    Given a temp rp config with the reference optical trains
    And rp is started with that config file
    When I GET /api/config
    And I PUT /api/config with the fetched config after setting "<pointer>" to "<value>"
    Then the config response status should be 400
    And the config response body should contain "<named>"

    Examples:
      | pointer                                  | value                                                                                     | named                          |
      | /equipment/optical_trains/0/auto_focus   | {"step_size": 100, "half_width": 1000, "min_area": 4, "max_area": 500}                    | duration                       |
      | /equipment/optical_trains/0/auto_focus   | {"duration": "3s", "step_size": 100, "half_width": 1000, "min_area": 4, "max_area": 500, "frames_per_step": 3} | frames_per_step |
      | /equipment/optical_trains/1/auto_focus   | {"step_size": 50, "half_width": 500, "duration": "3s"}                                    | duration                       |
      | /equipment/optical_trains/1/auto_focus   | {"half_width": 500}                                                                       | step_size                      |
      | /equipment/mount/guiding/focus_watch/window        | 2   | focus_watch.window        |
      | /equipment/mount/guiding/focus_watch/degrade_ratio | 1.0 | focus_watch.degrade_ratio |

  Scenario: Capture addresses the train's terminal camera
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator in an imaging train with focal length 1000.0
    And an MCP client connected to rp
    When the MCP client calls "capture" with train "main" for 1000 ms
    And I fetch the document for the captured document_id
    Then the document response status should be 200
    And the document optics focal length should be 1000.0

  Scenario: Capture rejects train addressing combined with a camera id
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator in an imaging train with focal length 1000.0
    And an MCP client connected to rp
    When the MCP client calls "capture" with both camera "main-cam" and train "main" for 1000 ms
    Then the tool call should return an error
    And the error message should contain "mutually exclusive"

  Scenario: Capture through an unknown train is rejected
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator in an imaging train with focal length 1000.0
    And an MCP client connected to rp
    When the MCP client calls "capture" with train "nonexistent" for 1000 ms
    Then the tool call should return an error
    And the error message should contain "train not found: nonexistent"

  Scenario: set_filter addresses the train's sole filter wheel
    Given a running Alpaca simulator
    And rp is running with a camera and filter wheel on the simulator in an imaging train
    And an MCP client connected to rp
    When the MCP client calls "set_filter" with train "main" and filter "Red"
    And the MCP client calls "get_filter" with filter wheel "main-fw"
    Then the current filter should be "Red"

  Scenario: set_filter through a train without a filter wheel is rejected
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator in an imaging train with focal length 1000.0
    And an MCP client connected to rp
    When the MCP client calls "set_filter" with train "main" and filter "Red"
    Then the tool call should return an error
    And the error message should contain "train 'main' has no filter wheel"

  # --- Cover calibrators as train members (calibrator-flats-provider plan, D3) ---
  # A cover calibrator sits at the objective: it may only be the first
  # device of a train, a train holds one at most (a separate dust cap and
  # light panel on one train is not modeled), and one calibrator may be
  # first in several trains — a flip-flat over the OTA covers the main
  # camera and the OAG guide camera alike.

  Scenario: A cover calibrator first in a train is accepted
    Given a temp rp config with the reference optical trains
    And rp is started with that config file
    When I GET /api/config
    And I PUT /api/config with the fetched config after setting "/equipment/optical_trains" to:
      """
      [ { "id": "main", "devices": ["flat-panel", "main-focuser", "main-fw", "main-cam"] } ]
      """
    Then the config response status should be 200
    And the apply status should be "ok"

  Scenario: A cover calibrator after another device is rejected naming the position
    Given a temp rp config with the reference optical trains
    And rp is started with that config file
    When I GET /api/config
    And I PUT /api/config with the fetched config after setting "/equipment/optical_trains" to:
      """
      [ { "id": "main", "devices": ["main-focuser", "flat-panel", "main-cam"] } ]
      """
    Then the config response status should be 200
    And the apply status should be "invalid"
    And the apply errors should name path "equipment.optical_trains.0.devices.1"
    And the apply error at path "equipment.optical_trains.0.devices.1" should mention "must be the first device"

  Scenario: Two cover calibrators in one train are rejected
    Given a temp rp config with the reference optical trains
    And rp is started with that config file
    When I GET /api/config
    And I PUT /api/config with the fetched config after setting "/equipment/optical_trains" to:
      """
      [ { "id": "main", "devices": ["dust-cap", "flat-panel", "main-cam"] } ]
      """
    Then the config response status should be 200
    And the apply status should be "invalid"
    And the apply errors should name path "equipment.optical_trains.0.devices.1"
    And the apply error at path "equipment.optical_trains.0.devices.1" should mention "at most one cover calibrator"

  Scenario: One cover calibrator shared as the first device of two trains is accepted
    Given a temp rp config with the reference optical trains
    And rp is started with that config file
    When I GET /api/config
    And I PUT /api/config with the fetched config after setting "/equipment/optical_trains" to:
      """
      [ { "id": "main",  "devices": ["flat-panel", "main-focuser", "main-cam"] },
        { "id": "guide", "devices": ["flat-panel", "guide-focuser", "guide-cam"] } ]
      """
    Then the config response status should be 200
    And the apply status should be "ok"

  # --- get_train_info (calibrator-flats-provider plan, D4) ---
  # A read over the train model: the terminal camera, the sole filter
  # wheel with its configured filter names in position order, the cover
  # calibrator, the focusers, the sole rotator, purpose and focal length.
  # Members the train lacks — or has several of, for the sole-member
  # fields — are null. No device is touched.

  Scenario: get_train_info lists the wheel's filters and the calibrator
    Given a running Alpaca simulator
    And rp is running with a cover calibrator, a filter wheel and a camera on the simulator in an imaging train
    And an MCP client connected to rp
    When the MCP client calls "get_train_info" with train "main"
    Then the tool call should succeed
    And the tool result "camera_id" should be "main-cam"
    And the tool result "filter_wheel_id" should be "main-fw"
    And the tool result list "filters" should be exactly "Luminance,Red,Green,Blue"
    And the tool result "calibrator_id" should be "flat-panel"
    And the tool result "purpose" should be "imaging"
    And the tool result list "focusers" should be exactly ""
    And the tool result "rotator_id" should be null
    And the tool result "focal_length_mm" should be null

  Scenario: get_train_info for an unknown train is an error naming it
    Given a running Alpaca simulator
    And rp is running with a cover calibrator, a filter wheel and a camera on the simulator in an imaging train
    And an MCP client connected to rp
    When the MCP client calls "get_train_info" with train "nope"
    Then the tool call should return an error
    And the error message should contain "train not found: nope"

  Scenario: Tool catalog includes get_train_info
    Given a running Alpaca simulator
    And rp is running with a cover calibrator, a filter wheel and a camera on the simulator in an imaging train
    And an MCP client connected to rp
    When the MCP client lists available tools
    Then the tool list should include "get_train_info"
