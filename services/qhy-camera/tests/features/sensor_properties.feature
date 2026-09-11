@serial
Feature: Sensor geometry and type
  Once connected, a camera reports its sensor geometry (G1): CameraXSize /
  CameraYSize are the SDK's effective area — the region it reads out — not
  the chip, reduced until the full frame at every supported bin is a region
  the sensor delivers whole (R4), while PixelSizeX / PixelSizeY in microns
  come from the cached CCD info. SensorType is RGGB when the colour control
  is present and Monochrome otherwise, with BayerOffsetX / BayerOffsetY
  following the reported Bayer pattern (ST1). MaxADU is
  (2^OutputDataActualBits) - 1, i.e. 65535 for a 16-bit sensor. The simulated
  QHY178M-Simulated camera is a monochrome 16-bit sensor with a 3072x2048
  chip whose first 24 columns are an overscan margin and whose last two rows
  are never read out, so its effective area is 3048x2046 and the size it
  reports is 3048x2044.

  Background:
    Given the qhy-camera service running with the simulation backend
    And camera device 0 is connected

  Scenario: Sensor geometry is the effective area, not the chip
    Then camera device 0 reports the sensor geometry:
      | property    | value |
      | CameraXSize | 3048  |
      | CameraYSize | 2044  |
    And camera device 0 reports a positive PixelSizeX
    And camera device 0 reports a positive PixelSizeY

  Scenario: A monochrome sensor reports SensorType Monochrome
    Then camera device 0 reports SensorType as Monochrome

  Scenario: A 16-bit sensor reports MaxADU 65535
    Then camera device 0 reports MaxADU as 65535

  Scenario: SensorName is reported and non-empty
    Then camera device 0 reports a non-empty SensorName
