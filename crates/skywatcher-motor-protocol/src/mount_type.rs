//! Sky-Watcher mount-type identification.
//!
//! The `:e<axis>` (motor-board-version) reply is the only Sky-Watcher command
//! that meaningfully identifies the device on the wire. After the codec's
//! low-byte-first hex decode, its 24-bit payload packs the mount-type ID in
//! the **low** byte and the firmware version in the upper bytes — the `GTi`'s
//! wire reply `=03300C\r` (measured on the real mount) decodes to:
//!
//! ```text
//! 0x0C_30_03
//!   ^^         firmware, second wire byte (0x0C)
//!      ^^      firmware, first wire byte (0x30)
//!         ^^   mount-type ID (0x03 = EQ family, includes the Star Adventurer GTi)
//! ```
//!
//! The two firmware bytes keep their wire order in the probe table's
//! annotation (`fw 0x30/0x0C`); the low-byte-first decode moves the pair
//! above the type byte, reversing them within the integer.
//!
//! The same split INDI eqmod applies: `MountCode = MCVersion & 0xFF`
//! (`indi-eqmod/skywatcher.cpp`).
//!
//! [`MountType::from_motor_board_version`] is the whitelist gate used by the
//! `star-adventurer-gti` driver's connect handshake to refuse to talk to a
//! device that isn't a Sky-Watcher motor controller before any mount-specific
//! command (`:F`, `:a`, `:b`, `:g`, …) goes on the wire. See
//! [issue #254][issue] for the hardware session that motivated this.
//!
//! [issue]: https://github.com/rusty-photon/rusty-photon/issues/254

/// Sky-Watcher motor-controller mount-type families, keyed off the type byte
/// (low byte after decode) of the `:e` motor-board-version reply.
///
/// The byte values are documented in the Sky-Watcher motor-controller command
/// set and cross-checked against the INDI `indi-eqmod` reference driver.
/// Variants are named after the mount family rather than a specific model
/// because the firmware byte does not distinguish between e.g. an EQ3 and an
/// EQ5 — both report `0x03` / `0x02` from the same firmware build.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum MountType {
    /// `0x00` — EQ6 / EQ6 Pro German Equatorial.
    Eq6,
    /// `0x01` — HEQ5 / HEQ5 Pro German Equatorial.
    Heq5,
    /// `0x02` — EQ5 / EQ5 Pro German Equatorial.
    Eq5,
    /// `0x03` — EQ3 / EQ3-2 / Star Adventurer `GTi` German Equatorial.
    /// The Star Adventurer `GTi` reports as this family; see the hardware probe
    /// table in `docs/references/skywatcher-motor-controller-command-set.md`.
    Eq3,
    /// `0x04` — EQ8 German Equatorial.
    Eq8,
    /// `0x05` — AZ-EQ6 dual-mode (GEM + `AltAz`).
    AzEq6,
    /// `0x06` — AZ-EQ5 dual-mode (GEM + `AltAz`).
    AzEq5,
    /// `0x80` — Star Adventurer (the original single-axis tracker, not the `GTi`).
    StarAdventurer,
    /// `0x82` — AZ-GTi / Star Adventurer `GTi` (`AltAz` firmware variant).
    AzGti,
}

impl MountType {
    /// Extract the mount-type byte (**low** byte of the 24-bit value) from
    /// a `:e` reply and look it up against the whitelist.
    ///
    /// `version` is the [`crate::Response::U24`] payload of the
    /// [`crate::Command::InquireMotorBoardVersion`] reply, with the codec's
    /// low-byte-first hex decoding (see [`crate::codec::decode_u24`])
    /// already applied — i.e. for the `GTi` probe the wire reply
    /// `=03300C\r` decodes to `0x000C_3003`, which is what the caller
    /// passes in. The mount-type ID rides in the low byte of that value
    /// (`0x03`), the upper bytes are the firmware version — the same
    /// split INDI eqmod applies (`MountCode = MCVersion & 0xFF`,
    /// `indi-eqmod/skywatcher.cpp`) and the split verified against the
    /// real Star Adventurer `GTi` over USB.
    ///
    /// Returns `Ok(MountType)` when the low byte is in the whitelist;
    /// returns `Err(byte)` carrying the unrecognised mount-type byte
    /// otherwise so the driver can quote it in operator-facing diagnostics.
    pub const fn from_motor_board_version(version: u32) -> Result<Self, u8> {
        let [mount_id, _, _, _] = version.to_le_bytes();
        match mount_id {
            0x00 => Ok(Self::Eq6),
            0x01 => Ok(Self::Heq5),
            0x02 => Ok(Self::Eq5),
            0x03 => Ok(Self::Eq3),
            0x04 => Ok(Self::Eq8),
            0x05 => Ok(Self::AzEq6),
            0x06 => Ok(Self::AzEq5),
            0x80 => Ok(Self::StarAdventurer),
            0x82 => Ok(Self::AzGti),
            other => Err(other),
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn gti_probe_value_decodes_to_eq3_family() {
        // The Star Adventurer GTi probe: wire `=03300C\r` (measured on the
        // real mount over USB; also the probe table in
        // docs/references/skywatcher-motor-controller-command-set.md)
        // decodes low-byte-first to 0x000C_3003 — the value the driver
        // must accept on every connect.
        assert_eq!(
            MountType::from_motor_board_version(0x000C_3003).unwrap(),
            MountType::Eq3
        );
    }

    #[test]
    fn whitelisted_type_bytes_decode_to_named_variants() {
        for (version, expected) in [
            (0x0000_0000_u32, MountType::Eq6),
            (0x00FF_FF01, MountType::Heq5),
            (0x0000_0002, MountType::Eq5),
            (0x0000_0003, MountType::Eq3),
            (0x0000_0004, MountType::Eq8),
            (0x0000_0005, MountType::AzEq6),
            (0x0000_0006, MountType::AzEq5),
            (0x0000_0080, MountType::StarAdventurer),
            (0x0000_0082, MountType::AzGti),
        ] {
            assert_eq!(
                MountType::from_motor_board_version(version).unwrap(),
                expected,
                "version=0x{version:08X}"
            );
        }
    }

    #[test]
    fn firmware_bytes_do_not_affect_lookup() {
        // The high + mid bytes are firmware version and must not gate the
        // whitelist; only the low byte (mount-type ID) is consulted.
        for firmware_bytes in [0x0000_u32, 0xABCD, 0xFFFF, 0x0C30] {
            let v = (firmware_bytes << 8) | 0x03;
            assert_eq!(
                MountType::from_motor_board_version(v).unwrap(),
                MountType::Eq3,
                "version=0x{v:08X}"
            );
        }
    }

    #[test]
    fn unknown_mount_type_byte_surfaces_through_err() {
        // 0x07 is the gap between the EQ8 family (0x04..=0x06) and the AZ
        // family (0x80..) per the documented byte assignments. A reply with
        // this byte must be rejected so the driver doesn't proceed to issue
        // mount-specific commands against an unknown device.
        let err = MountType::from_motor_board_version(0x0000_0007).unwrap_err();
        assert_eq!(err, 0x07);

        // The QHY focuser misroute that motivated issue #254 returned data
        // that — were the bytes shuffled to look like a `:e` reply — would
        // decode to something unlike any Sky-Watcher mount-type ID. Pick a
        // plausible "wrong device" type byte and confirm it's rejected.
        let err = MountType::from_motor_board_version(0x0000_00FF).unwrap_err();
        assert_eq!(err, 0xFF);
    }

    #[test]
    fn only_the_type_byte_is_consulted_for_rejection() {
        // A version whose low byte is unknown must reject regardless of how
        // sensible the firmware bytes look. Symmetric to the
        // `firmware_bytes_do_not_affect_lookup` test for the accept path.
        assert_eq!(
            MountType::from_motor_board_version(0x000C_3099).unwrap_err(),
            0x99
        );
    }
}
