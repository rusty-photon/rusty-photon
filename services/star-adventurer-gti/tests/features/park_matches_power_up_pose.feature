Feature: Whether a power cut keeps the park pose
  The night schedule parks the mount at dawn, cuts its power, and powers
  it up again at dusk. The power-up seeds the frame at the pose
  unpark_from_ap_position names only when the mount came to rest there,
  so a cut keeps the mount's pose only when the mount parks at that pose.
  The read-only custom Action ParkMatchesPowerUpPose, which takes no
  parameters, answers whether it does, as a JSON object of four fields.
  matches is true only when both tick pairs are present and equal on
  both axes. power_up_pose is the unpark_from_ap_position name, and
  power_up_ticks its encoder pair: null for ap_park_0, which asserts no
  pose. park_target_ticks is the park target the driver holds when it
  answers: null when an axis has none, as for an unanchored frame
  without a raw override, or a position that is lost or awaiting its
  power-up check.

  SupportedActions lists exactly this Action, in this spelling. Its name
  is matched without regard to case, and every other name answers
  ACTION_NOT_IMPLEMENTED. The Action moves nothing and writes nothing.
  It answers NOT_CONNECTED while the mount is not connected, because the
  park target is resolved on connect.

  The examples run at latitude 0 on the mock's counts per revolution
  (RA 3,628,800, Dec 2,903,040), where ap_park_3 is RA -907200 and
  Dec 725760.

  Scenario: SupportedActions lists exactly ParkMatchesPowerUpPose
    Given a running star-adventurer service
    When the supported actions are queried
    Then the supported actions are exactly "ParkMatchesPowerUpPose"

  Scenario: A mount that parks at its power-up pose matches it
    # preferred_ap_park defaults to ap_park_3, so the park target the
    # connect arms is the pose the power-up seeds.
    Given a star-adventurer service configured with unpark_from_ap_position "ap_park_3"
    When I connect the device
    And I ask whether the park matches the power-up pose
    Then the action answers the JSON object:
      | field             | value                          |
      | matches           | true                           |
      | power_up_pose     | "ap_park_3"                    |
      | power_up_ticks    | {"ra": -907200, "dec": 725760} |
      | park_target_ticks | {"ra": -907200, "dec": 725760} |

  Scenario: A SetPark away from the power-up pose makes the park miss it
    Given a star-adventurer service configured with unpark_from_ap_position "ap_park_3"
    When I connect the device
    And the mount reports both axes stopped at RA 8000 and Dec -3000 ticks
    And I set the park position
    And I ask whether the park matches the power-up pose
    Then the action answers the JSON object:
      | field             | value                          |
      | matches           | false                          |
      | power_up_pose     | "ap_park_3"                    |
      | power_up_ticks    | {"ra": -907200, "dec": 725760} |
      | park_target_ticks | {"ra": 8000, "dec": -3000}     |

  Scenario: A mount that powers up at ap_park_0 asserts no pose for its park to match
    # ap_park_0 leaves the frame unanchored, and with no raw override
    # neither axis has a park target.
    Given a star-adventurer service configured with unpark_from_ap_position "ap_park_0"
    When I connect the device
    And I ask whether the park matches the power-up pose
    Then the action answers the JSON object:
      | field             | value       |
      | matches           | false       |
      | power_up_pose     | "ap_park_0" |
      | power_up_ticks    | null        |
      | park_target_ticks | null        |

  Scenario: The Action answers NOT_CONNECTED while the mount is not connected
    Given a star-adventurer service configured with unpark_from_ap_position "ap_park_3"
    When I try to ask whether the park matches the power-up pose
    Then the operation should fail with not-connected
    When I connect the device
    And I disconnect the device
    And I try to ask whether the park matches the power-up pose
    Then the operation should fail with not-connected

  Scenario Outline: The Action's name is matched without regard to case
    Given a star-adventurer service configured with unpark_from_ap_position "ap_park_3"
    When I connect the device
    And the action "<name>" is called
    Then the answer's "matches" field is true

    Examples:
      | name                   |
      | parkmatchespoweruppose |
      | PARKMATCHESPOWERUPPOSE |

  Scenario Outline: Every other action name answers ACTION_NOT_IMPLEMENTED
    Given a star-adventurer service configured with unpark_from_ap_position "ap_park_3"
    When I connect the device
    And the action "<name>" is called
    Then the call should fail with an action-not-implemented error

    Examples:
      | name                      |
      | NoSuchAction              |
      | ParkMatchesPowerUpPoseNow |
      | ParkMatches               |

  Scenario: The Action moves nothing and writes nothing
    # The writer that follows the mount records the seeded pose once
    # after the connect, and writes again only when the mount moves.
    Given a star-adventurer service configured with unpark_from_ap_position "ap_park_3"
    When I connect the device
    Then the mount state record should say at rest at RA -907200 and Dec 725760 ticks
    When I note the configuration file, the mount state record and the command log
    And I ask whether the park matches the power-up pose
    Then the configuration file and the mount state record should be as noted
    And the mount should have received only position and status reads since the note
    And the mount's encoders should read RA -907200 and Dec 725760 ticks
