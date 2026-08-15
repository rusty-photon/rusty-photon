//! Property tests for the Falcon Rotator wire protocol.
//!
//! Round-trip: build a valid `FA` wire payload from random fields, parse it
//! back via `FalconStatus`'s `FromStr` impl, and assert every field
//! survives. The
//! degree field is generated as integer hundredths so the `{:.2}` write
//! format and the `f64` parse can compare exactly without epsilon.

#![allow(clippy::unwrap_used, clippy::expect_used)]
// Curated test-scope allow list — documented in the root Cargo.toml [workspace.lints] block.
#![allow(
    clippy::needless_pass_by_ref_mut,
    clippy::needless_pass_by_value,
    clippy::unused_async,
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

use pa_falcon_rotator::protocol::FalconStatus;
use pa_falcon_rotator::{MechanicalDegrees, Steps};
use proptest::prelude::*;

proptest! {
    #[test]
    fn full_status_wire_format_round_trips(
        // Signed range: the Falcon step counter goes negative CCW of the 0°
        // home (real hardware, firmware 1.5), so the round-trip must hold for
        // negative steps too.
        steps in -1_000_000i32..1_000_000,
        deg_cents in 0u32..36_000,
        is_moving in any::<bool>(),
        limit_detect in any::<bool>(),
        do_derotation in any::<bool>(),
        motor_reverse in any::<bool>(),
    ) {
        let deg = f64::from(deg_cents) / 100.0;
        let wire = format!(
            "FR_OK:{steps}:{deg:.2}:{}:{}:{}:{}",
            u8::from(is_moving),
            u8::from(limit_detect),
            u8::from(do_derotation),
            u8::from(motor_reverse),
        );
        let parsed = wire.parse::<FalconStatus>().unwrap();
        prop_assert_eq!(parsed, FalconStatus {
            position_steps: Steps(steps),
            position_deg: MechanicalDegrees::new(deg),
            is_moving,
            limit_detect,
            do_derotation,
            motor_reverse,
        });
    }
}
