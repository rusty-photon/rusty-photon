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

/// Whether this process can write a log in `dir`: make whatever part of `dir`
/// is missing, as the SDK would at its first call (mode 0755, as spdlog makes
/// it), then make a file in it and remove it again.
///
/// Making the directory here rather than leaving it to the SDK closes the gap
/// between this check and the SDK's first call: once this process owns `dir`,
/// no other user can make it first. Leaves no file behind.
fn check(dir: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    let probe = dir.join(format!(".rusty-photon-check-{}", std::process::id()));
    let made = std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(dir)
        .and_then(|()| {
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&probe)
        });
    match made {
        Ok(file) => {
            drop(file);
            if let Err(error) = std::fs::remove_file(&probe) {
                tracing::debug!(probe = %probe.display(), %error, "could not remove the EAF SDK log check's file");
            }
            tracing::debug!(dir = %dir.display(), "the EAF SDK can write its log");
            Ok(())
        }
        Err(error) => Err(refusal(dir, &error)),
    }
}

/// The error for a log directory `dir` this process could not make or write
/// in. What refused is the deepest part of `dir` that exists: `dir` itself, or
/// the parent that would not take the rest of it.
fn refusal(dir: &Path, error: &std::io::Error) -> Error {
    use std::os::unix::fs::MetadataExt;

    let blocked = dir.ancestors().find(|path| path.exists()).unwrap_or(dir);
    let owner = std::fs::metadata(blocked)
        .map(|metadata| format!(" (owner uid {})", metadata.uid()))
        .unwrap_or_default();
    Error::EafLog {
        dir: dir.display().to_string(),
        reason: format!(
            "{}{owner} refused the log: {error}; make it writable for this user, or run as its owner",
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
    fn a_missing_log_directory_is_made_as_the_sdk_would_make_it() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("zwo/log/eaf_sdk");

        check(&dir).unwrap();

        for made in [
            tmp.path().join("zwo"),
            tmp.path().join("zwo/log"),
            dir.clone(),
        ] {
            let mode = std::fs::metadata(&made).unwrap().permissions().mode();
            assert_eq!(
                mode & 0o777 & !0o755,
                0,
                "{} is mode {mode:o}",
                made.display()
            );
        }
        assert_eq!(entries(&dir), Vec::<std::ffi::OsString>::new());
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
            let prefix = format!("{} (owner uid ", dir.display());
            assert!(
                matches!(&error, Error::EafLog { reason, .. } if reason.starts_with(&prefix)),
                "{error:?}"
            );
        }
        assert_eq!(entries(&dir), Vec::<std::ffi::OsString>::new());
    }

    #[test]
    fn a_parent_this_user_cannot_write_is_refused_and_named() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path().join("zwo/log");
        std::fs::create_dir_all(&parent).unwrap();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o555)).unwrap();
        let writable = std::fs::create_dir(parent.join("direct")).is_ok();
        let _ = std::fs::remove_dir(parent.join("direct"));
        let dir = parent.join("eaf_sdk");

        let result = check(&dir);
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();

        if writable {
            result.unwrap();
        } else {
            let error = result.unwrap_err();
            let prefix = format!("{} (owner uid ", parent.display());
            assert!(
                matches!(&error, Error::EafLog { reason, .. } if reason.starts_with(&prefix)),
                "{error:?}"
            );
            assert!(!dir.exists());
        }
    }
}
