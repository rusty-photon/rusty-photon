//! The EAF SDK's own log, and the check that keeps it from killing the process.
//!
//! On Linux the EAF SDK (1.7.7) logs every process's calls through spdlog to a
//! directory fixed in the library, [`DIR`], creating whatever part of it is
//! missing at its first call. When it cannot open its file there, spdlog
//! throws a C++ exception that the SDK lets out through its C API, and the
//! process aborts: Rust cannot catch a foreign exception. Whoever first makes
//! the directory on a shared `/tmp` owns it, with the usual umask writable by
//! nobody else, so any other user that loads the SDK there dies at its first
//! call. Measured on the field rig with a directory a root run had left.
//!
//! [`ensure_writable`] runs before the first EAF SDK call in a process and
//! turns that abort into an [`Error::EafLog`]. It is Linux-only: the macOS
//! library logs elsewhere (`Library/Application Support/eaf_sdk/`), and how
//! the macOS and Windows libraries fail without their log has not been
//! measured.

use std::path::Path;

use crate::{Error, Result};

/// Where the EAF SDK writes its log on Linux, fixed in the library.
#[cfg(not(feature = "simulation"))]
const DIR: &str = "/tmp/zwo/log/eaf_sdk";

/// Whether the check has passed in this process. From the first SDK call on,
/// the SDK has its log open, so one pass is enough. A failed check is never
/// recorded: nothing called the SDK, and a later call checks again.
#[cfg(not(feature = "simulation"))]
static CHECKED: std::sync::Mutex<bool> = std::sync::Mutex::new(false);

/// Make sure the EAF SDK can write its log before anything calls it.
///
/// # Errors
/// Returns [`Error::EafLog`] when it cannot: the SDK would abort the process
/// at its first call.
#[cfg(not(feature = "simulation"))]
pub fn ensure_writable() -> Result<()> {
    let mut checked = CHECKED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !*checked {
        check(Path::new(DIR))?;
        *checked = true;
    }
    drop(checked);
    Ok(())
}

/// Whether this process can write a log in `dir`: make a file there and
/// remove it again. When `dir`, or part of it, does not exist yet, try the
/// nearest part that does, where the SDK would have to make the rest. Leaves
/// nothing behind and makes no directory.
fn check(dir: &Path) -> Result<()> {
    let nearest = dir.ancestors().find(|path| path.exists()).unwrap_or(dir);
    let probe = nearest.join(format!(".rusty-photon-check-{}", std::process::id()));
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    {
        Ok(file) => {
            drop(file);
            if let Err(error) = std::fs::remove_file(&probe) {
                tracing::debug!(probe = %probe.display(), %error, "could not remove the EAF SDK log check's file");
            }
            tracing::debug!(dir = %dir.display(), "the EAF SDK can write its log");
            Ok(())
        }
        Err(error) => Err(refusal(dir, nearest, &error)),
    }
}

/// The error for a log directory `blocked` refused to take a file in.
fn refusal(dir: &Path, blocked: &Path, error: &std::io::Error) -> Error {
    use std::os::unix::fs::MetadataExt;

    let owner = std::fs::metadata(blocked)
        .map(|metadata| format!(" (owner uid {})", metadata.uid()))
        .unwrap_or_default();
    Error::EafLog {
        dir: dir.display().to_string(),
        reason: format!(
            "{}{owner} refused a new file: {error}; make it writable for this user, or run as its owner",
            blocked.display()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn entries(dir: &Path) -> Vec<std::ffi::OsString> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect()
    }

    #[test]
    fn a_writable_log_directory_passes_and_keeps_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("zwo/log/eaf_sdk");
        std::fs::create_dir_all(&dir).unwrap();

        check(&dir).unwrap();

        assert_eq!(entries(&dir), Vec::<std::ffi::OsString>::new());
    }

    #[test]
    fn a_missing_log_directory_is_tried_where_the_sdk_would_make_it() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("zwo/log/eaf_sdk");

        check(&dir).unwrap();

        assert_eq!(entries(tmp.path()), Vec::<std::ffi::OsString>::new());
    }

    #[test]
    fn a_file_in_the_way_is_refused_and_named() {
        let tmp = tempfile::tempdir().unwrap();
        let in_the_way = tmp.path().join("zwo");
        std::fs::write(&in_the_way, "").unwrap();
        let dir = in_the_way.join("log/eaf_sdk");

        let error = check(&dir).unwrap_err();

        let Error::EafLog { dir: named, reason } = &error else {
            panic!("expected Error::EafLog, got {error:?}");
        };
        assert_eq!(named, &dir.display().to_string());
        assert!(
            reason.starts_with(&format!("{} (owner uid ", in_the_way.display())),
            "{reason}"
        );
    }

    #[test]
    fn a_log_directory_this_user_cannot_write_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("zwo/log/eaf_sdk");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        // A privileged user writes through the mode, and so would the SDK, so
        // the check must agree with what the filesystem lets this user do.
        let writable = std::fs::write(dir.join("direct"), "").is_ok();
        let _ = std::fs::remove_file(dir.join("direct"));

        let result = check(&dir);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        if writable {
            result.unwrap();
        } else {
            let error = result.unwrap_err();
            assert!(
                matches!(&error, Error::EafLog { reason, .. } if reason.contains("refused a new file")),
                "{error:?}"
            );
        }
        assert_eq!(entries(&dir), Vec::<std::ffi::OsString>::new());
    }
}
