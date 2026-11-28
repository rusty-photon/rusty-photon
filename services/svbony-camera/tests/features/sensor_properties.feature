@serial
Feature: Sensor geometry, type, and signal
  Once connected, a camera reports its sensor geometry from the cached
  SVB_CAMERA_PROPERTY (G1): CameraXSize / CameraYSize in pixels and
  PixelSizeX / PixelSizeY in microns (SVBGetSensorPixelSize), with
  PixelSizeX equal to PixelSizeY because SVBony exposes a single pixel
  size. SVBGetSensorPixelSize writes a C float, and PixelSizeX reports the
  decimal that float stands for: 3.76, which the float holds only as
  3.759999990463257. CameraXSize / CameraYSize are the raw sensor extent aligned DOWN
  so the full frame divided by every supported bin satisfies the SDK's
  width%8 / height%2 ROI rule (the simulated SV605CC's raw 3008x3008 with
  bins 1-4 reports 2976x3000) -- real-hardware ConformU takes a full frame
  at every bin via NumX = CameraXSize / bin, which the raw extent cannot
  satisfy at bin 3; same mechanism as zwo-camera's R4. SensorType is RGGB when the camera is a colour (OSC) model and
  Monochrome otherwise, derived at runtime from IsColorCam / BayerPattern --
  never hardcoded to the SV605CC's own pattern -- with BayerOffsetX /
  BayerOffsetY following the reported pattern (ST1). Unlike zwo-camera,
  SVB_CAMERA_PROPERTY carries no native electrons-per-ADU field, so
  ElectronsPerADU is a NOT_IMPLEMENTED placeholder (ST2) rather than a
  native value -- confirmed against real
  hardware: no gain-dependent conversion factor is exposed anywhere in the
  SDK. MaxADU is the selected readout format's full scale, not
  (2^MaxBitDepth) - 1 = 16383 (ST3): in the default Raw16 mode it is
  65535, because the SDK rescales sub-16-bit ADC data to the full 16-bit
  range -- hardware-verified on the 14-bit SV605CC, whose saturated Raw16
  pixels read 65535. MaxADU in the Raw8 mode is covered by the readout-mode
  feature, which owns format selection. The simulated
  SV605CC-Simulated camera is a 3008x3008 colour (Bayer RG) 14-bit sensor
  with a 3.76 micron pixel size.

  Background:
    Given the svbony-camera service running with the simulation backend
    And camera device 0 is connected

  Scenario: Sensor geometry reflects the simulated camera info
    Then camera device 0 reports the sensor geometry:
      | property    | value |
      | CameraXSize | 2976  |
      | CameraYSize | 3000  |
    And camera device 0 reports PixelSizeX as 3.76
    And camera device 0 reports PixelSizeX equal to PixelSizeY

  Scenario: A colour sensor reports SensorType RGGB
    Then camera device 0 reports SensorType as RGGB

  Scenario: A 14-bit sensor reports the Raw16 full-scale MaxADU in the default mode
    Then camera device 0 reports MaxADU as 65535

  Scenario: SensorName is reported and non-empty
    Then camera device 0 reports a non-empty SensorName

  Scenario: ElectronsPerADU has no native SDK field and is not implemented
    Then camera device 0 reports ElectronsPerADU as NOT_IMPLEMENTED
