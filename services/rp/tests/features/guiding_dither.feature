@wip @serial
Feature: Dither on the guider's contract
  dither moves the guider's lock position and blocks until the run has
  settled again. The amount is the call's pixels, or the guiding
  config's dither_pixels when the call names none. unit says what the
  amount is. guide_px, the default, sends it as guide-camera pixels in
  amount_px. arcsec sends it as arcseconds in amount_arcsec, which the
  guider converts with its own pixel scale, so rp needs no guiding
  train for it. main_px multiplies it by the pixel scale of the one
  imaging train, 206.265 x pixel_size_x_um / focal_length_mm, and
  sends arcseconds; its error names the conversion input that is
  missing. dither_started carries the amount as it is sent, one of
  amount_px and amount_arcsec with the other null, and the dither's
  deadline as max_duration_ms: the resolved settle timeout plus 15 s,
  and no deadline when no settle timeout is resolved. The guider
  refuses a dither outside the state guiding with not_guiding, a paused
  or star-lost run included, and its refusals reach the caller with
  their code and message. A dither cancelled mid-settle sends the
  guider dither/abort and fails with the cancellation's reason.

  The stub guider settles a dither it accepts with the figures of a
  start: an RMS of 0.21 px in RA, 0.18 px in Dec and 0.28 px in total
  over 12 samples. It refuses a dither with not_guiding in every state
  but guiding.

  Scenario: A guide-pixel amount reaches the guider as amount_px and the settled figures come back
    Given a running Alpaca simulator
    And a stub guider in state "guiding"
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "dither" with pixels 5.0 and ra_only true
    Then the guider result should contain "state" with value "guiding"
    And the guider result should contain "rms_ra_px" with number 0.21
    And the guider result should contain "rms_dec_px" with number 0.18
    And the guider result should contain "total_rms_px" with number 0.28
    And the guider result should contain "sample_count" with number 12
    And the stub guider should have received a dither request with amount_px 5.0 and ra_only true

  Scenario: A dither that names no amount sends the guiding config's dither_pixels as guide-camera pixels
    Given a running Alpaca simulator
    And a stub guider in state "guiding"
    And rp is running with a camera on the simulator and guider dither_pixels 3.5
    And an MCP client connected to rp
    When the MCP client calls "dither" with no arguments
    Then the stub guider should have received a dither request with amount_px 3.5 and ra_only false

  Scenario: An arcsecond amount reaches the guider as amount_arcsec on a rig with no guiding train
    Given a running Alpaca simulator
    And a stub guider in state "guiding"
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "dither" with pixels 10.0 and unit "arcsec"
    Then the guider result should contain "state" with value "guiding"
    And the stub guider should have received a dither request with amount_arcsec 10.0 and ra_only false

  Scenario: A main_px amount reaches the guider in arcseconds at the imaging train's pixel scale
    Given a running Alpaca simulator
    And a stub guider in state "guiding"
    And rp is running with the simulator camera in an imaging train with focal length 400.0
    And an MCP client connected to rp
    When the MCP client calls "dither" with pixels 10.0 and unit "main_px"
    Then the guider result should contain "state" with value "guiding"
    And the stub guider should have received a dither request with amount_arcsec equal to 10.0 pixels at the imaging train's 400.0 mm pixel scale

  Scenario: main_px on a rig with no imaging train names the missing train
    Given a running Alpaca simulator
    And a stub guider in state "guiding"
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "dither" with pixels 10.0 and unit "main_px"
    Then the tool call should return an error
    And the error message should contain "imaging train"
    And the stub guider should not have received a "/dither" request

  Scenario: main_px with an imaging train that has no focal length names focal_length_mm
    Given a running Alpaca simulator
    And a stub guider in state "guiding"
    And rp is running with a camera-only imaging train "main" on the simulator
    And an MCP client connected to rp
    When the MCP client calls "dither" with pixels 10.0 and unit "main_px"
    Then the tool call should return an error
    And the error message should contain "focal_length_mm"
    And the stub guider should not have received a "/dither" request

  Scenario: main_px with an imaging camera that is not connected names the pixel size
    Given a stub guider in state "guiding"
    And rp is running with an offline camera in an imaging train with focal length 400.0
    And an MCP client connected to rp
    When the MCP client calls "dither" with pixels 10.0 and unit "main_px"
    Then the tool call should return an error
    And the error message should contain "pixel size"
    And the stub guider should not have received a "/dither" request

  Scenario Outline: dither_started carries a <unit> amount as it is sent, and the settle timeout plus 15 s as its deadline
    Given a running Alpaca simulator
    And a stub guider in state "guiding"
    And a test webhook receiver subscribed to "dither_started" and "dither_settled"
    And rp is running with a camera on the simulator and guider settle pixels 0.8 time "8s" timeout "40s"
    And an MCP client connected to rp
    When the MCP client calls "dither" with pixels 5.0 and unit "<unit>"
    Then the test webhook receiver should receive a "dither_started" event
    And the test webhook receiver should receive a "dither_settled" event
    And the "dither_started" event payload field "amount_px" should be the JSON <amount_px>
    And the "dither_started" event payload field "amount_arcsec" should be the JSON <amount_arcsec>
    And the "dither_started" event payload field "ra_only" should be the JSON false
    And the "dither_started" event payload field "settle_timeout" should be "40s"
    And the "dither_started" event should carry a max_duration_ms of 55000
    And the "dither_settled" event payload field "total_rms_px" should be the number 0.28
    And the "dither_settled" event payload field "sample_count" should be the number 12

    Examples:
      | unit     | amount_px | amount_arcsec |
      | guide_px | 5.0       | null          |
      | arcsec   | null      | 5.0           |

  Scenario: A dither with no settle timeout resolved carries no deadline
    Given a running Alpaca simulator
    And a stub guider in state "guiding"
    And a test webhook receiver subscribed to "dither_started"
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "dither" with pixels 5.0 and ra_only false
    Then the test webhook receiver should receive a "dither_started" event
    And the "dither_started" event reserves the deadline fields as absent

  Scenario Outline: The guider refuses a dither in the state <state>, and the refusal reaches the caller
    Given a running Alpaca simulator
    And a stub guider in state "<state>"
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "dither" with pixels 5.0 and ra_only false
    Then the tool call should return an error
    And the error message should contain "not_guiding"

    Examples:
      | state       |
      | idle        |
      | looping     |
      | calibrating |
      | paused      |
      | star_lost   |

  Scenario Outline: A dither the guider fails with <code> reaches the caller and emits dither_failed
    Given a running Alpaca simulator
    And a stub guider in state "guiding"
    And the stub guider answers "/dither" requests with error code "<code>" and message "<message>"
    And a test webhook receiver subscribed to "dither_failed" and "dither_settled"
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "dither" with pixels 5.0 and ra_only false
    Then the tool call should return an error
    And the error message should contain "<code>"
    And the error message should contain "<message>"
    And the test webhook receiver should receive a "dither_failed" event
    And the "dither_failed" event payload field "error" should contain "<code>"
    And the test webhook receiver should not have received a "dither_settled" event

    Examples:
      | code           | message                               |
      | settle_timeout | the settle did not complete in time   |
      | guide_failed   | the run stopped: the mount is slewing |
      | stopped        | the run was stopped                   |

  Scenario: A dither whose caller disconnects mid-settle sends the guider dither/abort
    Given a running Alpaca simulator
    And a stub guider in state "guiding"
    And the stub guider settles after 2500 ms
    And a test webhook receiver subscribed to "dither_started" and "dither_failed"
    And rp is running with a camera on the simulator
    When a second MCP client starts a dither of 3.0 pixels in the background
    And the test webhook receiver has received a "dither_started" event
    And the second MCP client disconnects
    Then the stub guider should have received a "/dither/abort" request within 2 seconds
    And the test webhook receiver should receive a "dither_failed" event
    And the "dither_failed" event payload field "error" should be "cancelled: client disconnected"
