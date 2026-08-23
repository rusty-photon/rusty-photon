//! Can this identity use that path?
//!
//! A POSIX permission walk over gathered
//! [`PathFacts`](crate::facts::PathFacts) — owner class, else group class,
//! else other — the same order the kernel applies. This is a heuristic
//! (ACLs and capabilities are invisible to a `stat`), and every check
//! built on it says so in its detail text.

use crate::facts::PathFacts;

/// The identity a service runs as: its uid plus **every** gid it holds.
///
/// The gids are the primary from the user database, the supplementary
/// groups its unit confers (`SupplementaryGroups=`), and the account-level
/// memberships from the group database. The kernel grants the union of all
/// three, so callers judging access must populate `gids` with the union.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub uid: u32,
    pub gids: Vec<u32>,
}

impl Identity {
    /// Read + write — what opening a serial device needs.
    #[must_use]
    pub fn can_read_write(&self, facts: &PathFacts) -> bool {
        self.has(facts, 0o6)
    }

    /// Write + traverse — what creating files in a directory needs.
    #[must_use]
    pub fn can_write_dir(&self, facts: &PathFacts) -> bool {
        self.has(facts, 0o3)
    }

    /// POSIX class selection: exactly one class applies — a group-owner
    /// with a read-only group bit is denied even when the other bits are
    /// wide open.
    fn has(&self, facts: &PathFacts, bits: u32) -> bool {
        if self.uid == 0 {
            return true;
        }
        let class_shift = if self.uid == facts.uid {
            6
        } else if self.gids.contains(&facts.gid) {
            3
        } else {
            0
        };
        (facts.mode >> class_shift) & bits == bits
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::facts::PathKind;

    fn node(mode: u32, uid: u32, gid: u32) -> PathFacts {
        PathFacts {
            kind: PathKind::CharDevice,
            mode,
            uid,
            gid,
        }
    }

    #[test]
    fn test_the_canonical_serial_node_shape() {
        // crw-rw---- root:dialout — the udev default for tty devices.
        let facts = node(0o660, 0, 20);
        let in_dialout = Identity {
            uid: 990,
            gids: vec![990, 20],
        };
        let not_in_dialout = Identity {
            uid: 990,
            gids: vec![990],
        };
        assert!(in_dialout.can_read_write(&facts));
        assert!(!not_in_dialout.can_read_write(&facts));
    }

    #[test]
    fn test_exactly_one_class_applies() {
        // Group-owner with a read-only group bit: denied via the group
        // class even though the other class is rw.
        let facts = node(0o646, 0, 20);
        let group_member = Identity {
            uid: 990,
            gids: vec![20],
        };
        assert!(!group_member.can_read_write(&facts));
        let stranger = Identity {
            uid: 990,
            gids: vec![990],
        };
        assert!(stranger.can_read_write(&facts));
    }

    #[test]
    fn test_owner_and_root_paths() {
        let facts = node(0o600, 990, 990);
        assert!(Identity {
            uid: 990,
            gids: vec![]
        }
        .can_read_write(&facts));
        assert!(Identity {
            uid: 0,
            gids: vec![]
        }
        .can_read_write(&facts));
        assert!(!Identity {
            uid: 991,
            gids: vec![]
        }
        .can_read_write(&facts));
    }

    #[test]
    fn test_directory_write_needs_write_and_traverse() {
        let dir = PathFacts {
            kind: PathKind::Dir,
            mode: 0o755,
            uid: 0,
            gid: 0,
        };
        let user = Identity {
            uid: 990,
            gids: vec![990],
        };
        assert!(!user.can_write_dir(&dir), "r-x other class cannot write");
        let owned = PathFacts {
            kind: PathKind::Dir,
            mode: 0o700,
            uid: 990,
            gid: 990,
        };
        assert!(user.can_write_dir(&owned));
    }
}
