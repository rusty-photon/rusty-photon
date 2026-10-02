@serial
Feature: Gain, offset, and readout modes
  Gain maps to SVB_GAIN and Offset maps to SVB_BLACK_LEVEL (SVBony's ASCOM
  Offset-equivalent control). GainMin / GainMax and OffsetMin / OffsetMax
  are the limits SVBGetControlCaps advertises, read once at connect (GO3);
  a control the model lacks is NOT_IMPLEMENTED from all of its members.

  Gain and Offset report the value the next exposure arms, from the
  driver's cache, never from a read of the camera (GO1). A connect seeds
  that value by reading what the camera holds once its parameters are
  restored to the device defaults -- on the simulated SV605CC a gain of 100
  and an offset of 0 -- and nothing is written at connect. Setting either
  validates against the cached limits and rejects an out-of-range value
  with INVALID_VALUE, then stores it; nothing reaches the camera at the
  setter (GO2). Every StartExposure sends the gain, then the offset, to the
  camera after its own exposure write, on every exposure whether or not
  they changed -- the SDK refuses a gain while its auto-exposure state is
  on, and the exposure write is what clears it (GO5). A set therefore
  needs no device and is never refused as busy: one made while an exposure
  is in flight is accepted, reported, and taken for the next exposure,
  while the exposure in flight carries on with the values it was armed
  with. A reconnect seeds the values from the camera again, so a value set
  in one session is not armed in the next (GO4). The simulated frames do
  not depend on gain or offset, so which frame a value lands on cannot be
  seen from here; the driver's unit tests pin it.

  ReadoutModes is the camera's download-format list. At connect the driver
  intersects SVB_CAMERA_PROPERTY.SupportedVideoFormat with the formats it
  can deliver -- Raw16 first, then Raw8 -- and publishes the survivors, so
  the simulated SV605CC (which advertises both) reports "Raw16, Raw8".
  Index 0, the highest precision the camera offers, is the default and is
  restored on every connect. The selection is the driver's whole format
  story: it is pushed to SVBSetOutputImageType before each soft trigger, it
  sizes the download buffer, it picks the ImageArray unpack, and MaxADU
  reports its full scale -- 65535 for Raw16, 255 for Raw8 (RM1/RM2).
  Setting an unknown index is rejected with INVALID_VALUE. The readout
  mode is the one frame setting still refused while an exposure is in
  flight -- with INVALID_OPERATION, so a delivered frame and the MaxADU
  describing it can never disagree.
  RGB24 / RGB32 and Y8-Y16 are never offered: they are debayered or
  luminance formats that would change the device's SensorType and
  BayerOffset contract rather than just its buffer arithmetic, and a
  camera advertising no raw format at all fails to connect rather than
  silently downloading one of them (RM3/RM4).

  Background:
    Given the svbony-camera service running with the simulation backend
    And camera device 0 is connected

  Scenario: Gain limits are ordered and the current gain is within them
    Then camera device 0 reports GainMin not greater than GainMax
    And camera device 0 reports a Gain within GainMin and GainMax

  Scenario: A connect reports the gain and offset the camera holds
    Then camera device 0 reports Gain as 100 and Offset as 0

  Scenario: Setting gain to the maximum is accepted
    When I set Gain to GainMax on camera device 0
    Then camera device 0 reports Gain equal to GainMax

  Scenario: An exposure taken at the maximum gain and offset completes and still reports them
    When I set Gain to GainMax on camera device 0
    And I set Offset to OffsetMax on camera device 0
    And I StartExposure on camera device 0 with BinX 1 BinY 1 NumX 64 NumY 48 StartX 0 StartY 0 Duration 0.01 Light true
    And the exposure on camera device 0 completes
    Then camera device 0 reports ImageReady as true
    And camera device 0 reports Gain equal to GainMax
    And camera device 0 reports Offset equal to OffsetMax

  Scenario: A gain set during an exposure is accepted and reported while the exposure continues
    Given an exposure is in flight on camera device 0
    When I set Gain to GainMax on camera device 0
    Then camera device 0 reports Gain equal to GainMax
    And camera device 0 reports CameraState as Exposing

  Scenario: An offset set during an exposure is accepted and reported while the exposure continues
    Given an exposure is in flight on camera device 0
    When I set Offset to OffsetMax on camera device 0
    Then camera device 0 reports Offset equal to OffsetMax
    And camera device 0 reports CameraState as Exposing

  Scenario: Reconnecting reports the gain and offset the camera holds again
    When I set Gain to GainMax on camera device 0
    And I set Offset to OffsetMax on camera device 0
    And I disconnect camera device 0
    And I connect camera device 0
    Then camera device 0 reports Gain as 100 and Offset as 0

  Scenario: Setting gain above the maximum is rejected
    When I try to set Gain to one above GainMax on camera device 0
    Then the set is rejected with ASCOM INVALID_VALUE

  Scenario: Offset limits are ordered and the current offset is within them
    Then camera device 0 reports OffsetMin not greater than OffsetMax
    And camera device 0 reports an Offset within OffsetMin and OffsetMax

  Scenario: Setting offset below the minimum is rejected
    When I try to set Offset to one below OffsetMin on camera device 0
    Then the set is rejected with ASCOM INVALID_VALUE

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
