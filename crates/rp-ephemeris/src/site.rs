use std::sync::OnceLock;

use chrono::{DateTime, NaiveDate, Utc};
use tzf_rs::DefaultFinder;

/// Observer site: geographic latitude / longitude only. Elevation is
/// deferred to a future revision (see `docs/services/rp.md`
/// §"Planning and Ephemeris").
#[derive(Debug, Clone, Copy)]
pub struct Site {
    pub latitude_degrees: f64,
    pub longitude_degrees: f64,
    iana_tz: &'static str,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum SiteError {
    #[error("latitude_degrees must be in [-90, 90]; got {0}")]
    LatitudeOutOfRange(f64),
    #[error("longitude_degrees must be in [-180, 180]; got {0}")]
    LongitudeOutOfRange(f64),
}

static FINDER: OnceLock<DefaultFinder> = OnceLock::new();

fn finder() -> &'static DefaultFinder {
    FINDER.get_or_init(DefaultFinder::new)
}

impl Site {
    /// Construct a site, validating the lat/lon range and resolving
    /// the IANA timezone via `tzf-rs`. The finder is constructed on
    /// first call and held for the lifetime of the process — see the
    /// memory-footprint discussion in `docs/plans/rp-planning-tools.md`.
    ///
    /// # Errors
    ///
    /// Returns a [`SiteError`] if the latitude is outside ±90° or the
    /// longitude outside ±180°.
    pub fn new(latitude_degrees: f64, longitude_degrees: f64) -> Result<Self, SiteError> {
        if !(-90.0..=90.0).contains(&latitude_degrees) {
            return Err(SiteError::LatitudeOutOfRange(latitude_degrees));
        }
        if !(-180.0..=180.0).contains(&longitude_degrees) {
            return Err(SiteError::LongitudeOutOfRange(longitude_degrees));
        }
        // tzf-rs takes lng, lat (note order); the &str borrows from the
        // process-static finder, so its lifetime is effectively 'static.
        let iana_tz: &'static str = finder().get_tz_name(longitude_degrees, latitude_degrees);
        Ok(Self {
            latitude_degrees,
            longitude_degrees,
            iana_tz,
        })
    }

    /// IANA timezone name (e.g. `"America/Los_Angeles"`) derived from
    /// the site's lat/lon.
    #[must_use]
    pub const fn iana_timezone(&self) -> &'static str {
        self.iana_tz
    }

    /// The observing-night date `at` belongs to: the night rolls at
    /// local noon, so an instant before local noon belongs to the
    /// night that started the previous evening
    /// (rp-targets.md § Progress derivation's noon-rollover rule).
    /// `at` is converted to this site's IANA timezone (DST-aware) via
    /// `tzf-rs`'s lookup, then `night_date = (local − 12h).date()`.
    ///
    /// Falls back to UTC (logged at `debug!`) on the practically
    /// unreachable case where `tzf-rs`'s resolved zone name isn't one
    /// `chrono-tz` recognizes — both crates track the same IANA
    /// database, so a genuine mismatch would mean a stale build of one
    /// of them, not a real per-site failure mode. The frame still
    /// files correctly; only which calendar night it lands under could
    /// be off by less than a timezone's UTC offset.
    #[must_use]
    pub fn night_date(&self, at: DateTime<Utc>) -> NaiveDate {
        let tz: chrono_tz::Tz = self.iana_tz.parse().unwrap_or_else(|_| {
            tracing::debug!(
                iana_tz = self.iana_tz,
                "unrecognized by chrono-tz; falling back to UTC for night_date"
            );
            chrono_tz::UTC
        });
        // The checked subtraction can only fail within 12 h of chrono's
        // minimum representable instant; falling back to the unshifted
        // local date there matches this method's graceful-degradation
        // stance (see the UTC fallback above).
        let local = at.with_timezone(&tz);
        local
            .checked_sub_signed(chrono::Duration::hours(12))
            .unwrap_or(local)
            .date_naive()
    }
}

impl std::fmt::Display for Site {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "site lat={:.4}° lon={:.4}° tz={}",
            self.latitude_degrees, self.longitude_degrees, self.iana_tz
        )
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn rejects_out_of_range_latitude() {
        assert_eq!(
            Site::new(91.0, 0.0).unwrap_err(),
            SiteError::LatitudeOutOfRange(91.0)
        );
        assert_eq!(
            Site::new(-90.5, 0.0).unwrap_err(),
            SiteError::LatitudeOutOfRange(-90.5)
        );
    }

    #[test]
    fn rejects_out_of_range_longitude() {
        assert_eq!(
            Site::new(0.0, 181.0).unwrap_err(),
            SiteError::LongitudeOutOfRange(181.0)
        );
    }

    // These three assert the exact zone rather than a continent prefix.
    // `night_date` consumes the full name, so a dataset that moved any of
    // these points to a neighbouring zone would shift which calendar night
    // a frame files under — while a prefix assertion sailed through. All
    // three sit deep inside zones whose IANA names have been stable for
    // decades, so an exact mismatch means a bad dataset, not legitimate
    // upstream drift.

    #[test]
    fn seattle_resolves_to_los_angeles_zone() {
        let s = Site::new(47.6062, -122.3321).unwrap();
        assert_eq!(s.iana_timezone(), "America/Los_Angeles");
    }

    #[test]
    fn madrid_resolves_to_madrid_zone() {
        let s = Site::new(40.4168, -3.7038).unwrap();
        assert_eq!(s.iana_timezone(), "Europe/Madrid");
    }

    #[test]
    fn central_texas_resolves_to_chicago_zone() {
        // The observing site this project validates against. Its nearest
        // neighbours are an hour away in either direction, which is the
        // drift `night_date` would otherwise absorb silently.
        let s = Site::new(30.6280, -98.2690).unwrap();
        assert_eq!(s.iana_timezone(), "America/Chicago");
    }

    #[test]
    fn night_date_rolls_at_local_noon_not_midnight() {
        // Seattle is UTC-7 (PDT) in July: 2026-07-23T01:30:00Z is
        // 2026-07-22T18:30 local — well after local noon, so it's
        // already the night of the 22nd, not a rollover case. Use an
        // instant genuinely before local noon instead: 05:00 UTC is
        // 2026-07-21T22:00 local (still the 21st's evening) — belongs
        // to the night that started the 21st.
        let s = Site::new(47.6062, -122.3321).unwrap();
        let before_local_noon = DateTime::parse_from_rfc3339("2026-07-22T05:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            s.night_date(before_local_noon),
            NaiveDate::from_ymd_opt(2026, 7, 21).unwrap()
        );
    }

    #[test]
    fn night_date_after_local_noon_is_the_same_calendar_day() {
        let s = Site::new(47.6062, -122.3321).unwrap();
        // 2026-07-22T22:00:00Z is 2026-07-22T15:00 local (PDT) — after
        // local noon, so tonight's date is the 22nd.
        let after_local_noon = DateTime::parse_from_rfc3339("2026-07-22T22:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            s.night_date(after_local_noon),
            NaiveDate::from_ymd_opt(2026, 7, 22).unwrap()
        );
    }
}
