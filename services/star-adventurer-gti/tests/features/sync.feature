Feature: Sync to coordinates
  SyncToCoordinates writes the supplied RA / Dec to the mount via :E on
  each axis (which sets the encoder position) and updates the in-memory
  sync offset so subsequent RA / Dec reads reflect the new alignment.
  SyncToTarget syncs to the most-recent TargetRightAscension / Declination.

  A sync reads both axes from the mount before it writes, and refuses
  with INVALID_OPERATION while either axis is still running a goto,
  whether or not a slew owns it. Two ordinary sequences leave such a
  goto running: an AbortSlew, whose stops coast the axes on, or a client
  that reconnects mid-slew, which leaves the goto to run to its target.
  The position a sync would write there is not where the axis stops.

  Scenario: SyncToCoordinates fails while disconnected
    Given a running star-adventurer service
    When I try to sync to RA 6.0 hours and Dec 30.0 degrees
    Then the operation should fail with not-connected

  Scenario: SyncToCoordinates rejects RA out of range
    Given a running star-adventurer service
    When I connect the device
    And I try to sync to RA 24.5 hours and Dec 0.0 degrees
    Then the operation should fail with invalid-value

  Scenario: SyncToCoordinates rejects Dec out of range
    Given a running star-adventurer service
    When I connect the device
    And I try to sync to RA 0.0 hours and Dec 91.0 degrees
    Then the operation should fail with invalid-value

  Scenario: SyncToCoordinates fails while parked
    Given a running star-adventurer service
    And the device is parked
    When I try to sync to RA 6.0 hours and Dec 30.0 degrees
    Then the operation should fail with invalid-while-parked

  Scenario: SyncToCoordinates is refused while an axis is still running a goto
    # The seeded gotos stand in for one no slew owns, such as an aborted
    # slew's coast. Slewing reads true first, so the goto is on the wire
    # before the sync is tried.
    Given a running star-adventurer service
    And the mount is slewing
    When I connect the device
    Then Slewing should be true
    When I try to sync to RA 6.0 hours and Dec 30.0 degrees
    Then the operation should fail with invalid-operation
    And the mount should not have received an encoder-seed command

  Scenario: SyncToCoordinates succeeds once the goto has stopped
    Given a running star-adventurer service
    And the mount is slewing
    When I connect the device
    Then Slewing should be true
    When the mount reports both axes stopped in goto mode
    Then Slewing should eventually be false within 5 seconds
    When I sync to RA 6.0 hours and Dec 30.0 degrees
    Then RightAscension should be 6.0 hours within 0.001
    And Declination should be 30.0 degrees within 0.001

  Scenario: SyncToCoordinates issues :E on both axes
    Given a running star-adventurer service
    When I connect the device
    And I sync to RA 6.0 hours and Dec 30.0 degrees
    Then the mount should have received commands matching:
      | pattern |
      | :E1.*   |
      | :E2.*   |

  Scenario: After sync, RightAscension reads the synced value
    Given a running star-adventurer service
    When I connect the device
    And I sync to RA 6.0 hours and Dec 30.0 degrees
    Then RightAscension should be 6.0 hours within 0.001
    And Declination should be 30.0 degrees within 0.001

  Scenario: SyncToTarget without a stored target fails
    Given a running star-adventurer service
    When I connect the device
    And I try to sync to the stored target
    Then the operation should fail with invalid-operation

  Scenario: SyncToTarget uses the last set target
    Given a running star-adventurer service
    When I connect the device
    And I set TargetRightAscension to 8.0 hours
    And I set TargetDeclination to 45.0 degrees
    And I sync to the stored target
    Then RightAscension should be 8.0 hours within 0.001
    And Declination should be 45.0 degrees within 0.001
