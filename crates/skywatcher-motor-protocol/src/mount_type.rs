//! Sky-Watcher mount-type identification.
//!
//! The `:e<axis>` (motor-board-version) reply is the only Sky-Watcher command
//! that meaningfully identifies the device on the wire. Its three wire bytes
//! are the firmware major version, the firmware minor version and the mount
//! code, in that order: the `GTi`'s reply `=03300C\r` (measured on the real
//! mount) is firmware 3.48 (`0x03`, `0x30`), mount code `0x0C`. After the
//! codec's low-byte-first hex decode the value reads the other way round:
//!
//! ```text
//! 0x0C_30_03
//!   ^^         mount code (0x0C = Star Adventurer GTi)
//!      ^^      firmware minor (0x30 = 48)
//!         ^^   firmware major (0x03)
//! ```
//!
//! so the mount code is the **high** byte of the decoded value. INDI eqmod
//! reads it the same way: it swaps the decoded bytes into
//! `MCVersion = 0x03300C` and takes `MountCode = MCVersion & 0xFF`
//! (`indi-eqmod/skywatcher.cpp`), and its mount-code table is where the
//! whitelist below comes from.
//!
//! [`MountType::from_motor_board_version`] is the whitelist gate used by the
//! `star-adventurer-gti` driver's connect handshake to refuse to talk to a
//! device that isn't a Sky-Watcher motor controller before any mount-specific
//! command (`:F`, `:a`, `:b`, `:g`, …) goes on the wire. See
//! [issue #254][issue] for the hardware session that motivated this.
//!
//! [issue]: https://github.com/rusty-photon/rusty-photon/issues/254

/// Sky-Watcher motor-controller mount families, keyed off the mount code of
/// the `:e` motor-board-version reply.
///
/// The codes and names are INDI eqmod's (`Skywatcher::InquireBoardVersion`).
/// The whitelist admits the equatorial-capable Sky-Watcher controllers. It
/// leaves out the codes INDI itself refuses as unsupported — `0x80` GT,
/// `0x81` MF, `0x82` 114GT, `0x90` DOB — and INDI's `0xF0` custom board.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum MountType {
    /// `0x00` — EQ6.
    Eq6,
    /// `0x01` — HEQ5.
    Heq5,
    /// `0x02` — EQ5.
    Eq5,
    /// `0x03` — EQ3.
    Eq3,
    /// `0x04` — EQ8.
    Eq8,
    /// `0x05` — AZ-EQ6 dual-mode (GEM + `AltAz`).
    AzEq6,
    /// `0x06` — AZ-EQ5 dual-mode (GEM + `AltAz`).
    AzEq5,
    /// `0x0A` — Star Adventurer.
    StarAdventurer,
    /// `0x0C` — Star Adventurer `GTi`, the mount this driver is written for.
    StarAdventurerGti,
    /// `0x20` — EQ8-R Pro.
    Eq8RPro,
    /// `0x22` — AZ-EQ6 Pro.
    AzEq6Pro,
    /// `0x23` — EQ6-R Pro.
    Eq6RPro,
    /// `0x24` — EQ6 Pro.
    Eq6Pro,
    /// `0x25` — CQ350 Pro.
    Cq350Pro,
    /// `0x31` — EQ5 Pro.
    Eq5Pro,
    /// `0xA5` — AZ-GTi.
    AzGti,
}

impl MountType {
    /// Extract the mount code (**high** byte of the 24-bit value) from a
    /// `:e` reply and look it up against the whitelist.
    ///
    /// `version` is the [`crate::Response::U24`] payload of the
    /// [`crate::Command::InquireMotorBoardVersion`] reply, with the codec's
    /// low-byte-first hex decoding (see [`crate::codec::decode_u24`])
    /// already applied — i.e. for the `GTi` the wire reply `=03300C\r`
    /// decodes to `0x000C_3003`, which is what the caller passes in. The
    /// mount code rides in the high byte of that value (`0x0C`); the two
    /// bytes below it are the firmware version.
    ///
    /// Returns `Ok(MountType)` when the mount code is in the whitelist.
    ///
    /// # Errors
    ///
    /// Returns `Err(byte)` carrying the unrecognised mount code otherwise,
    /// so the driver can quote it in operator-facing diagnostics.
    pub const fn from_motor_board_version(version: u32) -> Result<Self, u8> {
        let [_firmware_major, _firmware_minor, mount_code, _] = version.to_le_bytes();
        match mount_code {
            0x00 => Ok(Self::Eq6),
            0x01 => Ok(Self::Heq5),
            0x02 => Ok(Self::Eq5),
            0x03 => Ok(Self::Eq3),
            0x04 => Ok(Self::Eq8),
            0x05 => Ok(Self::AzEq6),
            0x06 => Ok(Self::AzEq5),
            0x0A => Ok(Self::StarAdventurer),
            0x0C => Ok(Self::StarAdventurerGti),
            0x20 => Ok(Self::Eq8RPro),
            0x22 => Ok(Self::AzEq6Pro),
            0x23 => Ok(Self::Eq6RPro),
            0x24 => Ok(Self::Eq6Pro),
            0x25 => Ok(Self::Cq350Pro),
            0x31 => Ok(Self::Eq5Pro),
            0xA5 => Ok(Self::AzGti),
            other => Err(other),
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn gti_probe_value_decodes_to_star_adventurer_gti() {
        // The Star Adventurer GTi: wire `=03300C\r` (measured on the real
        // mount over USB; also the probe table in
        // docs/references/skywatcher-motor-controller-command-set.md)
        // decodes low-byte-first to 0x000C_3003 — the value the driver
        // must accept on every connect, and mount code 0x0C.
        assert_eq!(
            MountType::from_motor_board_version(0x000C_3003).unwrap(),
            MountType::StarAdventurerGti
        );
    }

    #[test]
    fn whitelisted_mount_codes_decode_to_named_variants() {
        for (code, expected) in [
            (0x00_u32, MountType::Eq6),
            (0x01, MountType::Heq5),
            (0x02, MountType::Eq5),
            (0x03, MountType::Eq3),
            (0x04, MountType::Eq8),
            (0x05, MountType::AzEq6),
            (0x06, MountType::AzEq5),
            (0x0A, MountType::StarAdventurer),
            (0x0C, MountType::StarAdventurerGti),
            (0x20, MountType::Eq8RPro),
            (0x22, MountType::AzEq6Pro),
            (0x23, MountType::Eq6RPro),
            (0x24, MountType::Eq6Pro),
            (0x25, MountType::Cq350Pro),
            (0x31, MountType::Eq5Pro),
            (0xA5, MountType::AzGti),
        ] {
            let version = code << 16;
            assert_eq!(
                MountType::from_motor_board_version(version).unwrap(),
                expected,
                "version=0x{version:08X}"
            );
        }
    }

    #[test]
    fn firmware_bytes_do_not_affect_lookup() {
        // The two low bytes are the firmware version and must not gate the
        // whitelist; only the high byte (mount code) is consulted.
        for firmware_bytes in [0x0000_u32, 0x3003, 0xABCD, 0xFFFF] {
            let v = (0x0C << 16) | firmware_bytes;
            assert_eq!(
                MountType::from_motor_board_version(v).unwrap(),
                MountType::StarAdventurerGti,
                "version=0x{v:08X}"
            );
        }
    }

    #[test]
    fn the_firmware_major_is_not_read_as_the_mount_code() {
        // Reading the low byte would take the GTi's firmware major (0x03)
        // for EQ3, and a firmware-4 board for EQ8. A version whose mount
        // code is unknown must be rejected whatever its firmware bytes say.
        assert_eq!(
            MountType::from_motor_board_version(0x0099_3003).unwrap_err(),
            0x99
        );
    }

    #[test]
    fn unsupported_and_unknown_mount_codes_surface_through_err() {
        // INDI refuses the alt-az-only GT / MF / 114GT / DOB boards; a
        // byte outside INDI's table is not a Sky-Watcher controller at all.
        // Either way the driver must stop before any mount-specific command.
        for code in [0x80_u8, 0x81, 0x82, 0x90, 0xF0, 0x07, 0xFF] {
            let version = u32::from(code) << 16;
            assert_eq!(
                MountType::from_motor_board_version(version).unwrap_err(),
                code,
                "version=0x{version:08X}"
            );
        }
    }
}
