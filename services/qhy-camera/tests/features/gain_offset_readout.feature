@serial
Feature: Gain, offset, and readout modes
  Gain and Offset report the value the next exposure arms, not a live read of
  the camera: a connect seeds each one by reading the value the camera holds,
  writing neither (GO1). A model that lacks the control answers
  NOT_IMPLEMENTED. Setters validate against the cached [min, max], reject an
  out-of-range value with INVALID_VALUE, and otherwise store the value without
  sending the camera anything. StartExposure sends the cached gain and then the
  cached offset on every exposure, under the claim the exposure holds, so a set
  needs no device of its own and is never refused as busy: one made while an
  exposure is in flight is taken for the next exposure (GO2). GainMin /
  GainMax and OffsetMin / OffsetMax reflect the cached SDK limits, which on the
  simulated camera are 0 to 100 for gain and 0 to 255 for offset (GO3). The
  simulated camera's frames do not depend on gain or offset, so these
  scenarios pin which values are accepted and what is reported; what an
  exposure sends the camera, and in what order, is pinned by the driver's
  unit tests.

  ReadoutModes is the SDK's named mode list, read at connect; ReadoutMode is
  the mode the camera was last switched into, and every connect leaves it at
  mode 0 (RM1, C1). An index outside the list is rejected with INVALID_VALUE
  whatever else the device is doing: the list is cached, so the answer needs
  no camera (B4). A valid mode is applied at the setter. The camera is
  re-initialized in that mode and every mode-dependent value is read again,
  so a change of mode resets BinX and BinY to 1 and the sub-frame to the new
  full frame (RM1). Selecting the mode already in force changes nothing, and
  keeps the binning and the sub-frame the client set. A mode change owns the
  camera while it runs, so selecting a mode is rejected with
  INVALID_OPERATION while an exposure is in flight (B4). The simulated camera
  has a single readout mode, so a switch between two modes is exercised by
  the driver's unit tests rather than here.

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

  Scenario: Setting offset to the maximum is accepted
    When I set Offset to OffsetMax on camera device 0
    Then camera device 0 reports Offset equal to OffsetMax

  Scenario: Setting offset below the minimum is rejected
    When I try to set Offset to one below OffsetMin on camera device 0
    Then the set is rejected with ASCOM INVALID_VALUE

  Scenario: A gain set while an exposure is in flight is accepted and still reported after the next exposure
    Given an exposure is in flight on camera device 0
    When I set Gain to 42 on camera device 0
    Then camera device 0 reports Gain as 42
    When I abort the exposure on camera device 0
    And I StartExposure on camera device 0 with the current sub-frame and Duration 0.01 Light true
    And the exposure on camera device 0 completes
    Then camera device 0 reports Gain as 42

  Scenario: An offset set while an exposure is in flight is accepted and still reported after the next exposure
    Given an exposure is in flight on camera device 0
    When I set Offset to 30 on camera device 0
    Then camera device 0 reports Offset as 30
    When I abort the exposure on camera device 0
    And I StartExposure on camera device 0 with the current sub-frame and Duration 0.01 Light true
    And the exposure on camera device 0 completes
    Then camera device 0 reports Offset as 30

  Scenario: A connected camera is in readout mode 0 of a non-empty list
    Then camera device 0 reports at least one ReadoutMode
    And camera device 0 reports ReadoutMode as 0

  Scenario: Selecting an out-of-range readout mode is rejected
    When I try to set ReadoutMode to 9999 on camera device 0
    Then the set is rejected with ASCOM INVALID_VALUE

  Scenario: An out-of-range readout mode is rejected as out of range even during an exposure
    Given an exposure is in flight on camera device 0
    When I try to set ReadoutMode to 9999 on camera device 0
    Then the set is rejected with ASCOM INVALID_VALUE

  Scenario: Selecting a readout mode while an exposure is in flight is rejected as busy
    Given an exposure is in flight on camera device 0
    When I try to set ReadoutMode to 0 on camera device 0
    Then the set is rejected with ASCOM INVALID_OPERATION

  Scenario: Selecting the readout mode already in force keeps the binning and the sub-frame
    When I set BinX 2 and BinY 2 on camera device 0
    And I set StartX 10 NumX 100 StartY 20 NumY 50 on camera device 0
    And I set ReadoutMode to 0 on camera device 0
    Then camera device 0 reports ReadoutMode as 0
    And camera device 0 reports BinX as 2 and BinY as 2
    And camera device 0 reports StartX 10 NumX 100 StartY 20 NumY 50
