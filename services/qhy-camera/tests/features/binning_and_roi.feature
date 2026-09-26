@serial
Feature: Binning and region-of-interest
  Binning is symmetric only: CanAsymmetricBin is false (B2) and MaxBinX /
  MaxBinY come from the SDK's valid binning modes. Setting a bin validates
  against those modes and rejects an unsupported value with INVALID_VALUE
  (B1). The cached ROI is held in unbinned sensor pixels, so a bin change
  changes only the divisor the binned members are read through: a sub-frame
  walked away from its bin and back is the frame the client set, not a
  truncated remainder of it (B3). The ROI setters (StartX / StartY / NumX /
  NumY) accept any u32 (R1) — geometry is
  not validated at the setter but at StartExposure, which rejects a zero or
  out-of-bounds sub-frame with INVALID_VALUE (R2), and likewise an odd NumX
  or NumY, because the sensor fills an odd extent one row or column short
  (R4). The bound is the sensor size CameraXSize / CameraYSize advertise
  (G1), and StartX / StartY count from its top-left pixel; the driver adds
  the area's origin when it arms the SDK. A fresh connection exposes the
  whole sensor: StartX / StartY 0 and NumX / NumY equal to CameraXSize /
  CameraYSize. The simulated QHY178M has a 3072x2048 chip whose first 24
  columns are an overscan margin and whose last two rows are never read out,
  so its effective area is 3048x2046 starting at column 24 and the size it
  reports is 3048x2044 — four rows short of the chip, so that the full frame
  at bin 2 is 1022 rows rather than an odd 1023.

  Background:
    Given the qhy-camera service running with the simulation backend
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

  Scenario: A fresh connection exposes the whole sensor as the default sub-frame
    Then camera device 0 reports StartX 0 NumX 3048 StartY 0 NumY 2044

  Scenario: The default sub-frame at a bin is the reported sensor divided by that bin
    When I set BinX 2 and BinY 2 on camera device 0
    Then camera device 0 reports StartX 0 NumX 1524 StartY 0 NumY 1022

  Scenario: A client sub-frame comes back from a bin round trip as the frame it was
    When I set StartX 201 NumX 101 StartY 201 NumY 101 on camera device 0
    And I set BinX 2 and BinY 2 on camera device 0
    And I set BinX 1 and BinY 1 on camera device 0
    Then camera device 0 reports StartX 201 NumX 101 StartY 201 NumY 101

  Scenario: A client sub-frame at a bin is the unbinned region divided by that bin
    When I set StartX 201 NumX 101 StartY 201 NumY 101 on camera device 0
    And I set BinX 2 and BinY 2 on camera device 0
    Then camera device 0 reports StartX 100 NumX 50 StartY 100 NumY 50

  Scenario: A sub-frame set at a bin is that many unbinned pixels once the bin is 1
    When I set BinX 2 and BinY 2 on camera device 0
    And I set StartX 100 NumX 50 StartY 100 NumY 50 on camera device 0
    And I set BinX 1 and BinY 1 on camera device 0
    Then camera device 0 reports StartX 200 NumX 100 StartY 200 NumY 100

  Scenario: A sub-pixel extent is one binned pixel, not a zero the client never set
    When I set StartX 0 NumX 1 StartY 0 NumY 1 on camera device 0
    And I set BinX 2 and BinY 2 on camera device 0
    Then camera device 0 reports StartX 0 NumX 1 StartY 0 NumY 1

  Scenario: A client-set zero extent stays zero across a bin change
    When I set StartX 0 NumX 0 StartY 0 NumY 0 on camera device 0
    And I set BinX 2 and BinY 2 on camera device 0
    Then camera device 0 reports StartX 0 NumX 0 StartY 0 NumY 0

  Scenario: The default sub-frame at a bin arrives with every row and column populated
    When I set BinX 2 and BinY 2 on camera device 0
    And I StartExposure on camera device 0 with the current sub-frame and Duration 0.01 Light true
    And the exposure on camera device 0 completes
    Then camera device 0 returns an ImageArray of 1524 by 1022
    And every row and column of the ImageArray from camera device 0 carries data

  Scenario Outline: An odd sub-frame extent is rejected at StartExposure
    When I StartExposure on camera device 0 with BinX 1 BinY 1 NumX <num_x> NumY <num_y> StartX 0 StartY 0 Duration 0.01 Light true
    Then the exposure is rejected with ASCOM INVALID_VALUE

    Examples:
      | num_x | num_y |
      | 101   | 100   |
      | 100   | 101   |

  Scenario: The ROI setters accept any value
    When I set StartX 5000 NumX 5000 StartY 5000 NumY 5000 on camera device 0
    Then camera device 0 accepts the ROI without error

  Scenario: A sub-frame reaching into the chip's overscan margin is rejected at StartExposure
    When I StartExposure on camera device 0 with BinX 1 BinY 1 NumX 3072 NumY 100 StartX 0 StartY 0 Duration 0.01 Light true
    Then the exposure is rejected with ASCOM INVALID_VALUE

  Scenario Outline: An out-of-bounds sub-frame is rejected at StartExposure
    When I StartExposure on camera device 0 with BinX 1 BinY 1 NumX <num_x> NumY <num_y> StartX <start_x> StartY <start_y> Duration 0.01 Light true
    Then the exposure is rejected with ASCOM INVALID_VALUE

    Examples:
      | num_x | num_y | start_x | start_y |
      | 0     | 100   | 0       | 0       |
      | 100   | 0     | 0       | 0       |
      | 4000  | 100   | 0       | 0       |
      | 100   | 3000  | 0       | 0       |
      | 100   | 100   | 3000    | 0       |
      | 100   | 100   | 0       | 2000    |
