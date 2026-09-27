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
  out-of-bounds sub-frame with INVALID_VALUE (R2) and a sub-frame violating
  the ASI alignment rules -- width not a multiple of 8 or height not a
  multiple of 2 -- with INVALID_VALUE (R3). The simulated ASI2600MM-Pro is
  6248x4176, but CameraXSize is reported as 6240 so the full frame at any
  supported bin (CameraXSize / bin) stays a valid ASI ROI (R4).

  Background:
    Given the zwo-camera service running with the simulation backend
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
    When I set StartX 9000 NumX 9000 StartY 9000 NumY 9000 on camera device 0
    Then camera device 0 accepts the ROI without error

  Scenario Outline: An out-of-bounds sub-frame is rejected at StartExposure
    When I StartExposure on camera device 0 with BinX 1 BinY 1 NumX <num_x> NumY <num_y> StartX <start_x> StartY <start_y> Duration 0.01 Light true
    Then the exposure is rejected with ASCOM INVALID_VALUE

    Examples:
      | num_x | num_y | start_x | start_y |
      | 0     | 64    | 0       | 0       |
      | 64    | 0     | 0       | 0       |
      | 8000  | 64    | 0       | 0       |
      | 64    | 6000  | 0       | 0       |
      | 64    | 64    | 6248    | 0       |
      | 64    | 64    | 0       | 4176    |

  Scenario Outline: A misaligned sub-frame is rejected at StartExposure
    When I StartExposure on camera device 0 with BinX 1 BinY 1 NumX <num_x> NumY <num_y> StartX 0 StartY 0 Duration 0.01 Light true
    Then the exposure is rejected with ASCOM INVALID_VALUE

    Examples:
      | num_x | num_y |
      | 100   | 64    |
      | 64    | 47    |

  # R4: the full frame at every bin (NumX = CameraXSize/bin = 6240/bin) is a
  # valid ASI ROI and must expose -- this is what conformance tools exercise.
  Scenario Outline: A binned full frame is accepted at every bin
    When I StartExposure on camera device 0 with BinX <bin> BinY <bin> NumX <num_x> NumY <num_y> StartX 0 StartY 0 Duration 0.01 Light true
    Then the exposure on camera device 0 completes

    Examples:
      | bin | num_x | num_y |
      | 2   | 3120  | 2088  |
      | 3   | 2080  | 1392  |
      | 4   | 1560  | 1044  |
