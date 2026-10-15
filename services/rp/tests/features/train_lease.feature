@serial
Feature: Train lease
  A training run on an optical train is many frames with focuser moves
  between them. rp's train lease holds three things off the run for its
  whole length: rp's own captures, focuser moves and filter changes on
  the train's light path, mount motion (the lease holds the motion gate
  shared, so slews and dithers wait), and, on a guiding train, PHD2
  guiding, through the camera facade's sweep lease. The caller names the
  lease id, acquires it before the first move, passes it on every
  capture, move_focuser and set_filter of the run, and releases it after
  the put-back. A call that names no lease, through a camera or a device
  the lease reserves, waits for the lease to end. An acquire whose
  reservation overlaps another lease is refused at once, naming it, and
  so is an auto_focus sweep over a leased light path. A call that names
  a lease which is not held is refused, saying why. An imaging train's
  lease takes no facade lease; a guiding train's lease requires its
  camera to be the PHD2 camera facade, which rp recognizes by the
  rustyphoton:sweep-lease-acquire Action.

  Scenario: A granted lease names what it moves, holds still and holds
    Given rp is running with imaging train "main" of a focuser, a filter wheel and a camera on the simulator
    When the MCP client calls "acquire_train_lease" for train "main" with lease id "lease-a"
    Then the train lease result field "lease_id" should be "lease-a"
    And the train lease result field "focuser_id" should be "main-focuser"
    And the train lease result list "filter_wheel_ids" should be "main-fw"
    And the train lease result list "held_focuser_ids" should be empty
    And the train lease result list "held_rotator_ids" should be empty
    And the train lease result list "cameras" should be "main-cam"
    And the train lease result field "facade_lease" should be false
    And the train lease result should carry an RFC 3339 "ends_by" time

  Scenario: Calls that name the held lease run under it
    Given rp is running with imaging train "main" of a focuser, a filter wheel and a camera on the simulator
    And the MCP client holds train lease "lease-a" on train "main"
    When the MCP client calls "move_focuser" with focuser "main-focuser" to position 25100 under lease "lease-a"
    Then the train lease call should succeed
    When the MCP client calls "set_filter" with filter wheel "main-fw" and filter "Red" under lease "lease-a"
    Then the train lease call should succeed
    When the MCP client calls "capture" with camera "main-cam" for 200 ms under lease "lease-a"
    Then the train lease call should succeed

  Scenario: A capture that names no lease waits for the lease to end
    Given a test webhook receiver subscribed to the events "exposure_started, exposure_complete"
    And rp is running with imaging train "main" of a focuser, a filter wheel and a camera on the simulator
    And the MCP client holds train lease "lease-a" on train "main"
    When a second MCP client starts a "200ms" capture of camera "main-cam" in the background
    Then no "exposure_started" event arrives within 1500 ms
    When the MCP client calls "release_train_lease" with lease id "lease-a"
    Then the train lease result field "released" should be true
    And the background "capture" call should succeed

  Scenario: A dither waits for a granted lease to end
    Given a test webhook receiver subscribed to the events "dither_started, mount_motion_pending"
    And rp is running with imaging train "main" of a focuser, a filter wheel and a camera on the simulator and a stub guider
    And the MCP client holds train lease "lease-a" on train "main"
    When a second MCP client starts a dither of 3.0 pixels in the background
    Then the test webhook receiver should receive a "mount_motion_pending" event
    And the "mount_motion_pending" event payload field "operation" should be "dither"
    When the MCP client calls "release_train_lease" with lease id "lease-a"
    Then the background "dither" call should succeed

  Scenario: A second lease over the same light path is refused at once, naming the first
    Given rp is running with imaging train "main" of a focuser, a filter wheel and a camera on the simulator
    And the MCP client holds train lease "lease-a" on train "main"
    When the MCP client calls "acquire_train_lease" for train "main" with lease id "lease-b"
    Then the tool call should return an error
    And the error message should contain "lease 'lease-a'"

  Scenario: An auto_focus sweep over a leased light path is refused at once, naming the lease
    Given rp is running with imaging train "main" of a focuser, a filter wheel and a camera on the simulator
    And the MCP client holds train lease "lease-a" on train "main"
    When the MCP client calls auto_focus with camera "main-cam" and focuser "main-focuser"
    Then the tool call should return an error
    And the error message should contain "lease 'lease-a'"

  Scenario: A lease id is never granted twice
    Given rp is running with imaging train "main" of a focuser, a filter wheel and a camera on the simulator
    And the MCP client holds train lease "lease-a" on train "main"
    When the MCP client calls "release_train_lease" with lease id "lease-a"
    And the MCP client calls "acquire_train_lease" for train "main" with lease id "lease-a"
    Then the tool call should return an error
    And the error message should contain "lease id 'lease-a' was used before"

  Scenario: A release that arrives before its acquire turns the acquire away
    Given rp is running with imaging train "main" of a focuser, a filter wheel and a camera on the simulator
    When the MCP client calls "release_train_lease" with lease id "lease-a"
    Then the train lease result field "released" should be false
    When the MCP client calls "acquire_train_lease" for train "main" with lease id "lease-a"
    Then the tool call should return an error
    And the error message should contain "lease id 'lease-a' was used before"

  Scenario: A call under a released lease is refused, saying why
    Given rp is running with imaging train "main" of a focuser, a filter wheel and a camera on the simulator
    And the MCP client holds train lease "lease-a" on train "main"
    When the MCP client calls "release_train_lease" with lease id "lease-a"
    And the MCP client calls "move_focuser" with focuser "main-focuser" to position 25100 under lease "lease-a"
    Then the tool call should return an error
    And the error message should contain "lease 'lease-a' is not held: it was released"

  Scenario: A capture under the lease through a camera it does not hold is refused
    Given rp is running with imaging train "main" of a focuser, a filter wheel and a camera on the simulator and an offline camera "side-cam" in no train
    And the MCP client holds train lease "lease-a" on train "main"
    When the MCP client calls "capture" with camera "side-cam" for 200 ms under lease "lease-a"
    Then the tool call should return an error
    And the error message should contain "camera 'side-cam' is not held by lease 'lease-a'"

  Scenario: A move under the lease of a focuser it holds still is refused
    Given rp is running with imaging train "main" behind an offline upstream focuser "upstream-focuser"
    When the MCP client calls "acquire_train_lease" for train "main" with lease id "lease-a"
    Then the train lease result list "held_focuser_ids" should be "upstream-focuser"
    When the MCP client calls "move_focuser" with focuser "upstream-focuser" to position 25100 under lease "lease-a"
    Then the tool call should return an error
    And the error message should contain "lease 'lease-a' holds focuser 'upstream-focuser' still"

  Scenario: A train without a focuser takes no lease
    Given rp is running with a camera-only imaging train "solo" on the simulator
    When the MCP client calls "acquire_train_lease" for train "solo" with lease id "lease-a"
    Then the tool call should return an error
    And the error message should contain "train 'solo' has no focuser"

  Scenario: A lease longer than ten minutes is refused
    Given rp is running with imaging train "main" of a focuser, a filter wheel and a camera on the simulator
    When the MCP client calls "acquire_train_lease" for train "main" with lease id "lease-a" for "11m"
    Then the tool call should return an error
    And the error message should contain "max_duration"

  Scenario: A guiding train's lease takes the camera facade's sweep lease and gives it back
    Given rp is running with guiding train "guide" of a focuser on the simulator and the stub camera facade
    When the MCP client calls "acquire_train_lease" for train "guide" with lease id "lease-g"
    Then the train lease result field "facade_lease" should be true
    And the stub camera facade should hold a sweep lease under a token of 32 lowercase hex digits with a ttl_s of 60
    When the MCP client calls "release_train_lease" with lease id "lease-g"
    Then the stub camera facade should hold no sweep lease
    And the stub camera facade's last release should name the token it granted

  Scenario: A guiding train whose PHD2 is guiding is refused before anything moves
    Given rp is running with guiding train "guide" of a focuser on the simulator and the stub camera facade
    And the stub camera facade reports PHD2 "Guiding"
    When the MCP client calls "acquire_train_lease" for train "guide" with lease id "lease-g"
    Then the tool call should return an error
    And the error message should contain "the camera facade 'guide-cam' refused the sweep lease"
    And the error message should contain "PHD2 is Guiding"
    And the stub camera facade should hold no sweep lease

  Scenario: A guiding train whose camera is not the facade is refused, naming what is missing
    Given rp is running with guiding train "guide" of a focuser and a camera on the simulator
    When the MCP client calls "acquire_train_lease" for train "guide" with lease id "lease-g"
    Then the tool call should return an error
    And the error message should contain "does not offer the sweep lease (rustyphoton:sweep-lease-acquire)"
