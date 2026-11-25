Feature: Dither, its abort, and pausing the corrections
  `POST /api/v1/dither` moves the lock by a draw within `amount_px`, or
  `amount_arcsec` at the pixel scale, and answers once guiding has settled
  at the new lock (docs/services/guider.md § `POST /api/v1/dither`,
  § Dither, settle and abort). It is valid only in `guiding`.
  `POST /api/v1/dither/abort` sets the lock to where the stars are and
  answers a waiting dither with `dither_aborted`; it takes no turn and
  answers `"aborted": false` with no dither in flight.
  `POST /api/v1/guiding/pause` stops the corrections, and with `full`
  stops exposing too; `POST /api/v1/guiding/resume` takes them up again
  with the guide stars and the lock kept (§ `POST /api/v1/guiding/pause`
  and `resume`).

  The simulated devices of each scenario are a 320 × 240 camera with 2 µm
  pixels under a sky of 6″ seeing and three stars, and a mount on the East
  side of the pier at declination 0° that guides at the sidereal rate. The
  guider polls them every 100 ms, exposes for 200 ms, and settles within
  1.5 px for 1 s, in at most 30 s. Each scenario starts with a stored
  calibration of the simulated rig. The simulated sky holds still, so a
  scenario whose settle must not come has the mount drift 1″ a minute.

  Background:
    Given the simulated camera and mount, both connected
    And the simulated sky holds 3 stars
    And the simulated mount guides at the sidereal rate
    And a guider configuration naming both devices and a focal length of 206.265 mm
    And the guider configuration settles within 1.5 px for 1 s in at most 30 s
    And the state directory holds a calibration of the simulated rig

  Scenario: A dither moves the lock and answers once guiding has settled there
    Given the guider is started
    And the guider is guiding
    When a client asks the guider to dither with the body {"amount_px": 4.0}
    Then the request answers 200 with the state "guiding" and a settled answer
    And the simulated mount has received a pulse

  Scenario: A dither in arcseconds is taken at the pixel scale
    Given the guider is started
    And the guider is guiding
    When a client asks the guider to dither with the body {"amount_arcsec": 8.0, "ra_only": true}
    Then the request answers 200 with the state "guiding" and a settled answer

  Scenario Outline: A dither the guider cannot read is refused before anything else
    Given the guider is started
    And the guider is guiding
    When a client asks the guider to dither with the body <body>
    Then the request answers 400 with the error "invalid_request"

    Examples:
      | body                                         |
      | {}                                           |
      | {"amount_px": 4.0, "amount_arcsec": 8.0}     |
      | {"amount_px": 0.0}                           |
      | {"amount_px": 4.0, "settle": {"time": "x"}}  |

  Scenario: A dither outside guiding is refused with not_guiding
    Given the guider is started
    And the guider is looping
    When a client asks the guider to dither with the body {"amount_px": 4.0}
    Then the request answers 409 with the error "not_guiding"

  Scenario: An abort answers the waiting dither with dither_aborted, and guiding goes on
    Given the simulated mount drifts 1″ a minute east
    And the guider is started
    And the guider is guiding
    And a client asks the guider to dither in the background with the body {"amount_px": 4.0, "settle": {"pixels": 0.0001, "time": "1s", "timeout": "60s"}}
    And the guider's stats report settling true
    When a client asks the guider to abort the dither
    Then the request answers 200 with the body {"state": "guiding", "aborted": true}
    And the background request answers 409 with the error "dither_aborted"
    And the guider's stats report the state "guiding"

  Scenario: An abort with no dither in flight changes nothing
    Given the guider is started
    When a client asks the guider to abort the dither
    Then the request answers 200 with the body {"state": "idle", "aborted": false}

  Scenario: A pause holds the corrections and the frames go on; a resume takes them up again
    Given the guider is started
    And the guider is guiding
    When a client asks the guider to pause
    Then the request answers 200 with the body {"state": "paused"}
    And the guider's metrics gain at least 3 more frames
    And a client asks the guider to resume
    And the request answers 200 with the body {"state": "guiding"}
    And the guider's stats report the state "guiding"
    And the guider's totals count at least 1 interruption

  Scenario: A full pause stops the exposures too
    Given the guider is started
    And the guider is guiding
    When a client asks the guider to pause with the body {"full": true}
    Then the request answers 200 with the body {"state": "paused"}
    And the simulated camera starts no exposure for 2 seconds
    And a client asks the guider to reselect the guide stars
    And the request answers 409 with the error "not_exposing"
    And a client asks the guider to resume
    And the request answers 200 with the body {"state": "guiding"}
    And the simulated camera starts exposures again

  Scenario Outline: A pause or a resume outside a run that guides is refused with not_guiding
    Given the guider is started
    And the guider is looping
    When a client asks the guider to <request>
    Then the request answers 409 with the error "not_guiding"

    Examples:
      | request |
      | pause   |
      | resume  |

  Scenario: A resume while guiding changes nothing
    Given the guider is started
    And the guider is guiding
    When a client asks the guider to resume
    Then the request answers 200 with the body {"state": "guiding"}
