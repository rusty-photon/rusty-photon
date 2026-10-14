Feature: The sweep lease holds guiding off for a whole focus sweep

  A focus sweep through the camera facade takes many frames, with the
  focuser moving between them. Each frame checks that PHD2 is Stopped, but
  nothing in that check holds guiding off between two frames. The sweep
  lease does: three ASCOM Actions on the facade's Camera,
  rustyphoton:sweep-lease-acquire, rustyphoton:sweep-lease-renew and
  rustyphoton:sweep-lease-release. Each takes its Parameters as a JSON
  object and answers one.

  The caller names the token, 32 lowercase hex digits, so a caller whose
  acquire got no answer can still release it. Token A here stands for 32
  a digits and token B for 32 b digits. ttl_s is a whole number of seconds
  from 10 to 300, and 60 when left out.

  An acquire is granted only to a connected camera with no frame in
  flight, no lease held, and PHD2 Stopped, and only under a token the
  facade has not seen. While a lease is held, every guiding operation a
  capture refuses is refused with camera_busy (HTTP 409). Stop is never
  refused, and it ends the lease. So do a disconnect, a release and the
  TTL running out without a renewal. A renewal of a token that holds no
  lease fails saying why, and a release of one answers released false and
  is never an error. The lease does not gate captures. The TTL's lapse is
  pinned by unit tests on paused time rather than here, since the shortest
  TTL is 10 s.

  ASCOM error numbers: 0x401 invalid value, 0x407 not connected, 0x40B
  invalid operation.

  Scenario: The facade offers the three lease Actions
    Given a mock PHD2 2.6.14 with an 8 x 6 camera
    And the guider service is running with the camera facade enabled
    Then the facade's supported actions should include "rustyphoton:sweep-lease-acquire"
    And the facade's supported actions should include "rustyphoton:sweep-lease-renew"
    And the facade's supported actions should include "rustyphoton:sweep-lease-release"

  Scenario: A lease is granted to a Stopped PHD2 under the token the caller names
    Given a mock PHD2 2.6.14 with an 8 x 6 camera
    And the guider service is running with the camera facade enabled
    And the facade camera is connected
    When the client acquires a sweep lease under token A for 120 s
    Then the lease reply should name token A with a TTL of 120 s
    And the mock PHD2 should have received no request that changes its state

  Scenario: A lease asked for without a TTL lasts 60 s
    Given a mock PHD2 2.6.14 with an 8 x 6 camera
    And the guider service is running with the camera facade enabled
    And the facade camera is connected
    When the client acquires a sweep lease under token A
    Then the lease reply should name token A with a TTL of 60 s

  Scenario Outline: A guiding operation while a lease is held is refused with camera_busy
    Given a mock PHD2 2.6.14 with an 8 x 6 camera
    And the guider service is running with the camera facade enabled
    And the facade camera is connected
    When the client acquires a sweep lease under token A for 60 s
    And the client sends <operation>
    Then the response status should be 409
    And the response error should be "camera_busy" mentioning "sweep lease"
    And the mock PHD2 should not have received a "<rpc>" request

    Examples:
      | operation           | rpc               |
      | a guiding start     | guide             |
      | a pause             | set_paused        |
      | a resume            | set_paused        |
      | a dither            | dither            |
      | a calibration clear | clear_calibration |
      | a star reselect     | find_star         |

  Scenario: A frame captured under the lease is served as any other
    Given a mock PHD2 2.6.14 with an 8 x 6 camera
    And the guider service is running with the camera facade enabled
    And the facade camera is connected
    When the client acquires a sweep lease under token A for 60 s
    And the client starts a 0.1 s light exposure
    Then the facade image should become ready within 10 s

  Scenario: Renewing the held lease answers its token and TTL
    Given a mock PHD2 2.6.14 with an 8 x 6 camera
    And the guider service is running with the camera facade enabled
    And the facade camera is connected
    When the client acquires a sweep lease under token A for 30 s
    And the client renews the sweep lease under token A
    Then the lease reply should name token A with a TTL of 30 s

  Scenario: Releasing the lease lets guiding start again
    Given a mock PHD2 2.6.14 with an 8 x 6 camera
    And the guider service is running with the camera facade enabled
    And the facade camera is connected
    When the client acquires a sweep lease under token A for 60 s
    And the client releases the sweep lease under token A
    Then the release reply should be released true
    When the client starts guiding
    Then the response status should be 200
    And the response field "state" should be "guiding"

  Scenario: Stopping guiding ends the lease
    Given a mock PHD2 2.6.14 with an 8 x 6 camera
    And the guider service is running with the camera facade enabled
    And the facade camera is connected
    When the client acquires a sweep lease under token A for 60 s
    And the client stops guiding
    Then the response status should be 200
    When the client tries to renew the sweep lease under token A
    Then the facade call should fail with ASCOM error 0x40B mentioning "guiding stop"
    When the client releases the sweep lease under token A
    Then the release reply should be released false
    When the client starts guiding
    Then the response status should be 200

  Scenario: Disconnecting the facade camera ends the lease
    Given a mock PHD2 2.6.14 with an 8 x 6 camera
    And the guider service is running with the camera facade enabled
    And the facade camera is connected
    When the client acquires a sweep lease under token A for 60 s
    And the client disconnects the facade camera
    And the client tries to renew the sweep lease under token A
    Then the facade call should fail with ASCOM error 0x40B mentioning "disconnected"
    When the client starts guiding
    Then the response status should be 200

  Scenario: A lease is refused while PHD2 is guiding, naming its state
    Given a mock PHD2 2.6.14 with an 8 x 6 camera
    And the guider service is running with the camera facade enabled
    And the facade camera is connected
    When the client starts guiding
    And the client tries to acquire a sweep lease under token A
    Then the facade call should fail with ASCOM error 0x40B mentioning "Guiding"

  Scenario: A lease is refused while a facade frame is in flight, naming when it ends
    Given a mock PHD2 2.6.14 with an 8 x 6 camera
    And the guider service is running with the camera facade enabled
    And the facade camera is connected
    When the client starts a 5 s light exposure
    And the client tries to acquire a sweep lease under token A
    Then the facade call should fail with ASCOM error 0x40B mentioning "at the latest"

  Scenario: A second lease is refused while one is held
    Given a mock PHD2 2.6.14 with an 8 x 6 camera
    And the guider service is running with the camera facade enabled
    And the facade camera is connected
    When the client acquires a sweep lease under token A for 60 s
    And the client tries to acquire a sweep lease under token B
    Then the facade call should fail with ASCOM error 0x40B mentioning "already held"

  Scenario: A token is never granted twice
    Given a mock PHD2 2.6.14 with an 8 x 6 camera
    And the guider service is running with the camera facade enabled
    And the facade camera is connected
    When the client acquires a sweep lease under token A for 60 s
    And the client releases the sweep lease under token A
    And the client tries to acquire a sweep lease under token A
    Then the facade call should fail with ASCOM error 0x40B mentioning "used before"

  Scenario: An acquire that arrives after its own release is refused
    Given a mock PHD2 2.6.14 with an 8 x 6 camera
    And the guider service is running with the camera facade enabled
    And the facade camera is connected
    When the client releases the sweep lease under token A
    Then the release reply should be released false
    When the client tries to acquire a sweep lease under token A
    Then the facade call should fail with ASCOM error 0x40B mentioning "used before"

  Scenario: Renewing a token that was never acquired fails, saying so
    Given a mock PHD2 2.6.14 with an 8 x 6 camera
    And the guider service is running with the camera facade enabled
    And the facade camera is connected
    When the client tries to renew the sweep lease under token A
    Then the facade call should fail with ASCOM error 0x40B mentioning "never acquired"

  Scenario: A lease is refused on a camera that is not connected
    Given a mock PHD2 2.6.14 with an 8 x 6 camera
    And the guider service is running with the camera facade enabled
    When the client tries to acquire a sweep lease under token A
    Then the facade call should fail with ASCOM error 0x407

  Scenario Outline: Lease parameters of the wrong shape are refused as invalid values
    Given a mock PHD2 2.6.14 with an 8 x 6 camera
    And the guider service is running with the camera facade enabled
    And the facade camera is connected
    When the client tries to acquire a sweep lease with the parameters <parameters>
    Then the facade call should fail with ASCOM error 0x401
    And the mock PHD2 should have received no request that changes its state

    Examples:
      | parameters                                                  |
      | not json                                                    |
      | {"ttl_s": 60}                                               |
      | {"token": "aaaa", "ttl_s": 60}                              |
      | {"token": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA", "ttl_s": 60}  |
      | {"token": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "ttl_s": 9}   |
      | {"token": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "ttl_s": 301} |
      | {"token": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "ttl_s": 60.5} |
