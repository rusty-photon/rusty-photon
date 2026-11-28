@serial
Feature: rp against the guider service
  rp's guiding tools, its check of the guider's devices, its watch over
  a run, the guide camera guard, a frame's guiding block and the
  safety stop run here against the guider service itself: the guider
  binary, guiding guide-simulator-alpaca's camera and mount
  (docs/services/rp.md § Guider Service, docs/services/guider.md
  § Testing). The other guiding features run against bdd-infra's stub
  of the guider, which gives what the service cannot be made to give
  on demand: a refusal of each code, a guider that stops answering or
  times out, the requests it received, and the figures it settles
  with. The figures here are the guider's own, so the scenarios hold
  them to how they relate rather than to values.

  The simulated camera is 320 x 240 pixels of 2 µm under a sky of 6″
  seeing and three stars. The simulated mount points at declination 0°
  on the East side of the pier, tracking, and guides at the sidereal
  rate. Neither device is connected until rp connects it. The guider
  polls them every 100 ms and exposes for 200 ms through a focal
  length of 206.265 mm, which is 2″ a pixel. It calibrates over 6 px
  in 4 steps and settles within 1.5 px for 1 s, in at most 30 s, and
  its state directory holds no calibration, so a start calibrates
  first. rp's mount is the simulated mount, and its guiding train
  "guide" holds the simulated camera as "guide-cam", so rp's check of
  the guider's devices passes on the UniqueIDs the devices report.
  rp's guiding block names the guider and no settle value.

  Scenario: start_guiding calibrates, settles and answers the guider's figures in pixels and in arcseconds
    Given a guider service on the simulated camera and mount
    And a test webhook receiver subscribed to "guide_started" and "guide_settled"
    And rp is running with the simulated mount and the simulated camera in a guiding train
    And an MCP client connected to rp
    When the MCP client calls "start_guiding" with no arguments
    Then the guider result should contain "state" with value "guiding"
    And the guider result's "sample_count" should be at least 1
    And the guider result's arcsecond figures should be its pixel figures at 2.0 arcseconds a pixel
    And the simulated mount should have received West pulses, then East, then North, then South
    And the guider service's stats should report the state "guiding"
    And the test webhook receiver should receive a "guide_started" event
    And the test webhook receiver should receive a "guide_settled" event

  Scenario: A start is refused when the guider guides on a camera that is not the guiding train's, naming both ids
    Given a running Alpaca simulator
    And a guider service on the simulated camera and mount
    And rp is running with the simulated mount and the simulator's camera in a guiding train
    And an MCP client connected to rp
    When the MCP client calls "start_guiding" with no arguments
    Then the tool call should return an error
    And the error message should contain "guide-simulator-camera"
    And the error message should name the simulator camera's unique id
    And the guider service's stats should report the state "idle"
    And the simulated mount should have received no pulse

  Scenario: get_guiding_stats returns the stats of the run the guider guides
    Given a guider service on the simulated camera and mount
    And rp is running with the simulated mount and the simulated camera in a guiding train
    And an MCP client connected to rp
    And rp has started guiding
    When the MCP client calls "get_guiding_stats"
    Then the guider result should contain "state" with value "guiding"
    And the guider result should contain "guiding" with the JSON true
    And the guider result should contain "pixel_scale_arcsec" with number 2.0
    And the guider result's "star_count" should be at least 1
    And the guider result should contain "last_stop" with the JSON null
    And the guider result should not contain "totals"

  Scenario: pause_guiding pauses the guider's run, and resume_guiding resumes it
    Given a guider service on the simulated camera and mount
    And rp is running with the simulated mount and the simulated camera in a guiding train
    And an MCP client connected to rp
    And rp has started guiding
    When the MCP client calls "pause_guiding" with full true
    Then the guider result should contain "state" with value "paused"
    And the guider service's stats should report the state "paused"
    When the MCP client calls "resume_guiding"
    Then the guider result should contain "state" with value "guiding"
    And the guider service's stats should report the state "guiding"

  Scenario: dither moves the guider's lock and answers once the run has settled again
    Given a guider service on the simulated camera and mount
    And a test webhook receiver subscribed to "dither_started" and "dither_settled"
    And rp is running with the simulated mount and the simulated camera in a guiding train
    And an MCP client connected to rp
    And rp has started guiding
    When the MCP client calls "dither" with pixels 3.0 and ra_only false
    Then the guider result should contain "state" with value "guiding"
    And the guider result's "sample_count" should be at least 1
    And the guider result's arcsecond figures should be its pixel figures at 2.0 arcseconds a pixel
    And the test webhook receiver should receive a "dither_started" event
    And the test webhook receiver should receive a "dither_settled" event
    And the "dither_started" event payload field "amount_px" should be the JSON 3.0

  Scenario: stop_guiding ends the guider's run as requested
    Given a guider service on the simulated camera and mount
    And a test webhook receiver subscribed to "guide_stopped"
    And rp is running with the simulated mount and the simulated camera in a guiding train
    And an MCP client connected to rp
    And rp has started guiding
    When the MCP client calls "stop_guiding"
    Then the guider result should contain "state" with value "idle"
    And the guider service's last stop should have the reason "requested"
    And the test webhook receiver should receive a "guide_stopped" event
    And the "guide_stopped" event payload field "reason" should be "requested"
    And the "guide_stopped" event payload field "stop_reason" should be the JSON null

  Scenario: A run the guider stops by itself when the mount slews is reported with the guider's reason
    Given a guider service on the simulated camera and mount
    And a test webhook receiver subscribed to "guide_settled" and "guide_stopped"
    And rp is running with the simulated mount and the simulated camera in a guiding train
    And an MCP client connected to rp
    When the MCP client calls "start_guiding" with no arguments
    And the test webhook receiver has received a "guide_settled" event
    And the simulated mount starts a slew of an hour
    Then the test webhook receiver should receive a "guide_stopped" event within 20 seconds
    And the "guide_stopped" event payload field "reason" should be "guider"
    And the "guide_stopped" event payload field "stop_reason" should be "mount_slewing"
    And the guider service's stats should report the state "idle"

  Scenario: Stars that leave the frame are reported lost once star_lost_after has passed, and their return ends the loss
    Given a guider service on the simulated camera and mount
    And a test webhook receiver subscribed to the events "guide_settled, guide_star_lost, guide_stopped"
    And rp's guiding config sets "star_lost_after" to the JSON "2s"
    And rp is running with the simulated mount and the simulated camera in a guiding train
    And an MCP client connected to rp
    When the MCP client calls "start_guiding" with no arguments
    And the test webhook receiver has received a "guide_settled" event
    And the simulated field jumps 100″ east
    Then the test webhook receiver should receive a "guide_star_lost" event within 20 seconds
    And the "guide_star_lost" event payload field "lost_for_ms" should be at least 2000
    When the simulated field jumps 100″ west
    Then the guider service's stats should report the state "guiding"
    And the test webhook receiver should not have received a "guide_stopped" event

  Scenario: A capture through the guide camera is refused while the guider guides, naming the state
    Given a guider service on the simulated camera and mount
    And rp is running with the simulated mount and the simulated camera in a guiding train
    And an MCP client connected to rp
    And rp has started guiding
    When the MCP client calls "capture" with camera "guide-cam" for 200 ms
    Then the tool call should return an error
    And the error message should contain "the guider is guiding; rp uses the guide camera 'guide-cam' only while the guider is idle"
    And the guider service's stats should report the state "guiding"

  Scenario: A frame taken while the guider guides records the guide error over its exposure
    Given a running Alpaca simulator
    And a guider service on the simulated camera and mount
    And rp is running with the simulated mount, the simulated camera in a guiding train and a camera on the simulator
    And an MCP client connected to rp
    And rp has started guiding
    When the MCP client calls "capture" with camera "main-cam" for 2000 ms
    And I fetch the document for the captured document_id
    Then the tool call should succeed
    And the document at "/guiding/guided_throughout" should be the JSON true
    And the document at "/guiding/guide_steps" should be a number of at least 1
    And the document at "/guiding/total_rms_px" should be a number
    And the document at "/guiding/star_lost" should be the JSON 0

  Scenario: The safety stop ends the guider's run and parks the mount
    Given a running Alpaca simulator
    And a safety monitor on the simulator
    And a guider service on the simulated camera and mount
    And a test webhook receiver subscribed to "guide_stopped"
    And rp is running with the simulated mount and the simulated camera in a guiding train
    And an MCP client connected to rp
    And rp has started guiding
    When the safety monitor reports unsafe
    Then the test webhook receiver should receive a "guide_stopped" event within 20 seconds
    And the "guide_stopped" event payload field "reason" should be "safety"
    And the simulated mount should report parked within 20 seconds
    And the guider service's last stop should have the reason "requested"

  Scenario: A start the guider refuses reaches the caller with the guider's code
    Given a guider service on the simulated camera and mount
    And a test webhook receiver subscribed to "guide_failed" and "guide_settled"
    And rp is running with the simulated mount and the simulated camera in a guiding train
    And an MCP client connected to rp
    And the simulated mount is parked
    When the MCP client calls "start_guiding" with no arguments
    Then the tool call should return an error
    And the error message should contain "mount_not_ready"
    And the test webhook receiver should receive a "guide_failed" event
    And the test webhook receiver should not have received a "guide_settled" event
    And the guider service's stats should report the state "idle"
