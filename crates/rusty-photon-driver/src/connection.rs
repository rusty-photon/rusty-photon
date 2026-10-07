//! The rule for an ASCOM `Connected` write on a device that may have left the
//! bus.
//!
//! A driver whose device can disappear under an open session (a USB camera or
//! focuser losing its cable or its power) asks its vendor SDK, on a failure,
//! whether the device is still there. Asking is the SDK's business and stays in
//! the driver. What a `Connected` write then does with the answer is one rule
//! for every such driver, and it lives here: [`connected_transition`] takes the
//! facts as booleans and picks a [`ConnectedTransition`].

/// What an ASCOM `Connected` write has to do, given the session the device holds.
///
/// [`connected_transition`] picks one, and its documentation says why a lost
/// session has something to do in both directions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectedTransition {
    /// The device is already in the requested state with a live session.
    Nothing,
    /// Open a session.
    Connect,
    /// End a live session.
    Disconnect,
    /// End a session whose device has left the bus, and succeed whatever the
    /// vendor's close says about a device that is no longer there.
    Release,
    /// End a session whose device has left the bus, then open a fresh one, so
    /// the client gets either a working device or the connect failure, never
    /// the lost session back.
    ReleaseThenConnect,
}

/// The [`ConnectedTransition`] a write of `Connected = requested` calls for.
///
/// `held` is whether the device holds a session; `lost` is whether that
/// session's device has left the bus. A session that is not held cannot be
/// lost, so `lost` is ignored when `held` is false.
///
/// | `requested` | `held`  | `lost`  | transition                                                      |
/// |-------------|---------|---------|-----------------------------------------------------------------|
/// | `true`      | `false` | either  | [`Connect`](ConnectedTransition::Connect)                       |
/// | `true`      | `true`  | `false` | [`Nothing`](ConnectedTransition::Nothing)                       |
/// | `true`      | `true`  | `true`  | [`ReleaseThenConnect`](ConnectedTransition::ReleaseThenConnect) |
/// | `false`     | `false` | either  | [`Nothing`](ConnectedTransition::Nothing)                       |
/// | `false`     | `true`  | `false` | [`Disconnect`](ConnectedTransition::Disconnect)                 |
/// | `false`     | `true`  | `true`  | [`Release`](ConnectedTransition::Release)                       |
///
/// # Lost is not closed
///
/// A driver never closes a lost session on its own. A call (a capture, a move)
/// may still be inside the SDK on that session's handle, and closing the handle
/// under it risks a use-after-free in vendor code. The session ends when a
/// client ends it, and until then the device reads `Connected == false`. That
/// is why both writes have work to do on a lost session, and why neither
/// obvious check sees it: comparing the request with what `Connected` reads
/// finds nothing to do for `false`, and asking whether a session is held finds
/// nothing to do for `true`.
///
/// - `false` releases the session. The client asked for a disconnected
///   device, and that is what it gets, whatever the vendor's close says about
///   a device that is no longer there.
/// - `true` releases the session and opens a fresh one, so a client that sees
///   `Connected == false` and reconnects gets either a working device or the
///   connect failure, never the lost session back.
///
/// It is pure: the driver supplies the three facts, and finding out whether a
/// device is still there stays with the driver and its SDK.
///
/// ```
/// use rusty_photon_driver::{connected_transition, ConnectedTransition};
///
/// // A client reconnecting a device that left the bus gets a fresh session.
/// assert_eq!(connected_transition(true, true, true), ConnectedTransition::ReleaseThenConnect);
/// // With no session held, there is nothing that could have been lost.
/// assert_eq!(connected_transition(false, false, true), ConnectedTransition::Nothing);
/// ```
#[must_use]
pub const fn connected_transition(requested: bool, held: bool, lost: bool) -> ConnectedTransition {
    match (requested, held, lost) {
        (true, false, _) => ConnectedTransition::Connect,
        (true, true, true) => ConnectedTransition::ReleaseThenConnect,
        (true, true, false) | (false, false, _) => ConnectedTransition::Nothing,
        (false, true, false) => ConnectedTransition::Disconnect,
        (false, true, true) => ConnectedTransition::Release,
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn connecting_with_no_session_held_opens_one() {
        assert_eq!(
            connected_transition(true, false, false),
            ConnectedTransition::Connect
        );
    }

    #[test]
    fn connecting_a_live_session_does_nothing() {
        assert_eq!(
            connected_transition(true, true, false),
            ConnectedTransition::Nothing
        );
    }

    #[test]
    fn connecting_a_lost_session_releases_it_then_opens_a_fresh_one() {
        // The device reads disconnected, so the client reconnects; handing
        // the lost session back would answer that with a device that is gone.
        assert_eq!(
            connected_transition(true, true, true),
            ConnectedTransition::ReleaseThenConnect
        );
    }

    #[test]
    fn disconnecting_with_no_session_held_does_nothing() {
        assert_eq!(
            connected_transition(false, false, false),
            ConnectedTransition::Nothing
        );
    }

    #[test]
    fn disconnecting_a_live_session_ends_it() {
        assert_eq!(
            connected_transition(false, true, false),
            ConnectedTransition::Disconnect
        );
    }

    #[test]
    fn disconnecting_a_lost_session_releases_it() {
        // The device already reads disconnected, but it still holds the
        // session; only a client's write ends it.
        assert_eq!(
            connected_transition(false, true, true),
            ConnectedTransition::Release
        );
    }

    #[test]
    fn lost_is_ignored_when_no_session_is_held() {
        // A session that is not held cannot be lost, so these answer exactly
        // as their `lost == false` twins do.
        assert_eq!(
            connected_transition(true, false, true),
            ConnectedTransition::Connect
        );
        assert_eq!(
            connected_transition(false, false, true),
            ConnectedTransition::Nothing
        );
    }
}
