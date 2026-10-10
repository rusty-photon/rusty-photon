@serial
Feature: FITS header on captured frames
  Every frame `capture` writes carries the standard FITS keywords that
  third-party tools read - ASTAP, PixInsight, Siril, astrometry.net - so a
  frame stays self-describing after it leaves rp without its sidecar
  (rp.md, Persistence: FITS header). The header is a portable copy of the
  exposure document: each keyword mirrors a document field or the rig
  configuration, and the sidecar stays the authority.

  A keyword whose source is unknown is left out, never written as a
  placeholder, and no header problem fails a capture. The simulator camera
  implements neither Gain nor Offset and these rigs configure no cooling,
  so their frames carry no GAIN, OFFSET or SET-TEMP. XPIXSZ/YPIXSZ are the
  pixel size after binning. TELESCOP is the train's configured telescope,
  and APTDIA/FOCRATIO its aperture and focal ratio. RA/DEC are where the
  mount pointed; OBJCTRA/OBJCTDEC are the target's catalog coordinates.
  DATE-OBS is the exposure start, the document's `exposure_started_at`.

  In the tables, `{version}` stands for rp's own version and
  `{document_id}` for the capture's document id.

  Scenario: A Light frame carries the acquisition, instrument, optics, pointing and site keywords
    Given rp is running with a fully equipped capture rig on the simulator
    And the MCP client has added a target named "M31" at ra_hours 0.7123 dec_degrees 41.2689
    When the MCP client calls "capture" with camera "main-cam" for 100 ms, the added target, and frame_type "Light"
    Then the tool call should succeed
    And the captured FITS header should carry these keywords:
      | keyword  | type    | value                     |
      | EXPTIME  | real    | 0.1                       |
      | IMAGETYP | string  | Light Frame               |
      | OBJECT   | string  | M31                       |
      | OBJCTRA  | string  | 00 42 44.28               |
      | OBJCTDEC | string  | +41 16 08.0               |
      | INSTRUME | string  | Alpaca Camera Simulator   |
      | TELESCOP | string  | Takahashi FSQ-106EDX4     |
      | FILTER   | string  | Luminance                 |
      | CCD-TEMP | real    | 10                        |
      | XBINNING | integer | 1                         |
      | YBINNING | integer | 1                         |
      | XPIXSZ   | real    | 5.6                       |
      | YPIXSZ   | real    | 5.6                       |
      | FOCALLEN | real    | 530                       |
      | APTDIA   | real    | 106                       |
      | FOCRATIO | real    | 5                         |
      | SITELAT  | real    | 51.0786                   |
      | SITELONG | real    | -0.2944                   |
      | SWCREATE | string  | rusty-photon rp {version} |
      | DOC_ID   | string  | {document_id}             |
    And the captured FITS header should not carry these keywords:
      | keyword  |
      | GAIN     |
      | OFFSET   |
      | SET-TEMP |
    And the captured FITS header "DATE-OBS" should be a UTC timestamp of the form "YYYY-MM-DDThh:mm:ss.sss"
    And the captured FITS header RA and DEC should match the mount's reported position within 0.1 degrees

  Scenario: The sidecar records the same per-frame facts the header carries
    Given rp is running with a fully equipped capture rig on the simulator
    And the MCP client has added a target named "M31" at ra_hours 0.7123 dec_degrees 41.2689
    When the MCP client calls "capture" with camera "main-cam" for 100 ms, the added target, and frame_type "Light"
    And I fetch the document for the captured document_id
    Then the tool call should succeed
    And the document field "camera_name" should be "Alpaca Camera Simulator"
    And the document field "train_id" should be "main"
    And the document field "telescope" should be "Takahashi FSQ-106EDX4"
    And the document field "filter" should be "Luminance"
    And the document body should not contain "gain"
    And the document body should not contain "offset"
    And the captured FITS header DATE-OBS should be the document's exposure_started_at
    And the document's exposure_started_at should not be later than its captured_at
    And the captured FITS header RA and DEC should be the document's pointing in degrees

  Scenario Outline: Calibration frames name their type and carry no object keywords
    Given rp is running with a fully equipped capture rig on the simulator
    When the MCP client calls "capture" with camera "main-cam" for 100 ms and frame_type "<frame_type>"
    Then the tool call should succeed
    And the captured FITS header should carry these keywords:
      | keyword  | type   | value      |
      | IMAGETYP | string | <imagetyp> |
    And the captured FITS header should not carry these keywords:
      | keyword  |
      | OBJECT   |
      | OBJCTRA  |
      | OBJCTDEC |

    Examples:
      | frame_type | imagetyp   |
      | Dark       | Dark Frame |
      | Flat       | Flat Field |
      | Bias       | Bias Frame |

  Scenario: A Dark frame carries no FILTER even with a filter wheel in the train
    Given rp is running with a fully equipped capture rig on the simulator
    When the MCP client calls "capture" with camera "main-cam" for 100 ms and frame_type "Dark"
    And I fetch the document for the captured document_id
    Then the tool call should succeed
    And the captured FITS header should not carry these keywords:
      | keyword |
      | FILTER  |
    And the document body should not contain "filter"

  Scenario: A Flat frame records the filter in the beam
    Given rp is running with a fully equipped capture rig on the simulator
    When the MCP client calls "capture" with camera "main-cam" for 100 ms and frame_type "Flat"
    Then the tool call should succeed
    And the captured FITS header should carry these keywords:
      | keyword | type   | value     |
      | FILTER  | string | Luminance |

  Scenario: An untyped capture carries every keyword but the frame-type and object ones
    Given rp is running with a fully equipped capture rig on the simulator
    When the MCP client calls "capture" with camera "main-cam" for 100 ms
    Then the tool call should succeed
    And the captured FITS header should carry these keywords:
      | keyword  | type   | value                   |
      | EXPTIME  | real   | 0.1                     |
      | INSTRUME | string | Alpaca Camera Simulator |
      | TELESCOP | string | Takahashi FSQ-106EDX4   |
      | FILTER   | string | Luminance               |
      | FOCALLEN | real   | 530                     |
    And the captured FITS header should not carry these keywords:
      | keyword  |
      | IMAGETYP |
      | OBJECT   |
      | OBJCTRA  |
      | OBJCTDEC |

  Scenario: A binned frame records the binned pixel size
    Given rp is running with a fully equipped capture rig on the simulator
    When the MCP client calls "capture" with camera "main-cam" for 100 ms at binning "2x2"
    Then the tool call should succeed
    And the captured FITS header should carry these keywords:
      | keyword  | type    | value |
      | XBINNING | integer | 2     |
      | YBINNING | integer | 2     |
      | XPIXSZ   | real    | 11.2  |
      | YPIXSZ   | real    | 11.2  |

  Scenario: A camera outside every train, with no mount or site, writes only what it knows
    Given rp is running with a camera on the simulator
    When the MCP client calls "capture" with camera "main-cam" for 100 ms
    Then the tool call should succeed
    And the captured FITS header should carry these keywords:
      | keyword  | type    | value                     |
      | EXPTIME  | real    | 0.1                       |
      | INSTRUME | string  | Alpaca Camera Simulator   |
      | XBINNING | integer | 1                         |
      | XPIXSZ   | real    | 5.6                       |
      | SWCREATE | string  | rusty-photon rp {version} |
      | DOC_ID   | string  | {document_id}             |
    And the captured FITS header should not carry these keywords:
      | keyword  |
      | TELESCOP |
      | FOCALLEN |
      | APTDIA   |
      | FOCRATIO |
      | FILTER   |
      | RA       |
      | DEC      |
      | SITELAT  |
      | SITELONG |
    And the captured FITS header "DATE-OBS" should be a UTC timestamp of the form "YYYY-MM-DDThh:mm:ss.sss"
