//! Regression guard for the ERFA leap-second lazy-init data race.
//!
//! ERFA's `eraDatini` (`external/erfa/src/erfadatextra.c`) fills two
//! file-static globals — the table pointer `changes` and its length
//! `NDAT` — on the first `eraDat` call with no lock and no atomics.
//! Concurrent first calls used to segfault the process (~20-40% of cold
//! starts at high thread counts on a 16-core host): one thread could see
//! `NDAT > 0` without a happens-before edge on `changes` and index a
//! null/half-written pointer. `time_jds` now warms that table once under a
//! `std::sync::Once`, so driving many threads onto their first ephemeris
//! call simultaneously must complete without crashing and return finite,
//! in-range results.
//!
//! This lives in its own integration-test binary on purpose: the race is on
//! *process-global* state, so it only reproduces when these threads make
//! the process's first-ever ERFA calls. A sibling unit test that happened
//! to run first would warm the table and silently mask a regression.

// Curated test-scope allow list — documented in the root Cargo.toml [workspace.lints] block.
#![allow(
    clippy::needless_pass_by_ref_mut,
    clippy::needless_pass_by_value,
    clippy::unused_async,
    clippy::unused_async_trait_impl,
    clippy::used_underscore_binding,
    clippy::significant_drop_tightening,
    clippy::significant_drop_in_scrutinee,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::cast_possible_wrap,
    clippy::suboptimal_flops,
    clippy::too_many_lines,
    clippy::option_if_let_else,
    clippy::match_same_arms,
    clippy::float_cmp,
    clippy::similar_names,
    clippy::struct_excessive_bools
)]

use std::sync::{Arc, Barrier};
use std::thread;

use chrono::{TimeZone, Utc};
use rp_ephemeris::{Ephemeris, ErfarsEphemeris, IcrsCoord, Site};

#[test]
fn concurrent_first_ephemeris_calls_do_not_race_the_leap_second_table() {
    // Oversubscribe the cores to widen the window in which two threads are
    // simultaneously inside the (formerly unsynchronized) lazy init.
    const THREADS: usize = 64;

    let site = Site::new(47.6062, -122.3321).expect("valid site");
    let target = IcrsCoord {
        ra_hours: 5.5,
        dec_degrees: 22.0,
    };
    let time = Utc
        .with_ymd_and_hms(2026, 1, 1, 8, 0, 0)
        .single()
        .expect("valid instant");

    // The barrier releases every thread onto its first ERFA call at the
    // same instant, maximizing collision on the one-time table init.
    let barrier = Arc::new(Barrier::new(THREADS));
    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let eph = ErfarsEphemeris::new();
                barrier.wait();
                // The first call here is this thread's first `eraDat`; the
                // short burst keeps threads overlapping so a torn read of
                // the shared table surfaces rather than being papered over.
                let mut last_lst = f64::NAN;
                for _ in 0..64 {
                    last_lst = eph.sidereal_time(&site, time).lst_hours;
                    let _ = eph.alt_az(&site, target, time);
                    let _ = eph.sun_position(&site, time);
                    let _ = eph.moon_position(&site, time);
                }
                last_lst
            })
        })
        .collect();

    for handle in handles {
        let lst = handle.join().expect("worker thread panicked");
        // A racing thread that survived a torn read would compute garbage
        // rather than a valid sidereal time; assert the value is sane too,
        // not merely that the process stayed alive.
        assert!(
            lst.is_finite() && (0.0..24.0).contains(&lst),
            "sidereal time out of range under concurrency: {lst}"
        );
    }
}
