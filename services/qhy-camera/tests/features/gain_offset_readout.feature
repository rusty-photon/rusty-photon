@serial
Feature: Gain, offset, and readout modes
  Gain and Offset return the current SDK value, or NOT_IMPLEMENTED when the
  model lacks the control (GO1). Setters validate against the cached
  [min, max] and reject an out-of-range value with INVALID_VALUE (GO2);
  GainMin / GainMax and OffsetMin / OffsetMax reflect the cached SDK limits
  (GO3). ReadoutModes is the SDK's named mode list; setting a mode validates
  the index, updates the cached resolution, and rejects an unknown index
  with INVALID_VALUE (RM1). A mode change writes to the camera and rewrites
  the geometry the next exposure is armed from, so it takes the same claim a
  capture does and is rejected with INVALID_OPERATION while an exposure is in
  flight — ahead of the index check, because the mode count comes off the
  camera and the driver may not ask it during a capture (B4).

  Background:
    Given the qhy-camera service running with the simulation backend
    And camera device 0 is connected

  Scenario: Gain limits are ordered and the current gain is within them
    Then camera device 0 reports GainMin not greater than GainMax
    And camera device 0 reports a Gain within GainMin and GainMax

  Scenario: Setting gain to the maximum is accepted
    When I set Gain to GainMax on camera device 0
    Then camera device 0 reports Gain equal to GainMax

  Scenario: Setting gain above the maximum is rejected
    When I try to set Gain to one above GainMax on camera device 0
    Then the set is rejected with ASCOM INVALID_VALUE

  Scenario: Offset limits are ordered and the current offset is within them
    Then camera device 0 reports OffsetMin not greater than OffsetMax
    And camera device 0 reports an Offset within OffsetMin and OffsetMax

  Scenario: Setting offset below the minimum is rejected
    When I try to set Offset to one below OffsetMin on camera device 0
    Then the set is rejected with ASCOM INVALID_VALUE

  Scenario: The readout modes list is non-empty and the current mode is valid
    Then camera device 0 reports at least one ReadoutMode
    And camera device 0 reports a ReadoutMode index within the modes list

  Scenario: Selecting an out-of-range readout mode is rejected
    When I try to set ReadoutMode to 9999 on camera device 0
    Then the set is rejected with ASCOM INVALID_VALUE

  Scenario: A readout-mode change while an exposure is in flight is rejected
    Given an exposure is in flight on camera device 0
    When I try to set ReadoutMode to 0 on camera device 0
    Then the set is rejected with ASCOM INVALID_OPERATION

  Scenario: An out-of-range readout mode during an exposure is refused as busy, not as out of range
    Given an exposure is in flight on camera device 0
    When I try to set ReadoutMode to 9999 on camera device 0
    Then the set is rejected with ASCOM INVALID_OPERATION
