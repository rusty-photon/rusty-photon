@serial
Feature: Mount motion gate
  Mount motion and in-flight imaging exposures are serialized by an
  rp-internal readers-writer gate on the singular mount: slew and
  dither take the gate exclusively, and captures through a camera
  terminating any train, imaging or guiding, hold it shared. A
  pending motion blocks new captures through a train's camera (fair
  FIFO — no starvation): in-flight subs complete, the motion runs and
  settles, held captures then start. rp emits the point event
  mount_motion_pending when a motion has to wait, and the motion's
  own *_started envelope only after the gate is acquired, so
  predictive deadlines never include gate wait. Captures through a
  camera outside every train bypass the gate (trains are enrichment,
  not a gate). A guiding train's camera is not exempt: a client's
  capture through the PHD2 camera facade is a focus sample that a
  dither mid-exposure would trail. The gate is process-wide: concurrent MCP sessions — an
  orchestrator plus an operator UI — contend on the same gate, which
  is what these scenarios drive with a second client.

  Scenario: A dither waits for the in-flight imaging-train exposure
    Given a test webhook receiver subscribed to the events "exposure_started, exposure_complete, dither_started, dither_settled, mount_motion_pending"
    And rp is running with a camera on the simulator in imaging train "main" and a stub guider
    When a second MCP client starts a "3s" capture of camera "main-cam" in the background
    And the test webhook receiver has received an "exposure_started" event
    And the MCP client calls "dither" with pixels 3.0 and ra_only false
    Then the "exposure_complete" event should have been emitted before the "dither_started" event
    And the test webhook receiver should receive a "mount_motion_pending" event
    And the "mount_motion_pending" event payload field "operation" should be "dither"
    And the background "capture" call should succeed

  Scenario: A capture requested behind a pending dither starts only after the dither settles
    Given a test webhook receiver subscribed to the events "exposure_started, exposure_complete, dither_started, dither_settled, mount_motion_pending"
    And rp is running with a camera on the simulator in imaging train "main" and a stub guider
    When a second MCP client starts a "3s" capture of camera "main-cam" in the background
    And the test webhook receiver has received an "exposure_started" event
    And a second MCP client starts a dither of 3.0 pixels in the background
    And the test webhook receiver has received a "mount_motion_pending" event
    And the MCP client calls "capture" with camera "main-cam" for 300 ms
    Then the "exposure_complete" event should have been emitted before the "dither_started" event
    And the last "exposure_started" event should have been emitted after the "dither_settled" event
    And the tool result should contain an image path
    And the background "capture" call should succeed
    And the background "dither" call should succeed

  Scenario: A slew waits for the in-flight imaging-train exposure
    Given a test webhook receiver subscribed to the events "exposure_started, exposure_complete, slew_started, mount_motion_pending"
    And rp is running with a camera on the simulator in imaging train "main" and a mount
    And the mount is unparked
    And the mount tracking is set to true
    When a second MCP client starts a "3s" capture of camera "main-cam" in the background
    And the test webhook receiver has received an "exposure_started" event
    And the MCP client calls "slew" with ra "10.6847" dec "41.2689"
    Then the "exposure_complete" event should have been emitted before the "slew_started" event
    And the test webhook receiver should receive a "mount_motion_pending" event
    And the "mount_motion_pending" event payload field "operation" should be "slew"
    And the background "capture" call should succeed

  Scenario: A capture through an imaging-train camera waits for the in-flight dither
    Given a test webhook receiver subscribed to the events "exposure_started, dither_started, dither_settled, mount_motion_pending"
    And rp is running with a camera on the simulator in imaging train "main" and a stub guider settling after 2500 ms
    When a second MCP client starts a dither of 3.0 pixels in the background
    And the test webhook receiver has received a "dither_started" event
    And the MCP client calls "capture" with camera "main-cam" for 200 ms
    Then the "dither_settled" event should have been emitted before the "exposure_started" event
    And the tool result should contain an image path
    And the background "dither" call should succeed

  Scenario: A capture through a camera outside any train ignores an in-flight dither
    Given a test webhook receiver subscribed to the events "exposure_started, dither_started, dither_settled, mount_motion_pending"
    And rp is running with a camera on the simulator and a stub guider settling after 2500 ms
    When a second MCP client starts a dither of 3.0 pixels in the background
    And the test webhook receiver has received a "dither_started" event
    And the MCP client calls "capture" with camera "main-cam" for 200 ms
    Then the "exposure_started" event should have been emitted before the "dither_settled" event
    And the tool result should contain an image path
    And the background "dither" call should succeed
    And the test webhook receiver should not have received a "mount_motion_pending" event

  Scenario: A capture through a guiding-train camera waits for the in-flight dither
    Given a test webhook receiver subscribed to the events "exposure_started, dither_started, dither_settled, mount_motion_pending"
    And rp is running with a camera on the simulator in guiding train "guide" and a stub guider settling after 2500 ms
    When a second MCP client starts a dither of 3.0 pixels in the background
    And the test webhook receiver has received a "dither_started" event
    And the MCP client calls "capture" with camera "main-cam" for 200 ms
    Then the "dither_settled" event should have been emitted before the "exposure_started" event
    And the tool result should contain an image path
    And the background "dither" call should succeed
