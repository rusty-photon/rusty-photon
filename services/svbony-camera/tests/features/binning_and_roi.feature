@serial
Feature: Binning and region-of-interest
  Binning is symmetric only: CanAsymmetricBin is false (B2) and MaxBinX /
  MaxBinY come from the SDK's SupportedBins. Setting a bin validates against
  those modes and rejects an unsupported value with INVALID_VALUE (B1). The
  cached ROI is held in unbinned sensor pixels, so a bin change changes only
  the divisor the binned members are read through: a sub-frame walked away
  from its bin and back is the frame the client set, not a truncated
  remainder of it (B3). The ROI setters
  (StartX / StartY / NumX / NumY) accept any u32 (R1) -- geometry is not
  validated at the setter but at StartExposure, which rejects a zero or
  out-of-bounds sub-frame with INVALID_VALUE (R2) and a sub-frame that
  violates SVBSetROIFormat's alignment rule -- width not a multiple of 8 or
  height not a multiple of 2, byte-for-byte the same rule zwo-camera enforces
  for ASI -- with INVALID_VALUE (R3). The simulated SV605CC-Simulated is
  raw 3008x3008 with supported bins 1-4; CameraXSize/CameraYSize get the
  same "reported aligned down so every binned full frame is a valid ROI"
  treatment as zwo-camera's R4 (2976x3000): clients -- ConformU among
  them -- take a full frame at every bin via NumX = CameraXSize / bin,
  which the raw extent cannot satisfy at every bin (3008/3 = 1002 is not
  a multiple of 8), so a full-frame StartExposure succeeds at every
  supported bin (R4).

  Background:
    Given the svbony-camera service running with the simulation backend
    And camera device 0 is connected

  Scenario: Asymmetric binning is not supported
    Then camera device 0 reports CanAsymmetricBin as false

  Scenario: A supported binning mode is accepted
    When I set BinX 2 and BinY 2 on camera device 0
    Then camera device 0 reports BinX as 2 and BinY as 2

  Scenario Outline: An unsupported binning value is rejected at the setter
    When I try to set BinX <bin_x> and BinY <bin_y> on camera device 0
    Then the set is rejected with ASCOM INVALID_VALUE

    Examples:
      | bin_x | bin_y |
      | 0     | 0     |
      | 99    | 99    |

  Scenario: A client sub-frame comes back from a bin walk as the frame it was
    When I set StartX 200 NumX 100 StartY 200 NumY 100 on camera device 0
    And I set BinX 3 and BinY 3 on camera device 0
    And I set BinX 4 and BinY 4 on camera device 0
    And I set BinX 1 and BinY 1 on camera device 0
    Then camera device 0 reports StartX 200 NumX 100 StartY 200 NumY 100

  Scenario: A client sub-frame at a bin is the unbinned region divided by that bin
    When I set StartX 200 NumX 100 StartY 200 NumY 100 on camera device 0
    And I set BinX 3 and BinY 3 on camera device 0
    Then camera device 0 reports StartX 66 NumX 33 StartY 66 NumY 33

  Scenario: A sub-pixel extent is one binned pixel, not a zero the client never set
    When I set StartX 0 NumX 1 StartY 0 NumY 1 on camera device 0
    And I set BinX 4 and BinY 4 on camera device 0
    Then camera device 0 reports StartX 0 NumX 1 StartY 0 NumY 1

  Scenario: The ROI setters accept any value
    When I set StartX 5000 NumX 5000 StartY 5000 NumY 5000 on camera device 0
    Then camera device 0 accepts the ROI without error

  Scenario Outline: An out-of-bounds sub-frame is rejected at StartExposure
    When I StartExposure on camera device 0 with BinX 1 BinY 1 NumX <num_x> NumY <num_y> StartX <start_x> StartY <start_y> Duration 0.01 Light true
    Then the exposure is rejected with ASCOM INVALID_VALUE

    Examples:
      | num_x | num_y | start_x | start_y |
      | 0     | 64    | 0       | 0       |
      | 64    | 0     | 0       | 0       |
      | 4000  | 64    | 0       | 0       |
      | 64    | 4000  | 0       | 0       |
      | 64    | 64    | 3008    | 0       |
      | 64    | 64    | 0       | 3008    |

  Scenario Outline: A full frame is achievable at every supported bin
    When I StartExposure on camera device 0 with BinX <bin> BinY <bin> NumX <num_x> NumY <num_y> StartX 0 StartY 0 Duration 0.01 Light true
    And the exposure on camera device 0 completes
    Then camera device 0 reports ImageReady as true

    Examples:
      | bin | num_x | num_y |
      | 1   | 2976  | 3000  |
      | 2   | 1488  | 1500  |
      | 3   | 992   | 1000  |
      | 4   | 744   | 750   |

  Scenario Outline: A misaligned sub-frame is rejected at StartExposure
    When I StartExposure on camera device 0 with BinX 1 BinY 1 NumX <num_x> NumY <num_y> StartX 0 StartY 0 Duration 0.01 Light true
    Then the exposure is rejected with ASCOM INVALID_VALUE

    Examples:
      | num_x | num_y |
      | 100   | 64    |
      | 64    | 47    |
