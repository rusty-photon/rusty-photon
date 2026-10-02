@serial
Feature: Gain, offset, and readout modes
  Gain and Offset report the value the next exposure arms, cached by the
  driver, not read from the camera: a connect seeds each by reading the
  camera's own, and writes nothing (GO1). A model lacking the control answers
  NOT_IMPLEMENTED. Setters validate against the cached [min, max], reject an
  out-of-range value with INVALID_VALUE, and store the value without sending
  the camera anything, so a set is never refused as busy (GO2); GainMin /
  GainMax and OffsetMin / OffsetMax reflect the cached SDK limits (GO3).
  Every StartExposure sends the gain, then the offset, to the camera ahead of
  the exposure itself, whether or not they changed. A gain or offset set while
  an exposure is in flight is taken for the next exposure; the one in flight
  keeps the values it was armed with. The simulated camera is 0.25 e-/ADU at
  gain 0 and 0.025 at gain 200, and ElectronsPerADU describes the gain the
  camera holds (ST2), which is how a scenario sees which gain a frame armed.

  ReadoutModes is the camera's download-format list. The driver intersects
  ASI_CAMERA_INFO.SupportedVideoFormat with the formats it can deliver --
  Raw16 first, then Raw8 -- and publishes the survivors, so the simulated
  camera (which advertises both) reports "Raw16, Raw8". Index 0, the highest
  precision the camera offers, is the default. The selection is passed to
  ASISetROIFormat for each frame, sizes the download buffer, picks the
  ImageArray unpack, and sets MaxADU (RM1/RM2). Setting an unknown index is
  rejected with INVALID_VALUE; changing the mode while an exposure is in
  flight is rejected with INVALID_OPERATION, so a delivered frame and the
  MaxADU describing it can never disagree. RGB24 and Y8 are never offered:
  they are debayered or luminance formats that would change the device's
  SensorType and BayerOffset contract rather than just its buffer arithmetic,
  and a camera advertising no raw format at all fails to connect rather than
  silently downloading one of them (RM3/RM4).

  Background:
    Given the zwo-camera service running with the simulation backend
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

  Scenario: A gain set while an exposure is in flight is taken for the next exposure
    When I set Gain to 0 on camera device 0
    And I StartExposure on camera device 0 with BinX 1 BinY 1 NumX 64 NumY 48 StartX 0 StartY 0 Duration 0.01 Light true
    And the exposure on camera device 0 completes
    And an exposure is in flight on camera device 0
    And I set Gain to 200 on camera device 0
    Then camera device 0 reports Gain as 200
    And camera device 0 keeps reporting ElectronsPerADU as 0.25 while the exposure is in flight
    When I stop the exposure on camera device 0
    And I StartExposure on camera device 0 with BinX 1 BinY 1 NumX 64 NumY 48 StartX 0 StartY 0 Duration 0.01 Light true
    And the exposure on camera device 0 completes
    Then camera device 0 reports ElectronsPerADU as 0.025

  Scenario: An offset set while an exposure is in flight is accepted, not refused as busy
    The simulated frame does not depend on the offset, so what this shows is a
    set taken while a frame is in flight rather than refused as busy; which
    frame arms it is pinned by the unit tests against the mock SDK seam.

    Given an exposure is in flight on camera device 0
    When I set Offset to 80 on camera device 0
    Then camera device 0 reports Offset as 80
    When I stop the exposure on camera device 0
    Then camera device 0 reports Offset as 80

  Scenario: The readout modes are the camera's supported download formats
    Then camera device 0 reports ReadoutModes as "Raw16, Raw8"
    And camera device 0 reports ReadoutMode as 0
    And camera device 0 reports MaxADU as 65535

  Scenario: Selecting the 8-bit readout mode drops MaxADU to 255
    When I set ReadoutMode to 1 on camera device 0
    Then camera device 0 reports ReadoutMode as 1
    And camera device 0 reports MaxADU as 255

  Scenario: A frame downloaded in the 8-bit readout mode has the requested sub-frame size
    When I set ReadoutMode to 1 on camera device 0
    And I StartExposure on camera device 0 with BinX 1 BinY 1 NumX 64 NumY 48 StartX 0 StartY 0 Duration 0.01 Light true
    And the exposure on camera device 0 completes
    Then camera device 0 reports ImageReady as true
    And camera device 0 returns an ImageArray of 64 by 48

  Scenario: Reconnecting restores the default readout mode
    When I set ReadoutMode to 1 on camera device 0
    And I disconnect camera device 0
    And I connect camera device 0
    Then camera device 0 reports ReadoutMode as 0

  Scenario: Changing the readout mode during an exposure is rejected
    Given an exposure is in flight on camera device 0
    When I try to set ReadoutMode to 1 on camera device 0
    Then the set is rejected with ASCOM INVALID_OPERATION

  Scenario: Selecting an out-of-range readout mode is rejected
    When I try to set ReadoutMode to 9999 on camera device 0
    Then the set is rejected with ASCOM INVALID_VALUE
