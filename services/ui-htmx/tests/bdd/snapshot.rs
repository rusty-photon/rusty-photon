//! `insta` byte-equivalence snapshots of the BFF's HTML output — Layer B of the
//! UI-testing plan ([`docs/plans/archive/ui-testing.md`] §5, obligation P2).
//!
//! The server's response bytes are the **cross-OS-comparable** artifact: htmx
//! swaps a fragment verbatim, so byte-identical output across OSes implies
//! identical browser behavior without a browser on every OS. One golden is
//! committed under `tests/snapshots/` and compared byte-for-byte on every leg.
//!
//! Two build-system facts shape this module:
//!
//! * **Snapshot location.** Under Cargo the goldens live next to the package
//!   (`$CARGO_MANIFEST_DIR/tests/snapshots`) and are written there by
//!   `cargo insta`. Under Bazel they reach the sandbox via the `bdd` target's
//!   `data` glob and are read through `$TEST_SRCDIR/$TEST_WORKSPACE`. The path is
//!   resolved at runtime ([`snapshot_dir`]) — it can't be a static string in
//!   `BUILD.bazel` — mirroring `services/ppba-driver/tests/translations.rs`.
//! * **Compare-only in CI/Bazel.** `INSTA_UPDATE=no` is forced on the Bazel
//!   target (Bazel does not propagate `CI`), and the sandbox is read-only, so a
//!   golden is never written there. Updates are Cargo-local: `cargo insta
//!   review` / `accept`, then commit.
//!
//! [`docs/plans/archive/ui-testing.md`]: ../../../../docs/plans/archive/ui-testing.md

use std::path::{Path, PathBuf};

/// Resolve the directory holding the committed `.snap` goldens at runtime.
///
/// Order: `$TEST_SRCDIR/$TEST_WORKSPACE/services/ui-htmx/tests/snapshots` (Bazel
/// runfiles, defaulting `TEST_WORKSPACE` to `_main`), then
/// `$CARGO_MANIFEST_DIR/tests/snapshots` **when that subdir exists** (Cargo — also
/// the write target for `cargo insta`), then a cwd-relative fallback (the package
/// dir under both runners). `TEST_SRCDIR` is checked **first** and is set only by
/// the Bazel test runner; the Cargo branch additionally requires the
/// `tests/snapshots` subdir to be present (not merely the manifest dir), so a
/// stray compile-time `CARGO_MANIFEST_DIR` that resolves to a dir *without* the
/// staged goldens can never shadow the runfiles path. See the module docs.
fn snapshot_dir() -> PathBuf {
    if let Ok(srcdir) = std::env::var("TEST_SRCDIR") {
        let workspace = std::env::var("TEST_WORKSPACE").unwrap_or_else(|_| "_main".into());
        return Path::new(&srcdir)
            .join(workspace)
            .join("services/ui-htmx/tests/snapshots");
    }
    // Cargo: CARGO_MANIFEST_DIR is the package source dir at runtime; the goldens
    // live (and `cargo insta` creates/writes them) under tests/snapshots. Gate on
    // the snapshots subdir itself — not just the manifest dir — so a stale
    // CARGO_MANIFEST_DIR pointing at a dir without the staged goldens falls through
    // (mirrors services/ppba-driver/tests/translations.rs, which checks i18n/).
    let snapshots = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots");
    if snapshots.is_dir() {
        return snapshots;
    }
    // First-time `cargo insta` generation (dir not yet created): under Cargo the
    // test cwd is the package root, so this resolves to the same tests/snapshots
    // that insta creates on accept.
    PathBuf::from("tests/snapshots")
}

/// Assert that `html` matches the committed golden named `name`, scrubbing the
/// one run/OS-varying token first so a single golden compares everywhere.
///
/// The dsd-fp2 driver binds `:0` (the OS assigns a free port), and the page
/// carries that effective `server.port` both in the read-only input and inside
/// the HTML-escaped hidden `__config` blob — so it is filtered to `<port>`
/// before compare. The serial port, baud rate, brightness, names, hints, and
/// `hx-*` wiring are all fixed inputs, so nothing else varies. (The
/// driver-unreachable error card is deliberately **not** snapshotted: its banner
/// embeds an OS-specific connection-refused string — "os error 111/61/10061" —
/// which is exactly the kind of output P2 cannot cover and P1's DOM check does.)
pub fn assert_html(name: &str, html: &str) {
    let mut settings = insta::Settings::clone_current();
    settings.set_snapshot_path(snapshot_dir());
    settings.set_prepend_module_to_snapshot(false);
    settings.set_omit_expression(true);
    // The read-only server.port input: `name="server.port" value="<digits>"`.
    settings.add_filter(r#"(name="server\.port" value=")\d+""#, r#"${1}<port>""#);
    // The same port inside the escaped __config blob: `&quot;port&quot;:<digits>`.
    // Only server.port is numeric here (serial.port is a quoted string, and
    // an unset discovery_port is absent from the blob rather than carrying a
    // value), so this can't clobber another value.
    settings.add_filter(r"(&quot;port&quot;:)\d+", r"${1}<port>");
    settings.bind(|| {
        insta::assert_snapshot!(name, html);
    });
}
