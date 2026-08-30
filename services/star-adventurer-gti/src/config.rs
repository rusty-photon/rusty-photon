//! Configuration types.
//!
//! See [`docs/services/star-adventurer-gti.md`](../../../docs/services/star-adventurer-gti.md)
//! §"Configuration" for the canonical schema and field meanings.

use std::net::{IpAddr, Ipv4Addr};
use std::path::Path;
use std::time::Duration;

pub use rusty_photon_server_config::AlpacaServerConfig;
use serde::{Deserialize, Serialize};

/// Top-level configuration deserialised from the JSON config file.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub transport: TransportConfig,
    pub server: AlpacaServerConfig,
    pub mount: MountConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            transport: TransportConfig::default(),
            server: AlpacaServerConfig::new(11_117),
            mount: MountConfig::default(),
        }
    }
}

/// Transport block — `usb` (serial) or `udp` (`WiFi`).
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum TransportConfig {
    Usb(UsbConfig),
    Udp(UdpConfig),
}

impl Default for TransportConfig {
    fn default() -> Self {
        Self::Usb(UsbConfig::default())
    }
}

impl TransportConfig {
    /// Human-readable label for the configured transport target. Used by
    /// the connect handshake's wrong-device diagnostic (see
    /// [`crate::error::StarAdvError::WrongDevice`] and issue #254) to
    /// quote the configured port in operator-facing error messages.
    ///
    /// * USB → the configured serial-port path verbatim (e.g.
    ///   `/dev/serial/by-id/...` or `/dev/ttyACM0`).
    /// * UDP → [`SocketAddr`]'s `Display` form, which brackets IPv6
    ///   correctly (`[fe80::1]:11880`) and leaves IPv4 unbracketed
    ///   (`192.168.4.1:11880`). Matches the canonical operator-typing
    ///   form a tool like `nc -u` accepts.
    ///
    /// [`SocketAddr`]: std::net::SocketAddr
    #[must_use]
    pub fn port_label(&self) -> String {
        match self {
            Self::Usb(u) => u.port.clone(),
            Self::Udp(u) => std::net::SocketAddr::new(u.address, u.port).to_string(),
        }
    }
}

/// USB-CDC serial transport config.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UsbConfig {
    pub port: String,
    #[serde(default = "default_baud_rate")]
    pub baud_rate: u32,
    #[serde(default = "default_command_timeout", with = "humantime_serde")]
    #[schemars(with = "String")]
    pub command_timeout: Duration,
    #[serde(default = "default_polling_interval", with = "humantime_serde")]
    #[schemars(with = "String")]
    pub polling_interval: Duration,
}

/// UDP/WiFi transport config.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UdpConfig {
    pub address: IpAddr,
    #[serde(default = "default_udp_port")]
    pub port: u16,
    /// Mandatory on UDP — must be a 192.168.4.0/24 host IP when the mount is
    /// in AP mode.
    pub bind_address: IpAddr,
    #[serde(default = "default_command_timeout", with = "humantime_serde")]
    #[schemars(with = "String")]
    pub command_timeout: Duration,
    #[serde(default = "default_polling_interval", with = "humantime_serde")]
    #[schemars(with = "String")]
    pub polling_interval: Duration,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MountConfig {
    pub name: String,
    /// Spec-compliant ASCOM `UniqueID`. Ships **empty** so that
    /// [`rusty_photon_config::materialize_identity`] mints a persistent
    /// `UUIDv4` on first run (see the design doc's
    /// [§"Device identity (`UniqueID`)"](../../../docs/services/star-adventurer-gti.md#device-identity-uniqueid)).
    /// `#[serde(default)]` lets the field be absent on disk: the
    /// startup materialize step fills it into the file's `mount` object
    /// before the config is loaded, so the running driver always sees a
    /// real id.
    #[serde(default)]
    pub unique_id: String,
    pub description: String,
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// WGS84 degrees, +N. ASCOM convention.
    pub site_latitude_deg: f64,
    /// WGS84 degrees, +E. ASCOM convention.
    pub site_longitude_deg: f64,
    #[serde(default)]
    pub site_elevation_m: f64,

    #[serde(default = "default_settle_after_slew", with = "humantime_serde")]
    #[schemars(with = "String")]
    pub settle_after_slew: Duration,

    /// Reserved for future expansion. Sidereal only in MVP.
    #[serde(default)]
    pub tracking_rate: TrackingRateName,

    /// CW exclusion zone in encoder `mech_HA` (signed
    /// hours, folded to `[−12, +12)`). Slews / syncs whose chosen
    /// pier side's `mech_HA` falls inside the active zone's open
    /// interval `(min_hours, max_hours)` are
    /// rejected with `INVALID_VALUE` and never reach the wire. Flip
    /// slews additionally check that the *path swept* by the polar
    /// axis stays clear of the zone — see [`flip_slew_ra_delta`].
    ///
    /// The physical constraint on the `GTi` is "the counterweights
    /// must not rise more than 0.95 h (≈ 14°) above horizontal at
    /// any point." This carves a one-sided arc on the positive
    /// `mech_HA` side: the CW shaft crosses horizontal at
    /// `mech_HA = +0.95`, peaks at `mech_HA = +6`, and crosses
    /// horizontal again at `mech_HA = +11.05`. The negative-mech_HA
    /// mirror is *not* a CW exclusion zone — the CW swings below horizontal
    /// into the ground beneath the pier rather than above the mount
    /// head.
    ///
    /// Defaults are `(0.95, 11.05)` — the wide-zone reading of the
    /// "0.95 h above horizontal" rule, hardware-verified at lat
    /// 32.7°N. The narrower `(6.95, 11.05)` zone the driver used
    /// previously captured only the *outer* portion (CW descending
    /// past the pier from above), missing the inner ascent through
    /// which the OTA contacted the tripod during the 2026-05-17
    /// session.
    ///
    /// JSON form is the active object `{ "min_hours": .., "max_hours": .. }`
    /// (validated at deserialize time), or `null` to disable the check
    /// entirely (deserialises to [`CwExclusionZone::Disabled`]); only
    /// disable for tests or for mounts whose CW exclusion arc is known to
    /// be elsewhere. See the design doc's
    /// [§Per-pier safety envelopes](../../../docs/services/star-adventurer-gti.md#per-pier-safety-envelopes).
    #[serde(default)]
    pub cw_exclusion_zone: CwExclusionZone,

    /// Slack added on both edges of the CW exclusion zone for the
    /// **tracking-time safety guard** (see the design doc's
    /// [§"Tracking-time safety guard"](../../../docs/services/star-adventurer-gti.md#tracking-time-safety-guard)).
    ///
    /// While `Tracking = true`, a background watcher stops the mount
    /// (`:K1`) once the live encoder `mech_HA` enters the active zone
    /// widened by the margin — `(min_hours − margin, max_hours + margin)` —
    /// before tracking drift can carry the counterweights into the
    /// zone. The margin lets cautious operators stop early; `0.0`
    /// stops exactly at zone entry. Defaults to `0.05` h (≈ 45 s of
    /// sidereal drift).
    ///
    /// Independent of [`FlipPolicy::enabled`]: the guard is the safety
    /// floor and runs whenever `cw_exclusion_zone` is
    /// [`Active`](CwExclusionZone::Active), regardless of meridian-flip
    /// support. Validated at deserialize time by
    /// [`TrackingGuardMarginHours`]: a non-finite, negative, or over-cap
    /// value (> [`MAX_TRACKING_GUARD_MARGIN_HOURS`]) fails config loading.
    /// The guard additionally treats a non-finite or negative value as
    /// `0.0` as defense-in-depth for construction paths that bypass
    /// validation.
    #[serde(default)]
    pub tracking_guard_margin_hours: TrackingGuardMarginHours,

    /// Minimum apparent-altitude floor, degrees. Slew / sync targets
    /// whose computed local altitude — from the target hour angle,
    /// declination, and `site_latitude_deg` via
    /// [`crate::coordinates::ra_dec_to_alt_az`] — is below the
    /// floor are rejected with `INVALID_VALUE` before any wire motion;
    /// a target exactly at the floor is accepted. Default `0.0` — the
    /// geometric horizon. Positive values add an operator buffer
    /// (refraction, horizon light pollution, local obstructions);
    /// negative values permit below-horizon pointing (dust-cap
    /// operations, closed-roof flats) and are logged `info!` at
    /// startup; `-90` never rejects anything. Replaced the rectangular
    /// `dec_limits` Dec envelope (2026-07-01); `MountConfig`'s
    /// `deny_unknown_fields` (#484) means a stale `dec_limits` key in
    /// an existing config file is now rejected loudly at load, naming
    /// the field, rather than being silently ignored. See the
    /// design doc's
    /// [§Altitude floor](../../../docs/services/star-adventurer-gti.md#altitude-floor).
    #[serde(default)]
    pub min_altitude_degrees: MinAltitudeDegrees,

    /// Persisted park-target encoder positions, written by `SetPark`
    /// and read on every connect. When `None` (default on first run),
    /// the driver falls back to the encoder positions captured during
    /// the init handshake (`:j1` / `:j2`). See the design doc's
    /// [§"Park lifecycle"](../../../docs/services/star-adventurer-gti.md#park-lifecycle)
    /// and [§"Park persistence"](../../../docs/services/star-adventurer-gti.md#park-persistence).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub park_ra_ticks: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub park_dec_ticks: Option<i32>,

    /// Meridian-flip policy. See the design doc's
    /// [§"Meridian flip"](../../../docs/services/star-adventurer-gti.md#meridian-flip).
    /// Defaults to `enabled = false` so the driver behaves identically
    /// to pre-Phase-6 builds until an operator opts in on a
    /// hardware-validated mount.
    #[serde(default)]
    pub flip_policy: FlipPolicy,

    /// Physical pose the operator powers the mount up in. This field is
    /// the **operator's assertion about the physical world**, so the
    /// ship default is [`ApPark::ApPark0`] ("current position") — the
    /// only value that is true when the operator has asserted nothing.
    /// A default that named a pose would seed a confidently *wrong*
    /// coordinate frame whenever the mount was powered up elsewhere,
    /// and a subsequent `Park()` would slew to a fabricated position.
    ///
    /// For `ap_park_1..ap_park_5` the driver seeds the firmware encoder
    /// on the fresh-power-up connect (no motion, just `:E1` / `:E2`) so
    /// the codebase's celestial-coordinate math matches the operator's
    /// physical pose — this **anchors the coordinate frame**, which is
    /// what permits `Park()` to slew to an absolute pose target.
    /// Operators with a repeatable setup should set their power-up pose
    /// (typically `ap_park_3`, Sky-Watcher's stock home): with the
    /// default `preferred_ap_park` equal to that pose, a park issued
    /// right after connect is a zero-distance goto. For `ap_park_0` the
    /// driver does **no** seeding and trusts the firmware encoder
    /// as-is — the operator will ground-truth the position via
    /// plate-solve + sync; until that sync the frame is unanchored and
    /// `Park()` stops in place instead of slewing (see the design doc's
    /// [§Park lifecycle](../../../docs/services/star-adventurer-gti.md#park-lifecycle)).
    ///
    /// The runtime `SetUnparkFromApPosition` Action persists a new value
    /// here (applied on the next fresh-power-up). See [`ApPark`] for the
    /// supported positions, the design doc's
    /// [§Unpark from AP position](../../../docs/services/star-adventurer-gti.md#unpark-from-ap-position),
    /// and the wiring in `MountDevice::seed_after_connect`.
    #[serde(default = "default_unpark_from_ap_position")]
    pub unpark_from_ap_position: ApPark,

    /// AP park the standard ASCOM `Park()` slews to when no raw
    /// `park_*_ticks` override is set. Defaults to [`ApPark::ApPark3`]
    /// (Sky-Watcher's stock power-up pose along the polar axis).
    ///
    /// `ap_park_0` is rejected at deserialize time — "current position"
    /// is not a slew target. The runtime `SetPreferredApPark` Action
    /// persists a new value here. When both this and an explicit
    /// `park_ra_ticks` / `park_dec_ticks` pair are set, the raw tick
    /// pair wins (per-axis). See the design doc's
    /// [§Custom Actions for runtime control](../../../docs/services/star-adventurer-gti.md#custom-actions-for-runtime-control).
    #[serde(
        default = "default_preferred_ap_park",
        deserialize_with = "deserialize_preferred_ap_park"
    )]
    pub preferred_ap_park: ApPark,
}

/// Master switch + parameters for driver-planned meridian flips.
///
/// `enabled = false` (the shipped default) disables every flip code
/// path: `CanSetPierSide` reports `false`, `SetSideOfPier` returns
/// `NOT_IMPLEMENTED`, `DestinationSideOfPier` always returns the
/// current side, and slews use the pre-flip coordinate pipeline only.
///
/// With `enabled = true`, the driver may pick the flipped pier side
/// for a slew (or honour an explicit `SetSideOfPier` call) when the
/// target's hour angle falls inside the meridian window of width
/// `2 × flip_range_hours`. See the design doc for the full decision
/// tree and the per-side safety envelopes.
///
/// Deserialised via [`FlipPolicyWire`] so the cross-field invariant —
/// `auto_flip_at_meridian_offset_hours` finite and within
/// `±flip_range_hours` — is checked at config load, with the field
/// named, like the single-field newtype validations.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(into = "FlipPolicyWire", try_from = "FlipPolicyWire")]
pub struct FlipPolicy {
    /// Master switch. Defaults `false` until the first real-hardware
    /// meridian flip on a `GTi` has been verified.
    pub enabled: bool,

    /// Half-width of the target-HA window around the meridian where
    /// the flipped state is mechanically reachable. Targets with
    /// `|target_HA| > flip_range_hours` are unflippable: the slew
    /// planner uses normal pointing only and `DestinationSideOfPier`
    /// returns the current side. Valid range `(0, 0.95]`; the upper
    /// bound matches the Phase 1.1 hardware-verified headroom past
    /// counterweight-horizontal on the pre-flip side.
    pub flip_range_hours: FlipRangeHours,

    /// Opt-in driver-initiated meridian flip while tracking. When
    /// `true` (and `enabled` is `true`), the tracking watcher issues
    /// the same through-wrap flip slew `SetSideOfPier` would once the
    /// live encoder `mech_HA` reaches
    /// `auto_flip_at_meridian_offset_hours`, then re-engages tracking
    /// on the new pier side. Defaults `false` — hosts like NINA / SGP
    /// own flip timing themselves, and a mid-exposure auto-flip breaks
    /// running frames. See the design doc's
    /// [§"Auto-flip during tracking"](../../../docs/services/star-adventurer-gti.md#auto-flip-during-tracking).
    pub auto_flip_during_tracking: bool,

    /// Target hour angle at which the auto-flip fires, hours. `0.0`
    /// (the default) flips exactly at meridian crossing; positive
    /// values delay the flip past the meridian (the common
    /// astrophotography preference). Only consulted when
    /// `auto_flip_during_tracking = true`. Must be finite and within
    /// `[−flip_range_hours, +flip_range_hours]` — validated at
    /// deserialize by the [`FlipPolicyWire`] `try_from`.
    pub auto_flip_at_meridian_offset_hours: f64,
}

impl Default for FlipPolicy {
    fn default() -> Self {
        Self {
            enabled: default_flip_policy_enabled(),
            flip_range_hours: FlipRangeHours::default(),
            auto_flip_during_tracking: false,
            auto_flip_at_meridian_offset_hours: 0.0,
        }
    }
}

/// Wire shape for [`FlipPolicy`]: same fields, each with its serde
/// default, plus the cross-field validation in `TryFrom`. Mirrors the
/// [`ActiveZoneWire`] pattern.
#[derive(Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct FlipPolicyWire {
    #[serde(default = "default_flip_policy_enabled")]
    enabled: bool,
    #[serde(default)]
    flip_range_hours: FlipRangeHours,
    #[serde(default)]
    auto_flip_during_tracking: bool,
    #[serde(default)]
    auto_flip_at_meridian_offset_hours: f64,
}

impl TryFrom<FlipPolicyWire> for FlipPolicy {
    type Error = String;
    fn try_from(w: FlipPolicyWire) -> std::result::Result<Self, String> {
        let range = w.flip_range_hours.value();
        let offset = w.auto_flip_at_meridian_offset_hours;
        if !offset.is_finite() || offset.abs() > range {
            return Err(format!(
                "flip_policy.auto_flip_at_meridian_offset_hours must be finite and within \
                 [-flip_range_hours, +flip_range_hours] = [{}, {range}] hours, got {offset}",
                -range
            ));
        }
        Ok(Self {
            enabled: w.enabled,
            flip_range_hours: w.flip_range_hours,
            auto_flip_during_tracking: w.auto_flip_during_tracking,
            auto_flip_at_meridian_offset_hours: w.auto_flip_at_meridian_offset_hours,
        })
    }
}

impl From<FlipPolicy> for FlipPolicyWire {
    fn from(p: FlipPolicy) -> Self {
        Self {
            enabled: p.enabled,
            flip_range_hours: p.flip_range_hours,
            auto_flip_during_tracking: p.auto_flip_during_tracking,
            auto_flip_at_meridian_offset_hours: p.auto_flip_at_meridian_offset_hours,
        }
    }
}

/// Outer bound on `FlipPolicy::flip_range_hours`.
///
/// Larger values would push the post-flip mechanical hour angle into the
/// unverified mirror of the Phase 4 counterweight-up CW exclusion zone.
/// See the plan `docs/plans/star-adventurer-gti-meridian-flip.md` §2.6.
pub const MAX_FLIP_RANGE_HOURS: f64 = 0.95;

/// Outer bound on [`MountConfig::tracking_guard_margin_hours`].
///
/// The margin is operator-preference slack before the CW exclusion zone,
/// not a mechanical limit; this cap only catches a misconfiguration
/// (e.g. a value entered in degrees, or a unit mix-up) at startup. The
/// shipped default is `0.05` h.
pub const MAX_TRACKING_GUARD_MARGIN_HOURS: f64 = 1.0;

// ===================== Validating config newtypes =====================
//
// Each field invariant lives in the type: an out-of-range value fails at
// `serde_json::from_str` with the offending field named, so a bad config is
// rejected at *load* rather than at slew/track time. This is what retired
// the hand-rolled `MountConfig::validate` / `FlipPolicy::validate`.

/// Half-width of the meridian-flip window, hours. Valid `(0, MAX_FLIP_RANGE_HOURS]`.
/// JSON form is a bare number.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(into = "f64", try_from = "f64")]
pub struct FlipRangeHours(f64);

impl FlipRangeHours {
    /// Unchecked `const` constructor, `pub(crate)` so the public API
    /// cannot bypass validation — it exists only for `Default` and
    /// in-crate callers with compile-time-known-good values (and tests
    /// that deliberately build out-of-range values). External and
    /// runtime-valued callers use [`try_new`](Self::try_new) or serde.
    pub(crate) const fn new(hours: f64) -> Self {
        Self(hours)
    }
    /// Validating constructor: the single source of truth for the
    /// invariant, which serde's `try_from` and any programmatic caller
    /// funnel through.
    ///
    /// # Errors
    ///
    /// Returns a message naming the field unless `hours` is finite and in
    /// `(0, MAX_FLIP_RANGE_HOURS]`.
    pub fn try_new(hours: f64) -> std::result::Result<Self, String> {
        if !hours.is_finite() || hours <= 0.0 || hours > MAX_FLIP_RANGE_HOURS {
            return Err(format!(
                "flip_policy.flip_range_hours must be in (0, {MAX_FLIP_RANGE_HOURS}] hours, got {hours}"
            ));
        }
        Ok(Self(hours))
    }
    /// The underlying value in hours.
    #[must_use]
    pub const fn value(self) -> f64 {
        self.0
    }
}

impl TryFrom<f64> for FlipRangeHours {
    type Error = String;
    fn try_from(v: f64) -> std::result::Result<Self, String> {
        Self::try_new(v)
    }
}

impl From<FlipRangeHours> for f64 {
    fn from(v: FlipRangeHours) -> Self {
        v.0
    }
}

/// Tracking-guard slack before the CW exclusion zone, hours. Valid
/// `[0, MAX_TRACKING_GUARD_MARGIN_HOURS]`. JSON form is a bare number.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(into = "f64", try_from = "f64")]
pub struct TrackingGuardMarginHours(f64);

impl TrackingGuardMarginHours {
    /// Unchecked `const` constructor — see [`FlipRangeHours::new`].
    pub(crate) const fn new(hours: f64) -> Self {
        Self(hours)
    }
    /// Validating constructor — see [`FlipRangeHours::try_new`].
    ///
    /// # Errors
    ///
    /// Returns a message naming the field unless `hours` is finite and in
    /// `[0, MAX_TRACKING_GUARD_MARGIN_HOURS]`.
    pub fn try_new(hours: f64) -> std::result::Result<Self, String> {
        if !hours.is_finite() || !(0.0..=MAX_TRACKING_GUARD_MARGIN_HOURS).contains(&hours) {
            return Err(format!(
                "tracking_guard_margin_hours must be in [0, {MAX_TRACKING_GUARD_MARGIN_HOURS}] hours, got {hours}"
            ));
        }
        Ok(Self(hours))
    }
    /// The underlying value in hours.
    #[must_use]
    pub const fn value(self) -> f64 {
        self.0
    }
}

impl TryFrom<f64> for TrackingGuardMarginHours {
    type Error = String;
    fn try_from(v: f64) -> std::result::Result<Self, String> {
        Self::try_new(v)
    }
}

impl From<TrackingGuardMarginHours> for f64 {
    fn from(v: TrackingGuardMarginHours) -> Self {
        v.0
    }
}

/// An active CW exclusion interval in encoder `mech_HA` (signed hours, folded
/// `[-12, +12)`): `-12 <= min_hours < max_hours <= 12`. JSON form is
/// `{ "min_hours": .., "max_hours": .. }`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(into = "ActiveZoneWire", try_from = "ActiveZoneWire")]
pub struct ActiveZone {
    min_hours: f64,
    max_hours: f64,
}

#[derive(Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct ActiveZoneWire {
    min_hours: f64,
    max_hours: f64,
}

impl ActiveZone {
    /// Unchecked `const` constructor — see [`FlipRangeHours::new`].
    pub(crate) const fn new(min_hours: f64, max_hours: f64) -> Self {
        Self {
            min_hours,
            max_hours,
        }
    }
    /// Validating constructor — see [`FlipRangeHours::try_new`].
    ///
    /// # Errors
    ///
    /// Returns a message naming the field unless both bounds are finite and
    /// `-12 <= min_hours < max_hours <= 12` (folded `mech_HA`);
    /// `null`/[`CwExclusionZone::Disabled`] is how a zone is turned off,
    /// so an inverted interval is rejected rather than silently disabling.
    pub fn try_new(min_hours: f64, max_hours: f64) -> std::result::Result<Self, String> {
        if !min_hours.is_finite() || !max_hours.is_finite() {
            return Err(format!(
                "cw_exclusion_zone bounds must be finite, got ({min_hours}, {max_hours})"
            ));
        }
        if min_hours >= max_hours {
            return Err(format!(
                "active cw_exclusion_zone needs min_hours < max_hours \
                 (use null to disable), got ({min_hours}, {max_hours})"
            ));
        }
        if min_hours < -12.0 || max_hours > 12.0 {
            return Err(format!(
                "active cw_exclusion_zone must satisfy -12 <= min_hours < max_hours <= 12 \
                 (folded mech_HA), got ({min_hours}, {max_hours})"
            ));
        }
        Ok(Self {
            min_hours,
            max_hours,
        })
    }
    #[must_use]
    pub const fn min_hours(self) -> f64 {
        self.min_hours
    }
    #[must_use]
    pub const fn max_hours(self) -> f64 {
        self.max_hours
    }
}

impl TryFrom<ActiveZoneWire> for ActiveZone {
    type Error = String;
    fn try_from(w: ActiveZoneWire) -> std::result::Result<Self, String> {
        Self::try_new(w.min_hours, w.max_hours)
    }
}

impl From<ActiveZone> for ActiveZoneWire {
    fn from(z: ActiveZone) -> Self {
        Self {
            min_hours: z.min_hours,
            max_hours: z.max_hours,
        }
    }
}

/// The counterweight exclusion zone: an [`ActiveZone`] interval, or
/// explicitly [`Disabled`](CwExclusionZone::Disabled).
///
/// Replaces the old `binding_zone_min/max_hours` pair and its
/// `min >= max = disabled` convention. JSON form is the active object, or
/// `null` for disabled.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(from = "Option<ActiveZone>", into = "Option<ActiveZone>")]
pub enum CwExclusionZone {
    Active(ActiveZone),
    Disabled,
}

impl CwExclusionZone {
    /// `(min, max)` bounds for the path/guard/flip checks, which take a
    /// bare `(f64, f64)`. `Disabled` yields the empty interval
    /// `(+∞, −∞)` — `min > max` strictly, which every consumer
    /// (`canonical_path_crosses_binding_zone`, `tracking_guard_breached`,
    /// `select_pier_side_for_target`) treats as "no zone".
    #[must_use]
    pub const fn bounds(self) -> (f64, f64) {
        match self {
            Self::Active(z) => (z.min_hours(), z.max_hours()),
            Self::Disabled => (f64::INFINITY, f64::NEG_INFINITY),
        }
    }
}

impl From<Option<ActiveZone>> for CwExclusionZone {
    fn from(o: Option<ActiveZone>) -> Self {
        o.map_or(Self::Disabled, Self::Active)
    }
}

impl From<CwExclusionZone> for Option<ActiveZone> {
    fn from(z: CwExclusionZone) -> Self {
        match z {
            CwExclusionZone::Active(a) => Some(a),
            CwExclusionZone::Disabled => None,
        }
    }
}

/// Minimum apparent-altitude floor, degrees. Valid finite `[-90, 90]`
/// (`-90` never rejects — the check is effectively disabled). JSON
/// form is a bare number.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(into = "f64", try_from = "f64")]
pub struct MinAltitudeDegrees(f64);

impl MinAltitudeDegrees {
    /// Unchecked `const` constructor — see [`FlipRangeHours::new`].
    pub(crate) const fn new(degrees: f64) -> Self {
        Self(degrees)
    }
    /// Validating constructor — see [`FlipRangeHours::try_new`].
    ///
    /// # Errors
    ///
    /// Returns a message naming the field unless `degrees` is finite and in
    /// `[-90, 90]`.
    pub fn try_new(degrees: f64) -> std::result::Result<Self, String> {
        if !degrees.is_finite() || !(-90.0..=90.0).contains(&degrees) {
            return Err(format!(
                "min_altitude_degrees must be finite in [-90, 90] degrees, got {degrees}"
            ));
        }
        Ok(Self(degrees))
    }
    /// The underlying value in degrees.
    #[must_use]
    pub const fn value(self) -> f64 {
        self.0
    }
}

impl TryFrom<f64> for MinAltitudeDegrees {
    type Error = String;
    fn try_from(v: f64) -> std::result::Result<Self, String> {
        Self::try_new(v)
    }
}

impl From<MinAltitudeDegrees> for f64 {
    fn from(v: MinAltitudeDegrees) -> Self {
        v.0
    }
}

// Defaults live with the types, so the config fields can use bare
// `#[serde(default)]` and no `default_*` free functions are needed.

impl Default for FlipRangeHours {
    /// `0.5` h — a half-hour meridian window.
    fn default() -> Self {
        Self::new(0.5)
    }
}

impl Default for TrackingGuardMarginHours {
    /// `0.05` h (≈ 45 s of sidereal drift).
    fn default() -> Self {
        Self::new(0.05)
    }
}

impl Default for CwExclusionZone {
    /// `(0.95, 11.05)` h — the wide-zone reading of the "0.95 h above
    /// horizontal" rule, hardware-verified at lat 32.7°N.
    fn default() -> Self {
        Self::Active(ActiveZone::new(0.95, 11.05))
    }
}

impl Default for MinAltitudeDegrees {
    /// `0.0`° — the geometric horizon.
    fn default() -> Self {
        Self::new(0.0)
    }
}

/// Physical pose the operator powers the mount up in.
///
/// Expressed as one of the Astro-Physics
/// [Park Positions Defined](https://astro-physics.info/tech_support/mounts/park-positions-defined.pdf)
/// positions.
///
/// The Sky-Watcher firmware resets its encoder counter to `(0, 0)`
/// every power-up. For `ap_park_1..ap_park_5` the driver seeds the
/// firmware encoder on connect so the codebase's coordinate math
/// interprets that zero against the operator's physical pose. The
/// [`ApPark::ApPark0`] variant means "no seed — trust the firmware
/// encoder as-is"; its `codebase_*` accessors return [`None`].
///
/// Each AP pose is mirror-symmetric between the Northern and Southern
/// Hemispheres around the observer's local meridian — the
/// counterweight-shaft direction inverts and the celestial-Dec sign of
/// the target inverts, so the codebase reading for each pose accounts
/// for the observer's hemisphere from `site_latitude_deg`. The
/// `codebase_*` accessor helpers do the hemisphere case-split
/// internally; callers pass `site_latitude_deg` unchanged and get back
/// a single signed encoder-reading value.
///
/// AP Park 4 and Park 5 are "east-side" (post-meridian-flip) poses;
/// the codebase reads them with `mech_HA` at the encoder wrap (`±12 h`)
/// for Park 4 (target on the meridian, anti-pole side) and at
/// `mech_HA = 0` for Park 5 (target on the anti-meridian, pole side
/// horizon), and the Dec encoder is past the celestial pole.
// Custom Serialize/Deserialize map to `preferred` / `ap_park_0..5` strings; the
// derived JsonSchema is advisory (the schema only shapes the UI form — the
// custom Deserialize is the gate). Rendered read-only in the UI regardless.
#[derive(Debug, Clone, Copy, PartialEq, Eq, schemars::JsonSchema)]
pub enum ApPark {
    /// "Current position" — the ship default. No encoder seeding — the
    /// driver trusts the firmware encoder as-is on connect. The
    /// operator asserts they will plate-solve and `SyncToCoordinates`
    /// before any blind-pointing slew; until that sync the coordinate
    /// frame is unanchored and `Park()` stops in place instead of
    /// slewing. The honest assumption when nothing is declared about
    /// the physical setup; operators with a repeatable power-up pose
    /// declare it via a named park instead. Not a valid
    /// `preferred_ap_park` (it is not a slew target). The `codebase_*`
    /// accessors return [`None`] for this variant.
    ApPark0,
    /// AP Park 1. "RA horizontal" (Dec axis east-west horizontal,
    /// saddle on the *west* end, counterweight on the east end). OTA
    /// tube level, pointing at the polar-side horizon — north horizon
    /// for north observers, south horizon for south observers.
    ///
    /// AP table: `Dec = (90 − Latitude)` for north,
    /// `(−90 − Latitude)` for south. Codebase reading:
    /// `mech_HA = 0`, `dec_encoder = ±(90 + |latitude|)` (sign matches
    /// the hemisphere; magnitude > 90 = past-pole encoding — see
    /// [`ApPark::codebase_dec_encoder_degrees`]).
    ApPark1,
    /// AP Park 2. "RA axis vertical, Dec = 0". OTA level facing the
    /// east horizon, counterweight shaft pointing down. The "RA
    /// axis vertical" description is the visual look near the equator;
    /// at any latitude the codebase reading is `mech_HA = −6 h`
    /// (target on the east-rising celestial equator), `dec_encoder
    /// = 0`. Hemisphere-independent.
    ApPark2,
    /// AP Park 3 (also Sky-Watcher's power-on home — the typical
    /// `unpark_from_ap_position` choice for a repeatable setup). OTA
    /// along the polar axis pointing at the visible celestial pole —
    /// Polaris for north observers, SCP for south observers.
    /// Counterweight shaft along the anti-pole half of the polar axis.
    ///
    /// AP table: `Dec = 90` (visible pole). Codebase reading:
    /// `mech_HA = −6 h` (dec axis south-up, same RA position as
    /// Park 2 — see [`ApPark::codebase_mech_ha_hours`]),
    /// `dec_encoder = +90°` for north / `−90°` for south.
    ApPark3,
    /// AP Park 4. East-side / post-meridian-flip equivalent of Park 1:
    /// saddle on the *east* end of the Dec axis, counterweight shaft
    /// horizontal pointing due west. OTA level facing the *anti-polar*
    /// horizon — south horizon for north observers, north horizon for
    /// south observers.
    ///
    /// AP table: `Dec = (−90 + Latitude)` for north,
    /// `(90 + Latitude)` for south. The target's celestial dec lands
    /// at `±(90 − |latitude|)` (sign anti-hemisphere), and the
    /// post-flip Dec encoder = `sign(dec) · (180 − |dec|) =
    /// ∓(90 + |latitude|)`. Codebase reading: `mech_HA = −12 h`
    /// (= `+12 h` via the encoder wrap),
    /// `dec_encoder = ∓(90 + |latitude|)`.
    ApPark4,
    /// AP Park 5. East-side / post-meridian-flip equivalent of Park 1
    /// (mirror of Park 4 across the polar-axis plane): saddle on the
    /// *east* end, counterweight shaft horizontal pointing due west.
    /// OTA level facing the *polar-side* horizon — north horizon for
    /// north observers, south horizon for south observers.
    ///
    /// AP table: `Dec = (90 − Latitude)` for north,
    /// `(−90 − Latitude)` for south. The celestial dec is at the same
    /// magnitude as Park 1 but reached from the post-flip side, so
    /// the post-flip Dec encoder = `±(90 + |latitude|)` (sign matches
    /// the hemisphere). Codebase reading: `mech_HA = 0` (target on
    /// the anti-meridian, post-flip wrap brings `mech_HA` back to 0),
    /// `dec_encoder = ±(90 + |latitude|)`.
    ///
    /// (`Note: Park 5 shown below is only available in APCC and the
    /// AP V2 driver.` per the AP doc — it's an APCC extension, not on
    /// the keypad.)
    ApPark5,
}

impl ApPark {
    /// The canonical `ap_park_N` token for this variant — the single
    /// source of truth for the string form used in config JSON
    /// (`Serialize`/`Deserialize` delegate here), ASCOM `Action`
    /// parameters, and `Action` return values.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ApPark0 => "ap_park_0",
            Self::ApPark1 => "ap_park_1",
            Self::ApPark2 => "ap_park_2",
            Self::ApPark3 => "ap_park_3",
            Self::ApPark4 => "ap_park_4",
            Self::ApPark5 => "ap_park_5",
        }
    }
}

impl std::fmt::Display for ApPark {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ApPark {
    type Err = ApParkParseError;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "ap_park_0" => Ok(Self::ApPark0),
            "ap_park_1" => Ok(Self::ApPark1),
            "ap_park_2" => Ok(Self::ApPark2),
            "ap_park_3" => Ok(Self::ApPark3),
            "ap_park_4" => Ok(Self::ApPark4),
            "ap_park_5" => Ok(Self::ApPark5),
            other => Err(ApParkParseError(other.to_string())),
        }
    }
}

/// Error from parsing an unrecognised AP-park token via
/// [`ApPark::from_str`]. Carries the offending token for the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApParkParseError(pub String);

impl std::fmt::Display for ApParkParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "unknown AP park {:?}; expected one of ap_park_0..ap_park_5",
            self.0
        )
    }
}

impl std::error::Error for ApParkParseError {}

impl Serialize for ApPark {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ApPark {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let token = String::deserialize(deserializer)?;
        token.parse().map_err(serde::de::Error::custom)
    }
}

impl ApPark {
    /// Codebase-convention `mech_HA` (signed hours, `[−12, +12)`)
    /// corresponding to firmware encoder `(0, 0)` at this AP park
    /// for the configured latitude. [`None`] for [`ApPark::ApPark0`]
    /// ("current position" has no fixed encoder mapping).
    ///
    /// AP defines each pose by the OTA pointing direction and which
    /// side of the mount the OTA tube is on (East or West, mechanical).
    /// The pier-side mechanical designation is the same for both
    /// hemispheres, but the driver's natural-vs-flipped pier convention
    /// flips between hemispheres (N natural = pierWest, S natural =
    /// pierEast — see `pre_flip_side` in `MountDevice`). So a pose like
    /// Park 1 ("OTA on west side") is the **natural side** in the
    /// North and the **flipped side** in the South — and its encoder
    /// representation differs accordingly.
    ///
    /// - **Natural side** (`mech_HA` = celestial HA): used when the
    ///   pose's mechanical pier matches the hemisphere's natural pier.
    /// - **Flipped side** (`mech_HA` = celestial HA + 12, folded): used
    ///   when the pose is on the opposite mechanical pier.
    ///
    /// | Pose | Celestial HA | Mech. pier | N: nat / flip | S: nat / flip |
    /// |------|--------------|-----------|---------------|----------------|
    /// | 1    | ±12          | West      | natural       | flipped        |
    /// | 2    | −6           | —         | natural       | natural        |
    /// | 3    | (pole)       | —         | natural       | natural        |
    /// | 4    | 0            | East      | flipped       | natural        |
    /// | 5    | ±12          | East      | flipped       | natural        |
    ///
    /// Concretely (Northern Hemisphere; Southern mirrors via the
    /// hemisphere case-split below):
    /// - Park 1 N: saddle west → `mech_HA = 0` (saddle in west half
    ///   of polar plane; OTA reaches north horizon via past-pole dec
    ///   rotation).
    /// - Park 4 N: saddle east → `mech_HA = −12` (encoder wrap;
    ///   saddle in east half; OTA reaches south horizon via past-
    ///   pole dec rotation).
    /// - Park 5 N: saddle east → `mech_HA = −12` (encoder wrap;
    ///   OTA reaches north horizon via natural-side dec rotation).
    /// - Parks 2 and 3 are hemisphere-neutral for `mech_HA` (`−6`,
    ///   saddle in the south-up direction, neither east nor west).
    #[must_use]
    pub const fn codebase_mech_ha_hours(&self, _latitude_deg: f64) -> Option<f64> {
        // We don't case-split on hemisphere for mech_HA: the saddle
        // east/west position is a function of the encoder alone,
        // which is hemisphere-independent. The dec encoder *is*
        // hemisphere-dependent — see [`codebase_dec_encoder_degrees`].
        match self {
            // "Current position" has no fixed encoder mapping.
            Self::ApPark0 => None,
            // Park 1: OTA west of mount → saddle west → mech_HA in
            // the (−6, +6) range. `mech_HA = 0` is the canonical
            // saddle-west position (dec axis east-west horizontal).
            Self::ApPark1 => Some(0.0),
            // Park 2 and Park 3 share the same RA position ("RA axis
            // vertical" per the AP doc); only the dec rotation differs.
            // Both put the dec axis south-up out of east-west horizontal
            // (`mech_HA = −6`).
            Self::ApPark2 | Self::ApPark3 => Some(-6.0),
            // Parks 4 and 5: OTA east of mount → saddle east →
            // `mech_HA` in the wrap region (only the dec rotation
            // differs between them). `mech_HA = −12` is the canonical
            // saddle-east position.
            Self::ApPark4 | Self::ApPark5 => Some(-12.0),
        }
    }

    /// Codebase-convention Dec encoder reading (degrees, signed,
    /// `[−180, +180)`) corresponding to firmware encoder `(0, 0)` at
    /// this home pose, given the observer's latitude.
    ///
    /// Hemisphere-dependent because the OTA points at the visible
    /// pole / horizon, which has opposite celestial-Dec signs between
    /// hemispheres. The codebase `dec_enc` value is the rotation angle
    /// around the dec axis from the dec=0 reference (which itself is
    /// hemisphere-dependent at fixed `mech_HA`).
    #[must_use]
    pub fn codebase_dec_encoder_degrees(&self, latitude_deg: f64) -> Option<f64> {
        let northern = latitude_deg >= 0.0;
        let lat_abs = latitude_deg.abs();
        // Magnitude common to Parks 1, 4, 5 — the celestial Dec at the
        // polar-side / anti-polar horizon at this latitude.
        let horizon_dec_mag = 90.0 - lat_abs;
        match self {
            // "Current position" has no fixed encoder mapping.
            Self::ApPark0 => None,
            // Park 1: saddle west (mech_HA=0). OTA at polar-side
            // horizon — north horizon for N (alt=0, az=0), south
            // horizon for S. From the saddle-west position at
            // mech_HA=0 the OTA reaches the polar-side horizon via
            // a past-pole rotation: `dec_enc ≈ ±(90 + |lat|)` (sign
            // matches hemisphere; magnitude > 90 indicates the
            // past-pole encoding).
            Self::ApPark1 => Some(if northern {
                90.0 + lat_abs
            } else {
                -(90.0 + lat_abs)
            }),
            Self::ApPark2 => Some(0.0),
            Self::ApPark3 => Some(
                // OTA at the visible celestial pole. Both hemispheres
                // encode this as `dec_enc = ±90°` (sign matches
                // hemisphere — celestial pole at +Dec in N, −Dec
                // in S).
                if northern { 90.0 } else { -90.0 },
            ),
            // Park 4: saddle east (mech_HA=-12). OTA at the anti-
            // polar horizon — south for N (alt=0, az=180), north
            // for S. Past-pole rotation gives `dec_enc = ∓(90+|lat|)`
            // (sign opposite hemisphere).
            Self::ApPark4 => Some(if northern {
                -(90.0 + lat_abs)
            } else {
                90.0 + lat_abs
            }),
            // Park 5: saddle east (mech_HA=-12, same as Park 4) but
            // OTA at the polar-side horizon (180° around dec axis
            // from Park 4). `dec_enc = ±(90 − |lat|)` (natural-side
            // encoding; |dec_enc| < 90).
            Self::ApPark5 => Some(if northern {
                horizon_dec_mag
            } else {
                -horizon_dec_mag
            }),
        }
    }
}

#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum TrackingRateName {
    #[default]
    Sidereal,
}

const fn default_baud_rate() -> u32 {
    115_200
}
const fn default_command_timeout() -> Duration {
    Duration::from_secs(2)
}
const fn default_polling_interval() -> Duration {
    Duration::from_millis(200)
}
const fn default_udp_port() -> u16 {
    11_880
}
const fn default_settle_after_slew() -> Duration {
    Duration::from_secs(2)
}
const fn default_flip_policy_enabled() -> bool {
    false
}
const fn default_true() -> bool {
    true
}
const fn default_unpark_from_ap_position() -> ApPark {
    // Ship default: "current position" — the only honest value when the
    // operator has asserted nothing about the physical pose. The frame
    // stays unanchored (Park stops in place) until a plate-solve sync,
    // so an unconfigured install can never slew to a fabricated
    // position. Operators with a repeatable setup declare their
    // power-up pose (typically ApPark3, Sky-Watcher's stock home) to
    // get an anchored frame and a zero-distance park from connect.
    ApPark::ApPark0
}
const fn default_preferred_ap_park() -> ApPark {
    // Sky-Watcher's stock power-up pose (OTA along the polar axis at
    // the visible celestial pole) — a sensible default `Park()` target.
    ApPark::ApPark3
}

/// Deserialize `preferred_ap_park`, rejecting [`ApPark::ApPark0`].
/// "Current position" is not a slew target, so it cannot be the
/// preferred `Park()` destination — surfacing the misconfiguration at
/// config-load time rather than at the first `Park()` call.
fn deserialize_preferred_ap_park<'de, D>(deserializer: D) -> std::result::Result<ApPark, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let park = ApPark::deserialize(deserializer)?;
    if park == ApPark::ApPark0 {
        return Err(serde::de::Error::custom(
            "preferred_ap_park cannot be \"ap_park_0\" (\"current position\" is not a slew target)",
        ));
    }
    Ok(park)
}

/// Platform-dependent default serial port. Both values are placeholders the
/// operator replaces with the real device path: the driver restart-loops
/// until then, on Windows (`COM3`) exactly as on Unix (`/dev/ttyACM0`).
#[cfg(windows)]
const DEFAULT_SERIAL_PORT: &str = "COM3";
#[cfg(not(windows))]
const DEFAULT_SERIAL_PORT: &str = "/dev/ttyACM0";

impl Default for UsbConfig {
    fn default() -> Self {
        Self {
            port: DEFAULT_SERIAL_PORT.to_string(),
            baud_rate: default_baud_rate(),
            command_timeout: default_command_timeout(),
            polling_interval: default_polling_interval(),
        }
    }
}

impl Default for UdpConfig {
    fn default() -> Self {
        Self {
            // GTi AP-mode address (192.168.4.1) and a typical bind
            // address on the same /24. Constructed via `Ipv4Addr::new`
            // rather than parsing a string so `Default::default()`
            // can't panic on a typo — the compiler validates the
            // octet literals.
            address: IpAddr::V4(Ipv4Addr::new(192, 168, 4, 1)),
            port: default_udp_port(),
            bind_address: IpAddr::V4(Ipv4Addr::new(192, 168, 4, 2)),
            command_timeout: default_command_timeout(),
            polling_interval: default_polling_interval(),
        }
    }
}

impl Default for MountConfig {
    fn default() -> Self {
        Self {
            name: "Star Adventurer GTi".to_string(),
            // Empty by default: the spec-compliant UUIDv4 is minted on
            // first run by `rusty_photon_config::materialize_identity`
            // (called from `main.rs`) and persisted to the config file,
            // never overwritten thereafter. See the design doc's
            // §"Device identity (UniqueID)".
            unique_id: String::new(),
            description: "Sky-Watcher Star Adventurer GTi German Equatorial Mount".to_string(),
            enabled: true,
            site_latitude_deg: 0.0,
            site_longitude_deg: 0.0,
            site_elevation_m: 0.0,
            settle_after_slew: default_settle_after_slew(),
            tracking_rate: TrackingRateName::Sidereal,
            cw_exclusion_zone: CwExclusionZone::default(),
            tracking_guard_margin_hours: TrackingGuardMarginHours::default(),
            min_altitude_degrees: MinAltitudeDegrees::default(),
            park_ra_ticks: None,
            park_dec_ticks: None,
            flip_policy: FlipPolicy::default(),
            unpark_from_ap_position: default_unpark_from_ap_position(),
            preferred_ap_park: default_preferred_ap_park(),
        }
    }
}

/// Load a [`Config`] from a JSON file.
///
/// # Errors
///
/// Returns the I/O error if `path` cannot be read, or the JSON error if its
/// contents do not deserialize as a [`Config`] — an out-of-range value in
/// one of the validating newtypes included, with the field named.
pub fn load_config(
    path: &Path,
) -> std::result::Result<Config, Box<dyn std::error::Error + Send + Sync>> {
    let content = std::fs::read_to_string(path)?;
    // Validation is construct-time: the config newtypes
    // (`FlipRangeHours`, `CwExclusionZone`, `MinAltitudeDegrees`,
    // `TrackingGuardMarginHours`) reject out-of-range values during
    // deserialize, with the offending field named in the error — so a
    // bad config fails here at load rather than at slew/track time.
    let config: Config = serde_json::from_str(&content)?;
    Ok(config)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_match_the_design_doc() {
        let cfg = Config::default();
        assert_eq!(cfg.server.port, 11_117);
        assert_eq!(cfg.server.bind_address.to_string(), "0.0.0.0");
        assert_eq!(cfg.mount.name, "Star Adventurer GTi");
        // `unique_id` ships empty so the first-run materialize step mints
        // a persistent UUIDv4; the hardcoded literal was removed for
        // spec-compliant per-install uniqueness.
        assert_eq!(cfg.mount.unique_id, "");
        assert!(cfg.mount.enabled);
        assert_eq!(cfg.mount.tracking_rate, TrackingRateName::Sidereal);
        assert_eq!(cfg.mount.settle_after_slew, Duration::from_secs(2));
        assert!(matches!(cfg.transport, TransportConfig::Usb(_)));
    }

    #[test]
    fn usb_transport_default_uses_recommended_baud_rate() {
        let usb = UsbConfig::default();
        // Spec says 9600; in practice GTi USB also accepts 115200, which we
        // recommend (matches EQMOD docs).
        assert_eq!(usb.baud_rate, 115_200);
        assert_eq!(usb.command_timeout, Duration::from_secs(2));
        assert_eq!(usb.polling_interval, Duration::from_millis(200));
    }

    #[test]
    fn udp_transport_default_targets_the_ap_mode_address() {
        let udp = UdpConfig::default();
        assert_eq!(udp.address.to_string(), "192.168.4.1");
        assert_eq!(udp.port, 11_880);
        // bind_address is mandatory for UDP and must be on the 192.168.4.0/24
        // subnet when the mount is in AP mode.
        assert!(udp.bind_address.to_string().starts_with("192.168.4."));
    }

    #[test]
    fn port_label_usb_returns_serial_path_verbatim() {
        let cfg = TransportConfig::Usb(UsbConfig {
            port: "/dev/serial/by-id/usb-Some_Device-port0".into(),
            ..UsbConfig::default()
        });
        assert_eq!(cfg.port_label(), "/dev/serial/by-id/usb-Some_Device-port0");
    }

    #[test]
    fn port_label_udp_formats_address_and_port() {
        let cfg = TransportConfig::Udp(UdpConfig::default());
        // UdpConfig::default() targets the GTi's AP-mode address on the
        // documented port.
        assert_eq!(cfg.port_label(), "192.168.4.1:11880");
    }

    #[test]
    fn port_label_udp_brackets_ipv6_addresses() {
        // `SocketAddr`'s Display brackets v6 so the resulting label is
        // unambiguous (without brackets `fe80::1:11880` could read as
        // address `fe80::1` port `11880` *or* address `fe80::1:11880`
        // with no port). Matches the canonical operator-typing form.
        let cfg = TransportConfig::Udp(UdpConfig {
            address: "fe80::1".parse().expect("parse v6"),
            port: 11880,
            ..UdpConfig::default()
        });
        assert_eq!(cfg.port_label(), "[fe80::1]:11880");
    }

    #[test]
    fn config_round_trips_through_json() {
        let cfg = Config::default();
        let json = serde_json::to_string(&cfg).expect("serialise");
        let back: Config = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(back.server.port, cfg.server.port);
        assert_eq!(back.mount.name, cfg.mount.name);
    }

    #[test]
    fn transport_config_deserialises_usb_variant() {
        let json = r#"{
            "kind": "usb",
            "port": "/dev/ttyACM0",
            "baud_rate": 9600,
            "command_timeout": "1s",
            "polling_interval": "100ms"
        }"#;
        let t: TransportConfig = serde_json::from_str(json).expect("usb variant");
        match t {
            TransportConfig::Usb(usb) => {
                assert_eq!(usb.port, "/dev/ttyACM0");
                assert_eq!(usb.baud_rate, 9600);
                assert_eq!(usb.command_timeout, Duration::from_secs(1));
                assert_eq!(usb.polling_interval, Duration::from_millis(100));
            }
            other @ TransportConfig::Udp(_) => panic!("expected Usb, got {other:?}"),
        }
    }

    #[test]
    fn mount_config_park_ticks_default_to_none() {
        let cfg = MountConfig::default();
        assert_eq!(cfg.park_ra_ticks, None);
        assert_eq!(cfg.park_dec_ticks, None);
    }

    #[test]
    fn mount_config_deserialises_missing_park_ticks_as_none() {
        // Existing config files written before the SetPark feature
        // landed do not carry `park_ra_ticks` / `park_dec_ticks`; the
        // driver must read them as `None` rather than failing.
        let json = r#"{
            "name": "T",
            "unique_id": "t-001",
            "description": "T",
            "site_latitude_deg": 0.0,
            "site_longitude_deg": 0.0
        }"#;
        let m: MountConfig = serde_json::from_str(json).expect("deserialise");
        assert_eq!(m.park_ra_ticks, None);
        assert_eq!(m.park_dec_ticks, None);
    }

    #[test]
    fn mount_config_rejects_a_stale_dec_limits_key() {
        // dec_limits was replaced by min_altitude_degrees on 2026-07-01;
        // before deny_unknown_fields (#484) it was silently ignored. It
        // must now fail loudly at load, naming the field.
        let json = r#"{
            "name": "T",
            "unique_id": "t-001",
            "description": "T",
            "site_latitude_deg": 0.0,
            "site_longitude_deg": 0.0,
            "dec_limits": { "min_dec_deg": -20.0, "max_dec_deg": 80.0 }
        }"#;
        let err = serde_json::from_str::<MountConfig>(json).unwrap_err();
        assert!(err.to_string().contains("dec_limits"), "{err}");
    }

    #[test]
    fn mount_config_round_trips_park_ticks_through_json() {
        let cfg = MountConfig {
            park_ra_ticks: Some(8000),
            park_dec_ticks: Some(-3000),
            ..MountConfig::default()
        };
        let json = serde_json::to_string(&cfg).expect("serialise");
        let back: MountConfig = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(back.park_ra_ticks, Some(8000));
        assert_eq!(back.park_dec_ticks, Some(-3000));
    }

    #[test]
    fn unpark_from_ap_position_default_is_ap_park_0() {
        // The ship default is `ap_park_0` ("current position") — the
        // field is the operator's assertion about the physical world,
        // and this is the only value that is true when nothing was
        // asserted. A named-pose default would seed a confidently
        // wrong frame whenever the mount powered up elsewhere, and a
        // subsequent Park would slew to a fabricated position. The
        // unanchored frame parks in place instead; operators with a
        // repeatable setup declare their pose (typically ap_park_3).
        let cfg = MountConfig::default();
        assert_eq!(cfg.unpark_from_ap_position, ApPark::ApPark0);
    }

    #[test]
    fn preferred_ap_park_default_is_ap_park_3() {
        // The default `Park()` target is Sky-Watcher's stock power-up
        // pose along the polar axis.
        let cfg = MountConfig::default();
        assert_eq!(cfg.preferred_ap_park, ApPark::ApPark3);
    }

    #[test]
    fn ap_park_deserialises_from_snake_case() {
        for (json, expected) in [
            (r#""ap_park_0""#, ApPark::ApPark0),
            (r#""ap_park_1""#, ApPark::ApPark1),
            (r#""ap_park_2""#, ApPark::ApPark2),
            (r#""ap_park_3""#, ApPark::ApPark3),
            (r#""ap_park_4""#, ApPark::ApPark4),
            (r#""ap_park_5""#, ApPark::ApPark5),
        ] {
            let got: ApPark = serde_json::from_str(json).expect(json);
            assert_eq!(got, expected, "json input {json}");
        }
    }

    #[test]
    fn ap_park_as_str_and_from_str_are_the_canonical_token_mapping() {
        for park in [
            ApPark::ApPark0,
            ApPark::ApPark1,
            ApPark::ApPark2,
            ApPark::ApPark3,
            ApPark::ApPark4,
            ApPark::ApPark5,
        ] {
            // `as_str` ↔ `FromStr` round-trip.
            assert_eq!(park.as_str().parse::<ApPark>().unwrap(), park);
            // serde delegates to the same mapping, so the JSON string
            // form is exactly `as_str()` — the single source of truth.
            assert_eq!(
                serde_json::to_value(park).unwrap(),
                serde_json::Value::String(park.as_str().to_string()),
            );
        }
        assert_eq!(ApPark::ApPark3.as_str(), "ap_park_3");
        // An unrecognised token is a parse error that names the offender.
        let err = "ap_park_9".parse::<ApPark>().unwrap_err();
        assert!(err.to_string().contains("ap_park_9"), "{err}");
    }

    #[test]
    fn ap_park_tokens_keep_their_trailing_underscore() {
        // The six tokens are a deployed-config wire contract: every rig's
        // config JSON, the `Action` parameters and the `Action` return
        // values all use these exact strings, and the value feeds the
        // connect-time encoder seed and the `Park()` slew target. A
        // rename — e.g. dropping the underscore to `ap_park0` — silently
        // breaks every deployed config, so the literals are pinned here
        // in both directions.
        for (park, token) in [
            (ApPark::ApPark0, "ap_park_0"),
            (ApPark::ApPark1, "ap_park_1"),
            (ApPark::ApPark2, "ap_park_2"),
            (ApPark::ApPark3, "ap_park_3"),
            (ApPark::ApPark4, "ap_park_4"),
            (ApPark::ApPark5, "ap_park_5"),
        ] {
            assert_eq!(park.as_str(), token);
            assert_eq!(park.to_string(), token);
            assert_eq!(token.parse::<ApPark>().unwrap(), park);
        }
        // The underscore-less spellings are not accepted.
        for token in [
            "ap_park0", "ap_park1", "ap_park2", "ap_park3", "ap_park4", "ap_park5",
        ] {
            let err = token.parse::<ApPark>().unwrap_err();
            assert_eq!(err, ApParkParseError(token.to_string()), "token {token}");
        }
    }

    #[test]
    fn ap_park_0_has_no_codebase_encoder_mapping() {
        // "Current position" has no fixed encoder mapping; both
        // accessors return `None` so callers must handle the no-seed
        // case explicitly rather than seeding to a sentinel.
        for lat in [-45.0, 0.0, 32.7] {
            assert_eq!(ApPark::ApPark0.codebase_mech_ha_hours(lat), None);
            assert_eq!(ApPark::ApPark0.codebase_dec_encoder_degrees(lat), None);
        }
    }

    #[test]
    fn ap_park_1_matches_ap_table_both_hemispheres() {
        // AP Park 1: OTA on west side of mount, level, facing the
        // polar-side horizon (N: north horizon, S: south horizon).
        // Saddle is on the west end of the dec axis → mech_HA = 0
        // (canonical saddle-west position; dec axis east-west
        // horizontal). To reach the polar-side horizon from this
        // saddle position, the dec axis must rotate past the pole:
        // `dec_enc = ±(90 + |lat|)` (sign matches hemisphere).
        // Verified on hardware at lat 32.7°N (2026-05-17): the
        // encoder pose previously labelled Park 1 N (mech_HA=−12,
        // dec_enc=+57.3) physically corresponds to Park 5 N (saddle
        // east) — see commit fixing the swap.
        let n = 32.7_f64;
        let s = -33.0_f64;
        assert!(
            (ApPark::ApPark1.codebase_dec_encoder_degrees(n).unwrap() - 122.7).abs() < 1e-9,
            "Park 1 N dec_enc at 32.7°: got {:?}",
            ApPark::ApPark1.codebase_dec_encoder_degrees(n)
        );
        assert!(
            (ApPark::ApPark1.codebase_dec_encoder_degrees(s).unwrap() - (-123.0)).abs() < 1e-9,
            "Park 1 S dec_enc at −33°: got {:?}",
            ApPark::ApPark1.codebase_dec_encoder_degrees(s)
        );
        assert_eq!(ApPark::ApPark1.codebase_mech_ha_hours(n), Some(0.0));
        assert_eq!(ApPark::ApPark1.codebase_mech_ha_hours(s), Some(0.0));
    }

    #[test]
    fn ap_park_2_is_hemisphere_independent() {
        // Park 2: "RA axis vertical, Dec = 0", both hemispheres.
        // OTA at east-rising celestial equator → celestial HA = −6 h,
        // celestial dec = 0. Both hemispheres land on the natural side
        // (|dec_enc| = 0 ≤ 90), so encoder = celestial dec for both.
        for lat in [-89.0, -33.0, 0.0, 32.7, 89.0] {
            assert_eq!(
                ApPark::ApPark2.codebase_mech_ha_hours(lat),
                Some(-6.0),
                "Park 2 mech_HA at lat {lat}"
            );
            assert_eq!(
                ApPark::ApPark2.codebase_dec_encoder_degrees(lat),
                Some(0.0),
                "Park 2 dec at lat {lat}"
            );
        }
    }

    #[test]
    fn ap_park_3_visible_pole_inverts_with_hemisphere() {
        // Park 3 / Sky-Watcher home: OTA along polar axis at the
        // visible pole. Celestial dec = +90 (N) / −90 (S). Natural
        // side for both hemispheres → encoder = celestial dec.
        // Verified on hardware at lat 32.7°N (2026-05-15): mech_HA =
        // −6 h and dec_enc = +90 leaves the OTA pointing at the NCP.
        assert_eq!(
            ApPark::ApPark3.codebase_dec_encoder_degrees(32.7),
            Some(90.0)
        );
        assert_eq!(
            ApPark::ApPark3.codebase_dec_encoder_degrees(-33.0),
            Some(-90.0)
        );
        // Boundary: lat = 0 falls into the "north" arm via `>= 0`.
        assert_eq!(
            ApPark::ApPark3.codebase_dec_encoder_degrees(0.0),
            Some(90.0)
        );
        assert_eq!(ApPark::ApPark3.codebase_mech_ha_hours(32.7), Some(-6.0));
        assert_eq!(ApPark::ApPark3.codebase_mech_ha_hours(-33.0), Some(-6.0));
    }

    #[test]
    fn ap_park_4_dec_at_lat_32() {
        // AP Park 4: OTA on east side of mount, level, facing the
        // anti-polar horizon (N: south horizon, S: north horizon).
        // Saddle east → mech_HA = −12. AP celestial dec:
        //   N = (−90 + Lat) = −(90 − |lat|),
        //   S = (+90 + Lat) = +(90 − |lat|).
        // Past-pole encoding (180° around dec axis from natural-side
        // reference at this mech_HA): `dec_enc = ∓(90 + |lat|)`
        // (sign opposite hemisphere).
        assert!(
            (ApPark::ApPark4.codebase_dec_encoder_degrees(32.7).unwrap() - (-122.7)).abs() < 1e-9,
            "Park 4 N at 32.7°: got {:?}",
            ApPark::ApPark4.codebase_dec_encoder_degrees(32.7)
        );
        assert!(
            (ApPark::ApPark4.codebase_dec_encoder_degrees(-33.0).unwrap() - 123.0).abs() < 1e-9,
            "Park 4 S at −33°: got {:?}",
            ApPark::ApPark4.codebase_dec_encoder_degrees(-33.0)
        );
        assert_eq!(ApPark::ApPark4.codebase_mech_ha_hours(32.7), Some(-12.0));
        assert_eq!(ApPark::ApPark4.codebase_mech_ha_hours(-33.0), Some(-12.0));
    }

    #[test]
    fn ap_park_5_matches_ap_table_both_hemispheres() {
        // AP Park 5 (APCC / AP V2 driver only): OTA on east side of
        // mount, level, facing the polar-side horizon (N: north,
        // S: south). Saddle east → mech_HA = −12. Natural-side dec
        // rotation around the dec axis at this mech_HA reaches the
        // polar-side horizon: `dec_enc = ±(90 − |lat|)` (sign
        // matches hemisphere).
        // Hardware-verified at lat 32.7°N (2026-05-17): slewing to
        // celestial (LST+12, +57.28) lands at encoder (−cpr/2,
        // +461,815), saddle east, OTA at north horizon — matches AP
        // Park 5 N visually.
        assert!(
            (ApPark::ApPark5.codebase_dec_encoder_degrees(32.7).unwrap() - 57.3).abs() < 1e-9,
            "Park 5 N at 32.7°: got {:?}",
            ApPark::ApPark5.codebase_dec_encoder_degrees(32.7)
        );
        assert!(
            (ApPark::ApPark5.codebase_dec_encoder_degrees(-33.0).unwrap() - (-57.0)).abs() < 1e-9,
            "Park 5 S at −33°: got {:?}",
            ApPark::ApPark5.codebase_dec_encoder_degrees(-33.0)
        );
        assert_eq!(ApPark::ApPark5.codebase_mech_ha_hours(32.7), Some(-12.0));
        assert_eq!(ApPark::ApPark5.codebase_mech_ha_hours(-33.0), Some(-12.0));
    }

    #[test]
    fn ap_park_1_park5_share_celestial_target_opposite_pier() {
        // Park 1 (saddle west) and Park 5 (saddle east) point at the
        // same celestial coordinates (polar-side horizon) but on
        // opposite mechanical pier sides. The dec-encoder magnitudes
        // therefore differ by the "past the pole" offset:
        // `|natural| + |flipped| = |dec| + (180 − |dec|) = 180°`.
        for lat in [-45.0, -33.0, 32.7, 45.0] {
            let p1 = ApPark::ApPark1
                .codebase_dec_encoder_degrees(lat)
                .unwrap()
                .abs();
            let p5 = ApPark::ApPark5
                .codebase_dec_encoder_degrees(lat)
                .unwrap()
                .abs();
            assert!(
                (p1 + p5 - 180.0).abs() < 1e-9,
                "lat {lat}: |p1| {p1}, |p5| {p5}, sum {} (expected 180)",
                p1 + p5
            );
        }
    }

    #[test]
    fn ap_park_4_and_park5_share_pier_opposite_celestial_dec() {
        // Park 4 (OTA facing anti-polar horizon) and Park 5 (OTA
        // facing polar-side horizon) are on the same mechanical pier
        // (East, `mech_HA = −12`), and their celestial Decs are equal
        // in magnitude but opposite in sign. The Park 4 encoder uses
        // past-pole encoding (|dec_enc| > 90); Park 5 uses
        // natural-side encoding (|dec_enc| < 90). The magnitudes
        // therefore add to 180°, and the signs differ — i.e. their
        // encoder values are equal in magnitude with opposite signs
        // iff one is past-pole and the other is not.
        for lat in [-45.0, -33.0, 32.7, 45.0] {
            assert_eq!(ApPark::ApPark4.codebase_mech_ha_hours(lat), Some(-12.0));
            assert_eq!(ApPark::ApPark5.codebase_mech_ha_hours(lat), Some(-12.0));
            let p4_abs = ApPark::ApPark4
                .codebase_dec_encoder_degrees(lat)
                .unwrap()
                .abs();
            let p5_abs = ApPark::ApPark5
                .codebase_dec_encoder_degrees(lat)
                .unwrap()
                .abs();
            assert!(
                (p4_abs + p5_abs - 180.0).abs() < 1e-9,
                "lat {lat}: |p4| {p4_abs}, |p5| {p5_abs}"
            );
        }
    }

    #[test]
    fn unpark_from_ap_position_round_trips_through_mount_config_json() {
        // Every AP park variant round-trips through the required field.
        for pose in [
            ApPark::ApPark0,
            ApPark::ApPark1,
            ApPark::ApPark2,
            ApPark::ApPark3,
            ApPark::ApPark4,
            ApPark::ApPark5,
        ] {
            let cfg = MountConfig {
                unpark_from_ap_position: pose,
                ..MountConfig::default()
            };
            let json = serde_json::to_string(&cfg).expect("serialise");
            let back: MountConfig = serde_json::from_str(&json).expect("deserialise");
            assert_eq!(
                back.unpark_from_ap_position, pose,
                "round trip for {pose:?}"
            );
        }
    }

    #[test]
    fn preferred_ap_park_round_trips_each_slew_target() {
        // `ap_park_0` is excluded — it is rejected at deserialize time
        // (covered by `preferred_ap_park_rejects_ap_park_0`).
        for pose in [
            ApPark::ApPark1,
            ApPark::ApPark2,
            ApPark::ApPark3,
            ApPark::ApPark4,
            ApPark::ApPark5,
        ] {
            let cfg = MountConfig {
                preferred_ap_park: pose,
                ..MountConfig::default()
            };
            let json = serde_json::to_string(&cfg).expect("serialise");
            let back: MountConfig = serde_json::from_str(&json).expect("deserialise");
            assert_eq!(back.preferred_ap_park, pose, "round trip for {pose:?}");
        }
    }

    #[test]
    fn preferred_ap_park_rejects_ap_park_0() {
        // "Current position" is not a slew target — `preferred_ap_park:
        // "ap_park_0"` must fail at config-load time, not silently at
        // the first `Park()`.
        let json = r#"{
            "name": "T",
            "unique_id": "t-001",
            "description": "T",
            "site_latitude_deg": 0.0,
            "site_longitude_deg": 0.0,
            "preferred_ap_park": "ap_park_0"
        }"#;
        let err = serde_json::from_str::<MountConfig>(json).unwrap_err();
        assert!(
            err.to_string().contains("ap_park_0"),
            "error should name the rejected value: {err}"
        );
    }

    #[test]
    fn mount_config_deserialises_missing_unpark_fields_as_ship_defaults() {
        // Pre-rename config files (and any file omitting the new keys)
        // load cleanly: `unpark_from_ap_position` defaults to the
        // honest `ap_park_0` (unanchored — park in place until a
        // sync), `preferred_ap_park` to `ap_park_3`.
        let json = r#"{
            "name": "T",
            "unique_id": "t-001",
            "description": "T",
            "site_latitude_deg": 0.0,
            "site_longitude_deg": 0.0
        }"#;
        let m: MountConfig = serde_json::from_str(json).expect("deserialise");
        assert_eq!(m.unpark_from_ap_position, ApPark::ApPark0);
        assert_eq!(m.preferred_ap_park, ApPark::ApPark3);
    }

    #[test]
    fn flip_policy_default_is_disabled_with_half_hour_range() {
        // The shipped default disables every flip code path — a fresh
        // install must behave identically to pre-Phase-6 builds until
        // the operator explicitly opts in on a hardware-validated mount.
        let p = FlipPolicy::default();
        assert!(!p.enabled);
        assert!(
            (p.flip_range_hours.value() - 0.5).abs() < f64::EPSILON,
            "got {}",
            p.flip_range_hours.value()
        );
        assert!(!p.auto_flip_during_tracking);
        assert_eq!(p.auto_flip_at_meridian_offset_hours, 0.0);
    }

    #[test]
    fn mount_config_default_includes_disabled_flip_policy() {
        let cfg = MountConfig::default();
        assert!(!cfg.flip_policy.enabled);
    }

    #[test]
    fn mount_config_deserialises_missing_flip_policy_as_default() {
        // Existing config files written before Phase 6 do not carry
        // `flip_policy`; the driver must read them as
        // `FlipPolicy::default()` rather than failing.
        let json = r#"{
            "name": "T",
            "unique_id": "t-001",
            "description": "T",
            "site_latitude_deg": 0.0,
            "site_longitude_deg": 0.0
        }"#;
        let m: MountConfig = serde_json::from_str(json).expect("deserialise");
        assert!(!m.flip_policy.enabled);
        assert!((m.flip_policy.flip_range_hours.value() - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn mount_config_default_tracking_guard_margin_is_45s_of_drift() {
        // 0.05 h ≈ 45 s of sidereal drift — the shipped default the
        // design doc's §"Tracking-time safety guard" documents.
        let cfg = MountConfig::default();
        assert!(
            (cfg.tracking_guard_margin_hours.value() - 0.05).abs() < f64::EPSILON,
            "got {}",
            cfg.tracking_guard_margin_hours.value()
        );
    }

    #[test]
    fn mount_config_deserialises_missing_tracking_guard_margin_as_default() {
        // Configs written before the tracking-time safety guard landed
        // do not carry `tracking_guard_margin_hours`; the driver must
        // read them as the 0.05 h default rather than failing.
        let json = r#"{
            "name": "T",
            "unique_id": "t-001",
            "description": "T",
            "site_latitude_deg": 0.0,
            "site_longitude_deg": 0.0
        }"#;
        let m: MountConfig = serde_json::from_str(json).expect("deserialise");
        assert!((m.tracking_guard_margin_hours.value() - 0.05).abs() < f64::EPSILON);
    }

    // ---- Construct-time validation (replaces the retired validate()) ----

    #[test]
    fn default_config_round_trips_through_json() {
        // Defaults are in-range, so the typed fields construct without
        // error and survive a serialise → deserialise round trip.
        let json = serde_json::to_string(&MountConfig::default()).expect("serialise");
        serde_json::from_str::<MountConfig>(&json).expect("deserialise default");
    }

    #[test]
    fn cw_exclusion_zone_disabled_is_null_and_empty_interval() {
        let m: MountConfig = serde_json::from_str(
            r#"{"name":"t","unique_id":"t","description":"t",
                "site_latitude_deg":0.0,"site_longitude_deg":0.0,
                "cw_exclusion_zone":null}"#,
        )
        .expect("deserialise");
        assert_eq!(m.cw_exclusion_zone, CwExclusionZone::Disabled);
        // Disabled yields an empty interval (min > max) for the path checks.
        let (lo, hi) = m.cw_exclusion_zone.bounds();
        assert!(lo > hi, "disabled zone must be empty, got ({lo}, {hi})");
    }

    #[test]
    fn flip_range_hours_validates_at_construction() {
        for bad in [
            5.0,
            0.0,
            -0.1,
            MAX_FLIP_RANGE_HOURS + 1e-6,
            f64::INFINITY,
            f64::NAN,
        ] {
            assert!(
                FlipRangeHours::try_new(bad).is_err(),
                "{bad} should be rejected"
            );
        }
        assert!(FlipRangeHours::try_new(0.5).is_ok());
        assert!(FlipRangeHours::try_new(MAX_FLIP_RANGE_HOURS).is_ok());
        // The same rejection surfaces through serde with the field named.
        let err = serde_json::from_str::<FlipPolicy>(r#"{"flip_range_hours": 5.0}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("flip_range_hours"), "got {err}");
    }

    #[test]
    fn tracking_guard_margin_validates_at_construction() {
        for bad in [MAX_TRACKING_GUARD_MARGIN_HOURS + 0.1, -0.01, f64::NAN] {
            assert!(
                TrackingGuardMarginHours::try_new(bad).is_err(),
                "{bad} should be rejected"
            );
        }
        assert!(TrackingGuardMarginHours::try_new(0.0).is_ok());
        assert!(TrackingGuardMarginHours::try_new(MAX_TRACKING_GUARD_MARGIN_HOURS).is_ok());
        // The same rejection surfaces through serde (`try_from = "f64"`).
        assert!(serde_json::from_str::<TrackingGuardMarginHours>("-0.01").is_err());
    }

    #[test]
    fn cw_exclusion_zone_validates_active_bounds_at_deserialize() {
        // Out-of-folded-range and inverted active zones are rejected at
        // deserialize (disable via null instead).
        for bad in [
            r#"{"min_hours": 0.95, "max_hours": 20.0}"#,  // max > 12
            r#"{"min_hours": 11.05, "max_hours": 0.95}"#, // inverted
        ] {
            assert!(
                serde_json::from_str::<CwExclusionZone>(bad).is_err(),
                "{bad} should be rejected"
            );
        }
        let ok: CwExclusionZone =
            serde_json::from_str(r#"{"min_hours": 0.95, "max_hours": 11.05}"#).expect("valid");
        assert!(matches!(ok, CwExclusionZone::Active(_)));
        // The public checked constructor enforces the same invariant, so
        // programmatic callers can't build an invalid zone either.
        ActiveZone::try_new(0.95, 11.05).expect("in-range zone");
        assert!(ActiveZone::try_new(11.05, 0.95).is_err(), "inverted");
        assert!(ActiveZone::try_new(0.95, 20.0).is_err(), "max > 12");
    }

    #[test]
    fn min_altitude_degrees_validates_at_deserialize() {
        for bad in ["-90.1", "90.1", "NaN"] {
            assert!(
                serde_json::from_str::<MinAltitudeDegrees>(bad).is_err(),
                "{bad} should be rejected"
            );
        }
        for good in ["-90.0", "0.0", "5.0", "90.0"] {
            serde_json::from_str::<MinAltitudeDegrees>(good)
                .unwrap_or_else(|e| panic!("{good} should parse: {e}"));
        }
        // The public checked constructor enforces the same invariant.
        MinAltitudeDegrees::try_new(0.0).expect("geometric horizon");
        assert_eq!(MinAltitudeDegrees::try_new(-45.0).unwrap().value(), -45.0);
        assert!(MinAltitudeDegrees::try_new(-90.1).is_err(), "< -90");
        assert!(MinAltitudeDegrees::try_new(90.1).is_err(), "> 90");
        assert!(MinAltitudeDegrees::try_new(f64::NAN).is_err(), "NaN");
        assert!(MinAltitudeDegrees::try_new(f64::INFINITY).is_err(), "inf");
    }

    #[test]
    fn load_config_rejects_an_out_of_range_value() {
        // End-to-end: validation is wired into `load_config`, so a bad
        // value in the file fails the load rather than being silently
        // accepted. flip_range_hours = 5.0 is out of (0, 0.95].
        use std::io::Write;
        let cfg = Config {
            mount: MountConfig {
                flip_policy: FlipPolicy {
                    // Unchecked ctor builds a bad value that serialises to
                    // 5.0; load_config's deserialize is what rejects it.
                    flip_range_hours: FlipRangeHours::new(5.0),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Config::default()
        };
        let json = serde_json::to_string(&cfg).expect("serialise config");
        let mut f = tempfile::NamedTempFile::new().expect("temp file");
        f.write_all(json.as_bytes()).expect("write temp config");
        let err = load_config(f.path()).unwrap_err();
        assert!(
            err.to_string().contains("flip_range_hours"),
            "expected a flip_range_hours validation error, got: {err}"
        );
    }

    #[test]
    fn mount_config_round_trips_enabled_flip_policy_through_json() {
        let cfg = MountConfig {
            flip_policy: FlipPolicy {
                enabled: true,
                flip_range_hours: FlipRangeHours::new(0.7),
                auto_flip_during_tracking: true,
                auto_flip_at_meridian_offset_hours: 0.3,
            },
            ..MountConfig::default()
        };
        let json = serde_json::to_string(&cfg).expect("serialise");
        let back: MountConfig = serde_json::from_str(&json).expect("deserialise");
        assert!(back.flip_policy.enabled);
        assert!((back.flip_policy.flip_range_hours.value() - 0.7).abs() < f64::EPSILON);
        assert!(back.flip_policy.auto_flip_during_tracking);
        assert!((back.flip_policy.auto_flip_at_meridian_offset_hours - 0.3).abs() < f64::EPSILON);
    }

    #[test]
    fn flip_policy_deserialises_with_partial_fields() {
        // serde_default on each field means a partial block — only one
        // key present — fills the other in from the default.
        let json = r#"{"enabled": true}"#;
        let p: FlipPolicy = serde_json::from_str(json).expect("deserialise");
        assert!(p.enabled);
        assert!((p.flip_range_hours.value() - 0.5).abs() < f64::EPSILON);

        let json = r#"{"flip_range_hours": 0.25}"#;
        let p: FlipPolicy = serde_json::from_str(json).expect("deserialise");
        assert!(!p.enabled);
        assert!((p.flip_range_hours.value() - 0.25).abs() < f64::EPSILON);
        // Pre-auto-flip config blocks fill the auto-flip fields from
        // their defaults rather than failing.
        assert!(!p.auto_flip_during_tracking);
        assert_eq!(p.auto_flip_at_meridian_offset_hours, 0.0);
    }

    #[test]
    fn auto_flip_offset_validates_against_flip_range_at_deserialize() {
        // Cross-field rule: the offset must be finite and within
        // ±flip_range_hours, checked on the flip_policy block so a bad
        // pair fails at load with the field named.
        // (Non-finite offsets aren't expressible in JSON — serde_json
        // rejects them before try_from runs; the is_finite arm in
        // TryFrom is defense-in-depth for non-JSON construction paths.)
        for bad in [
            r#"{"flip_range_hours": 0.5, "auto_flip_at_meridian_offset_hours": 0.7}"#,
            r#"{"flip_range_hours": 0.5, "auto_flip_at_meridian_offset_hours": -0.7}"#,
        ] {
            let err = serde_json::from_str::<FlipPolicy>(bad)
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("auto_flip_at_meridian_offset_hours"),
                "{bad} should be rejected naming the field, got: {err}"
            );
        }
        // Both window edges and a negative in-window offset are valid.
        for good in [
            r#"{"flip_range_hours": 0.5, "auto_flip_at_meridian_offset_hours": 0.5}"#,
            r#"{"flip_range_hours": 0.5, "auto_flip_at_meridian_offset_hours": -0.5}"#,
            r#"{"auto_flip_at_meridian_offset_hours": -0.25}"#,
        ] {
            serde_json::from_str::<FlipPolicy>(good)
                .unwrap_or_else(|e| panic!("{good} should parse: {e}"));
        }
    }

    #[test]
    fn flip_policy_rejects_unknown_keys_at_deserialize() {
        // deny_unknown_fields lives on the wire struct now that
        // FlipPolicy deserialises via try_from; a typo must still fail
        // loudly at load.
        let err = serde_json::from_str::<FlipPolicy>(r#"{"auto_flip": true}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("auto_flip"), "got {err}");
    }

    #[test]
    fn transport_config_deserialises_udp_variant_with_bind_address() {
        let json = r#"{
            "kind": "udp",
            "address": "192.168.4.1",
            "port": 11880,
            "bind_address": "192.168.4.7",
            "command_timeout": "2s",
            "polling_interval": "200ms"
        }"#;
        let t: TransportConfig = serde_json::from_str(json).expect("udp variant");
        match t {
            TransportConfig::Udp(udp) => {
                assert_eq!(udp.address.to_string(), "192.168.4.1");
                assert_eq!(udp.bind_address.to_string(), "192.168.4.7");
            }
            other @ TransportConfig::Usb(_) => panic!("expected Udp, got {other:?}"),
        }
    }

    #[test]
    fn config_rejects_unknown_top_level_field() {
        let json = r#"{
            "transport": {"kind": "usb", "port": "/dev/ttyACM0"},
            "server": {"port": 11117},
            "mount": {
                "name": "T", "unique_id": "t-001", "description": "T",
                "site_latitude_deg": 0.0, "site_longitude_deg": 0.0
            },
            "plugins": []
        }"#;
        let err = serde_json::from_str::<Config>(json).unwrap_err();
        assert!(err.to_string().contains("plugins"), "{err}");
    }

    #[test]
    fn transport_config_usb_variant_rejects_unknown_field() {
        // The real operator-facing shape goes through the internally
        // tagged `TransportConfig` wrapper, not `UsbConfig` directly —
        // confirm deny_unknown_fields still applies once the "kind" tag
        // is stripped and the rest is handed to `UsbConfig::deserialize`.
        let json = r#"{"kind": "usb", "port": "/dev/ttyACM0", "flow_control": "none"}"#;
        let err = serde_json::from_str::<TransportConfig>(json).unwrap_err();
        assert!(err.to_string().contains("flow_control"), "{err}");
    }

    #[test]
    fn usb_config_rejects_unknown_field() {
        let json = r#"{"port": "/dev/ttyACM0", "flow_control": "none"}"#;
        let err = serde_json::from_str::<UsbConfig>(json).unwrap_err();
        assert!(err.to_string().contains("flow_control"), "{err}");
    }

    #[test]
    fn udp_config_rejects_unknown_field() {
        let json = r#"{"address": "192.168.4.1", "bind_address": "192.168.4.2", "mtu": 1500}"#;
        let err = serde_json::from_str::<UdpConfig>(json).unwrap_err();
        assert!(err.to_string().contains("mtu"), "{err}");
    }

    #[test]
    fn flip_policy_rejects_unknown_field() {
        let json = r#"{"enabled": true, "hysteresis_hours": 0.1}"#;
        let err = serde_json::from_str::<FlipPolicy>(json).unwrap_err();
        assert!(err.to_string().contains("hysteresis_hours"), "{err}");
    }

    #[test]
    fn cw_exclusion_zone_rejects_unknown_field() {
        let json = r#"{"min_hours": 0.95, "max_hours": 11.05, "margin_hours": 0.1}"#;
        let err = serde_json::from_str::<CwExclusionZone>(json).unwrap_err();
        assert!(err.to_string().contains("margin_hours"), "{err}");
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod doctor_toml_parity {
    use rusty_photon_server_config::doctor_toml::{parse, ServerClass};

    use super::Config;

    /// `pkg/doctor.toml` is this service's catalog entry for
    /// `rusty-photon-doctor` and must match the config defaults
    /// (docs/services/doctor.md §The derived catalog).
    #[test]
    fn pkg_doctor_toml_matches_config_defaults() {
        let meta = parse(include_str!("../pkg/doctor.toml")).unwrap();
        assert_eq!(meta.port, Config::default().server.port);
        assert_eq!(meta.class, ServerClass::Alpaca);

        // The declared serial pointer must resolve, in the serialized
        // default config, to the declared platform default.
        let serial = meta.serial.unwrap();
        let value = serde_json::to_value(Config::default()).unwrap();
        let port = value.pointer(&serial.pointer).unwrap().as_str().unwrap();
        #[cfg(unix)]
        assert_eq!(port, serial.default_unix);
        #[cfg(windows)]
        assert_eq!(port, serial.default_windows);

        // The serial checks apply only on the USB transport, and the
        // default config must be on that transport for the defaults above
        // to be meaningful.
        assert_eq!(
            serial.gate,
            Some(("/transport/kind".to_string(), "usb".to_string()))
        );
        assert_eq!(value.pointer("/transport/kind").unwrap(), "usb");

        // No USB identity declared yet (not measured on hardware).
        assert!(meta.usb.is_none());
    }
}

#[cfg(test)]
mod persisted_config_shape {
    use rusty_photon_server_config::unset::explicit_nulls;

    use super::Config;

    /// An unset optional field is spelled by its key's absence, never by an
    /// explicit `null` — see [`rusty_photon_server_config::unset`] for why.
    /// A field that grows without `skip_serializing_if` trips here rather
    /// than filling operators' config files with nulls.
    #[test]
    fn the_default_config_persists_no_explicit_nulls() {
        let persisted = serde_json::to_value(Config::default()).unwrap();
        assert_eq!(
            explicit_nulls(&persisted),
            Vec::<String>::new(),
            "unset optional fields must be omitted, not written as null: {persisted}"
        );
    }
}
