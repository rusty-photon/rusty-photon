Feature: A power cut while serving
  The mount's controller runs off its 12 V supply. A cut restarts it: its
  encoders come back at (0, 0) with both axes stopped, and each axis'
  init flag (the third nibble of its :f reply) reads clear until the
  driver sends :F again. The driver reads both flags in every handshake,
  before its own :F1 and :F2.

  When it finds the controller restarted while it was serving the mount,
  after an outage of at most 30 s, it writes the mount's last known
  position back with :E1 and :E2 before the transport serves again.
  Tracking stays off until a client turns it on. After a longer outage
  someone may have moved the mount, so nothing is written back: the
  mount state record decides, as at a power-up. An ap_park_0 mount names
  no pose to check against, and loses its position. If the mount was slewing at the cut, the position written back is
  only an estimate and the position is lost: slews, tracking and guide
  pulses are refused, and Park parks in place. A plate-solve sync, or the
  recovery from a named park, clears a lost position. A link that drops
  while the controller keeps its power leaves both flags set, and nothing
  is written back.

  Beside its config file the driver keeps a mount state record, named
  after that file: config.json's is config.mount-state.json. The record
  holds one of three states: at rest with its ticks, moving, or lost. It
  is written when that state changes while a client is connected. Every
  connect reads it, so a lost position stays lost across a reload or a
  restart of the service. After a restart, a power-up connect seeds the
  configured pose only when the record says the mount came to rest
  there. A record that says moving, lost or at rest elsewhere loses the
  position instead.

  Scenario: A cut while the mount is at rest writes its position back
    Given a star-adventurer service configured with unpark_from_ap_position "ap_park_3"
    When I connect the device
    And the mount loses power and comes back
    Then the mount's encoders should read RA -907200 and Dec 725760 ticks
    And Declination should be 90.0 degrees within 0.001
    And Tracking should be false
    When I enable tracking
    Then Tracking should be true

  Scenario: A cut while tracking writes back where tracking left the mount and leaves tracking off
    Given a star-adventurer service configured with unpark_from_ap_position "ap_park_3"
    When I connect the device
    And I enable tracking
    And the mount loses power and comes back
    Then the mount's encoders should be within 20 ticks of where they stood when the power went
    And Tracking should be false
    When I enable tracking
    Then Tracking should be true

  Scenario: A cut mid-slew loses the position, which refuses a slew
    Given a star-adventurer service configured with unpark_from_ap_position "ap_park_3"
    When I connect the device
    And the mount is slewing
    And the mount loses power and comes back
    And I try to slew asynchronously to RA 6.0 hours and Dec 30.0 degrees
    Then the operation should fail with invalid-operation
    And the error message should mention "position is lost"

  Scenario: A lost position refuses tracking
    Given a star-adventurer service configured with unpark_from_ap_position "ap_park_3"
    When I connect the device
    And the mount is slewing
    And the mount loses power and comes back
    And I try to enable tracking
    Then the operation should fail with invalid-operation
    And Tracking should be false

  Scenario: A lost position parks in place
    Given a star-adventurer service configured with unpark_from_ap_position "ap_park_3"
    When I connect the device
    And the mount is slewing
    And the mount loses power and comes back
    And I park the mount
    Then AtPark should eventually be true within 5 seconds
    And the mount should not have received any goto command

  Scenario: A sync clears a lost position
    Given a star-adventurer service configured with unpark_from_ap_position "ap_park_3"
    When I connect the device
    And the mount is slewing
    And the mount loses power and comes back
    And I sync to RA 6.0 hours and Dec 30.0 degrees
    And I slew asynchronously to RA 6.5 hours and Dec 30.0 degrees
    Then Slewing should eventually be false within 10 seconds

  Scenario: A cut longer than a blip seeds the power-up pose when the mount came to rest there
    Given a star-adventurer service configured with unpark_from_ap_position "ap_park_3"
    When I connect the device
    Then the mount state record should say at rest at RA -907200 and Dec 725760 ticks
    When the mount loses power for longer than a blip and comes back
    Then the mount's encoders should read RA -907200 and Dec 725760 ticks
    When I enable tracking
    Then Tracking should be true

  Scenario: A cut longer than a blip loses the position when the mount came to rest away from its power-up pose
    Given a star-adventurer service configured with unpark_from_ap_position "ap_park_3"
    When I connect the device
    And I sync to RA 6.0 hours and Dec 30.0 degrees
    Then the mount state record should say at rest away from RA -907200 and Dec 725760 ticks
    When the mount loses power for longer than a blip and comes back
    Then the mount state record should say "lost"
    And the mount's encoders should read RA 0 and Dec 0 ticks
    When I try to slew asynchronously to RA 6.5 hours and Dec 30.0 degrees
    Then the operation should fail with invalid-operation
    And the error message should mention "position is lost"

  Scenario: A link that drops without a power cut writes nothing back
    Given a star-adventurer service configured with unpark_from_ap_position "ap_park_3"
    When I connect the device
    And the link to the mount drops while the mount keeps its power
    Then the mount should not have received an encoder write since the link came back
    And Declination should be 90.0 degrees within 0.001

  Scenario: The mount state record follows the mount from rest into motion
    Given a star-adventurer service configured with unpark_from_ap_position "ap_park_3"
    When I connect the device
    Then the mount state record should say at rest at RA -907200 and Dec 725760 ticks
    When I enable tracking
    Then the mount state record should say "moving"

  Scenario: A lost position is written to the mount state record
    Given a star-adventurer service configured with unpark_from_ap_position "ap_park_3"
    When I connect the device
    And the mount is slewing
    And the mount loses power and comes back
    Then the mount state record should say "lost"

  Scenario: A lost position outlives a reload of the service
    Given a star-adventurer service configured with unpark_from_ap_position "ap_park_3"
    When I connect the device
    And the mount is slewing
    And the mount loses power and comes back
    Then the mount state record should say "lost"
    When the mount's setup page saves the power-up pose "ap_park_2"
    And the reloaded service serves the power-up pose "ap_park_2"
    And I connect the device
    And I try to slew asynchronously to RA 6.0 hours and Dec 30.0 degrees
    Then the operation should fail with invalid-operation
    And the error message should mention "position is lost"

  Scenario: A restart after the mount came to rest at its power-up pose seeds that pose
    Given a star-adventurer service configured with unpark_from_ap_position "ap_park_3"
    When I connect the device
    Then the mount state record should say at rest at RA -907200 and Dec 725760 ticks
    When the service restarts with the mount freshly powered up
    And I connect the device
    Then the mount should have received an encoder seed on both axes

  Scenario: A restart after the mount was left moving loses the position instead of seeding it
    Given a star-adventurer service configured with unpark_from_ap_position "ap_park_3"
    When I connect the device
    And I enable tracking
    Then the mount state record should say "moving"
    When the service restarts with the mount freshly powered up
    And I connect the device
    Then the mount should not have received an encoder-seed command
    When I try to slew asynchronously to RA 6.0 hours and Dec 30.0 degrees
    Then the operation should fail with invalid-operation
    And the error message should mention "position is lost"

  Scenario: A restart after the mount came to rest away from its power-up pose loses the position
    Given a star-adventurer service configured with unpark_from_ap_position "ap_park_3"
    When I connect the device
    And I sync to RA 6.0 hours and Dec 30.0 degrees
    Then the mount state record should say at rest away from RA -907200 and Dec 725760 ticks
    When the service restarts with the mount freshly powered up
    And I connect the device
    Then the mount should not have received an encoder-seed command
    When I try to slew asynchronously to RA 6.5 hours and Dec 30.0 degrees
    Then the operation should fail with invalid-operation
