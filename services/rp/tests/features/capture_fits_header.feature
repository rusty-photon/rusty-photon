@serial
Feature: FITS header on captured frames
  Every frame `capture` writes carries the standard FITS keywords that
  third-party tools read - ASTAP, PixInsight, Siril, astrometry.net - so a
  frame stays self-describing after it leaves rp without its sidecar
  (rp.md, Persistence: FITS header). The header is an export of the
  exposure document: each keyword mirrors a document field, so the
  header carries no fact the sidecar lacks, and the sidecar is the
  record (rp.md, Ground rules).

  A keyword whose source is unknown is left out, never written as a
  placeholder, and no header problem fails a capture. The simulator camera
  implements neither Gain nor Offset and these rigs configure no cooling,
  so their frames carry no GAIN, OFFSET or SET-TEMP. XPIXSZ/YPIXSZ are the
  pixel size after binning. TELESCOP is the train's configured telescope,
  and APTDIA/FOCRATIO its aperture and focal ratio, which the sidecar
  records in its optics block. SITELAT/SITELONG are the sidecar's site,
  and SWCREATE names the rp_version that captured the frame. RA/DEC are
  where the mount pointed; OBJCTRA/OBJCTDEC are the target's catalog
  coordinates. DATE-OBS is the exposure start, the document's
  `exposure_started_at`.

  The header does not export every fact the sidecar records. READOUTM is
  the readout mode's name. The electrons per ADU and the cooler power are
  recorded in the sidecar only: no frame carries EGAIN, because a driver's
  ADU need not be the file's. A colour mosaic is exported as BAYERPAT on an
  unbinned frame only, and a monochrome frame carries no Bayer cards.
  The simulator camera is a monochrome sensor with one readout mode,
  "Default", and 0.8 electrons per ADU, and it reports cooler power, 0 %
  while its cooler is off. Every frame says its rows run top-down: ROWORDER
  is TOP-DOWN.

  The rig state goes out under the cards N.I.N.A. writes for it, in its
  units: the mount's altitude, azimuth, airmass and pier side, the filter
  wheel's name, the train's terminal focuser, the train's one rotator by
  its mechanical angle, the position angle the frame was meant to be
  taken at, and each weather sensor from the first device that reports
  it, wind in km/h. Tracking, guiding, the safety monitors and the rain
  rate stay in the sidecar. The whole rig is the one the rig-state feature
  describes; the simulator lets its focuser's probe and its weather drift,
  so those cards are checked against the sidecar fields they copy.

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
      | OBJCTROT | real    | 0                         |
      | PIERSIDE | string  | East                      |
      | INSTRUME | string  | Alpaca Camera Simulator   |
      | TELESCOP | string  | Takahashi FSQ-106EDX4     |
      | FILTER   | string  | Luminance                 |
      | FWHEEL   | string  | Alpaca Filter Wheel Simulator |
      | READOUTM | string  | Default                   |
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
      | ROWORDER | string  | TOP-DOWN                  |
    And the captured FITS header should not carry these keywords:
      | keyword  |
      | GAIN     |
      | OFFSET   |
      | SET-TEMP |
      | EGAIN    |
      | BAYERPAT |
      | XBAYROFF |
      | YBAYROFF |
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
    And the document at "/filter_position" should be the JSON 0
    And the document should have no value at "/filter_wavelength_nm"
    And the document field "filter_wheel_name" should be "Alpaca Filter Wheel Simulator"
    And the document at "/train_devices" should be the JSON [{"id": "main-fw", "kind": "filter wheel"}, {"id": "main-cam", "kind": "camera"}]
    And the document at "/optics/focal_length_mm" should be the JSON 530.0
    And the document at "/optics/aperture_mm" should be the JSON 106.0
    And the document at "/optics/pixel_size_x_um" should be the JSON 5.6
    And the document at "/optics/pixel_size_y_um" should be the JSON 5.6
    And the document at "/site" should be the JSON {"latitude_degrees": 51.0786, "longitude_degrees": -0.2944}
    And the document's rp_version should be rp's own version
    And the document body should not contain "gain"
    And the document body should not contain "offset"
    And the captured FITS header DATE-OBS should be the document's exposure_started_at
    And the document's exposure_started_at should not be later than its captured_at
    And the captured FITS header RA and DEC should be the document's pointing in degrees

  Scenario: A frame on the whole rig carries the rig-state keywords
    Given rp is running with the whole rig on the simulator and default position angle 254.0
    When the MCP client calls "move_focuser" with focuser "main-focuser" to position 25200
    And the MCP client calls "move_rotator" with rotator "main-rotator" to angle 10.0
    And the MCP client calls "capture" with camera "main-cam" for 100 ms
    And I fetch the document for the captured document_id
    Then the tool call should succeed
    And the captured FITS header should carry these keywords:
      | keyword  | type    | value                         |
      | OBJCTROT | real    | 254                           |
      | PIERSIDE | string  | East                          |
      | FWHEEL   | string  | Alpaca Filter Wheel Simulator |
      | FOCNAME  | string  | Alpaca Focuser Simulator      |
      | FOCPOS   | integer | 25200                         |
      | FOCUSPOS | integer | 25200                         |
      | FOCUSSZ  | real    | 20                            |
      | ROTNAME  | string  | Alpaca Rotator Simulator      |
      | ROTATOR  | real    | 10                            |
      | ROTATANG | real    | 10                            |
      | ROTSTPSZ | real    | 0.75                          |
    And the captured FITS header should carry each of these keywords as the document field it copies:
      | keyword  | field                                               |
      | CENTALT  | /altitude_degrees                                   |
      | CENTAZ   | /azimuth_degrees                                    |
      | AIRMASS  | /airmass                                            |
      | FOCTEMP  | /focusers/0/temperature_c                           |
      | FOCUSTEM | /focusers/0/temperature_c                           |
      | AMBTEMP  | /observing_conditions/0/temperature_c               |
      | HUMIDITY | /observing_conditions/0/humidity_pct                |
      | DEWPOINT | /observing_conditions/0/dew_point_c                 |
      | PRESSURE | /observing_conditions/0/pressure_hpa                |
      | CLOUDCVR | /observing_conditions/0/cloud_cover_pct             |
      | SKYBRGHT | /observing_conditions/0/sky_brightness_lux          |
      | MPSAS    | /observing_conditions/0/sky_quality_mag_per_arcsec2 |
      | SKYTEMP  | /observing_conditions/0/sky_temperature_c           |
      | STARFWHM | /observing_conditions/0/star_fwhm_arcsec            |
      | WINDDIR  | /observing_conditions/0/wind_direction_degrees      |
    And the captured FITS header should carry each of these keywords in km/h as the document field in m/s it copies:
      | keyword  | field                                   |
      | WINDSPD  | /observing_conditions/0/wind_speed_m_per_s |
      | WINDGUST | /observing_conditions/0/wind_gust_m_per_s  |

  Scenario: The sidecar records what the camera reports about its sensor and readout
    Given rp is running with a fully equipped capture rig on the simulator
    When the MCP client calls "capture" with camera "main-cam" for 100 ms
    And I fetch the document for the captured document_id
    Then the tool call should succeed
    And the document field "sensor_type" should be "Monochrome"
    And the document should have no value at "/bayer_offset_x"
    And the document should have no value at "/bayer_offset_y"
    And the document at "/readout_mode" should be the JSON 0
    And the document field "readout_mode_name" should be "Default"
    And the document at "/electrons_per_adu" should be the JSON 0.8
    And the document at "/cooler_power_pct" should be the JSON 0.0

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
      | FWHEEL  |
    And the document body should not contain "filter"
    And the document body should not contain "filter_wheel_name"

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
      | ROWORDER | string  | TOP-DOWN                  |
    And the captured FITS header should not carry these keywords:
      | keyword  |
      | TELESCOP |
      | FOCALLEN |
      | APTDIA   |
      | FOCRATIO |
      | FILTER   |
      | FWHEEL   |
      | RA       |
      | DEC      |
      | CENTALT  |
      | CENTAZ   |
      | AIRMASS  |
      | PIERSIDE |
      | OBJCTROT |
      | FOCNAME  |
      | FOCPOS   |
      | FOCUSSZ  |
      | FOCTEMP  |
      | ROTNAME  |
      | ROTATOR  |
      | ROTATANG |
      | AMBTEMP  |
      | WINDSPD  |
      | SITELAT  |
      | SITELONG |
    And the captured FITS header "DATE-OBS" should be a UTC timestamp of the form "YYYY-MM-DDThh:mm:ss.sss"
