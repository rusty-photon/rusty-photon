@serial
Feature: Filter wheel
  qhy-camera registers each discovered CFW as an ASCOM FilterWheel device
  alongside the cameras (detection is the source of truth). Names lists the
  configured filter_names or generated Filter0..FilterN when none are given
  (FW1). Position returns the current slot, or the ASCOM moving sentinel
  while the target slot differs from the actual slot or the wheel reports
  itself moving, as the simulated CFW does in transit (FW7). set_position validates
  that the index is less than the filter count and rejects an out-of-range
  index with INVALID_VALUE (FW2). FocusOffsets returns zero for every filter
  in v0 (FW3). The simulated CFW has 7 positions.

  Background:
    Given the qhy-camera service running with the simulation backend
    And filterwheel device 0 is connected

  Scenario: The filter wheel exposes seven generated filter names
    Then filterwheel device 0 reports 7 filter names
    And filterwheel device 0 reports the generated names Filter0 through Filter6

  Scenario: Moving to a valid slot updates the reported position
    When I set filterwheel device 0 to position 3
    And the filter wheel move on device 0 completes
    Then filterwheel device 0 reports Position as 3

  Scenario: Position reads the moving sentinel while the wheel reports itself moving
    When I set filterwheel device 0 to position 3
    Then filterwheel device 0 reports Position as moving
    And the filter wheel move on device 0 completes
    And filterwheel device 0 reports Position as 3

  Scenario Outline: An out-of-range slot is rejected
    When I try to set filterwheel device 0 to position <slot>
    Then the set is rejected with ASCOM INVALID_VALUE

    Examples:
      | slot |
      | 7    |
      | 99   |

  Scenario: Focus offsets are zero for every filter
    Then filterwheel device 0 reports FocusOffsets of 7 zeros

  Scenario: Custom filter names from config are reported
    Given the qhy-camera service running with the simulation backend and filter names L, R, G, B, Ha, OIII, SII
    And filterwheel device 0 is connected
    Then filterwheel device 0 reports the filter names L, R, G, B, Ha, OIII, SII
