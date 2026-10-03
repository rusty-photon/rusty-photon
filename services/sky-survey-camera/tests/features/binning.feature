@serial
Feature: Symmetric binning
  The camera reports CanAsymmetricBin false, so BinX and BinY are one
  value (E8): a write to either member sets both, and both read it
  back. A client that sets only BinX gets a frame binned equally on
  both axes, and the survey cutout is requested at the sensor size
  divided by that bin on each axis. A bin outside 1 to MaxBinX is
  refused at the setter and leaves the bin as it was (E3). These
  scenarios run on a 640 by 480 sensor with MaxBinX 4.

  Scenario Outline: A write to either bin member sets both
    Given the camera is connected with the survey backend stubbed
    When I set <member> to 3
    Then BinX reads 3
    And BinY reads 3

    Examples:
      | member |
      | BinX   |
      | BinY   |

  Scenario: Setting BinX alone yields a frame binned on both axes
    Given the camera is connected with the survey backend stubbed
    And the survey backend returns a healthy FITS cutout
    And BinX is set to 2
    When I StartExposure with NumX 320 NumY 240
    Then the resulting image has dimensions 320 by 240
    And the survey cutout was requested at 320 by 240 pixels

  Scenario: A refused bin leaves both members as they were
    Given the camera is connected with the survey backend stubbed
    And BinX is set to 2
    When I try to set BinY to 5
    Then the write is rejected with ASCOM INVALID_VALUE
    And BinX reads 2
    And BinY reads 2
