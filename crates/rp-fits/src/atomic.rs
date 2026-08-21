//! Atomic file-write helper.
//!
//! Lifts the stage→fsync→rename→fsync-parent dance from
//! `services/rp/src/persistence/fits.rs::write_fits_sync` into a
//! generic, reusable helper.
//!
//! Guarantees:
//! 1. Data is fsynced *before* the rename — a crash after rename
//!    cannot surface a renamed-but-zero-length file.
//! 2. The rename is atomic on POSIX (`rename(2)`) and on Windows
//!    (`MoveFileExW` with `MOVEFILE_REPLACE_EXISTING`, which Rust's
//!    `std::fs::rename` and `tempfile::NamedTempFile::persist` both
//!    use). Readers either see the old file or the new one, never
//!    a partial mix.
//! 3. The parent directory entry is fsynced *after* the rename
//!    (POSIX only) so the rename itself survives a crash.
//! 4. If anything before the rename fails, the staging file is
//!    removed by `tempfile::TempPath`'s Drop guard.

use std::io::Write;
use std::path::Path;

use crate::error::FitsError;

/// Write `body` to `path` atomically.
///
/// Convenience wrapper around [`write_atomic_with`].
///
/// # Errors
///
/// Returns [`FitsError::Io`] if `path` has no parent directory or the
/// temp-file write/rename fails.
pub fn write_atomic(path: &Path, body: &[u8]) -> Result<(), FitsError> {
    write_atomic_with(path, |w| {
        w.write_all(body)?;
        Ok(())
    })
}

/// Write to `path` atomically, building the body via the provided
/// closure. Use this form when the body is produced by a streaming
/// writer (e.g. [`crate::writer::write_i32_image`]).
///
/// # Errors
///
/// Returns [`FitsError::Io`] if `path` has no parent directory or the
/// temp-file write/rename fails, and propagates whatever `build`
/// returns.
pub fn write_atomic_with<F>(path: &Path, build: F) -> Result<(), FitsError>
where
    F: FnOnce(&mut dyn Write) -> Result<(), FitsError>,
{
    let parent = path.parent().ok_or_else(|| {
        // Not a FITS *parse* failure — surface as I/O so callers can
        // dispatch on `FitsError::Io` rather than guessing from the
        // error text. The `InvalidInput` ErrorKind matches what
        // `std::fs` would return for a similar precondition violation.
        FitsError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("atomic write target has no parent dir: {}", path.display()),
        ))
    })?;
    std::fs::create_dir_all(parent)?;

    // Stage in the destination directory so the rename is on the same
    // filesystem (rename across mounts is not atomic).
    let mut staged = tempfile::NamedTempFile::new_in(parent)?;

    build(staged.as_file_mut())?;

    // Flush the user-buffered Vec/wrapper, then fsync the file's bytes
    // before persisting. `as_file()` borrows the underlying File without
    // consuming the NamedTempFile so the Drop guard stays armed.
    staged.as_file_mut().flush()?;
    staged.as_file().sync_all()?;

    // Atomic rename. `persist` consumes the NamedTempFile and disarms
    // the Drop guard on success; on failure it returns a
    // `PersistError` carrying the original NamedTempFile so the guard
    // still cleans up the staging file.
    staged.persist(path).map_err(|e| FitsError::Io(e.error))?;

    // fsync the parent directory entry. Windows can't open a directory
    // as a regular file handle, so unix-only.
    #[cfg(unix)]
    {
        std::fs::File::open(parent)?.sync_all()?;
    }

    Ok(())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use std::path::Path;

    fn entry_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn writes_body_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.bin");
        write_atomic(&path, b"hello").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
    }

    #[test]
    fn successful_write_leaves_no_staging_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.bin");
        write_atomic(&path, b"hello").unwrap();
        assert_eq!(entry_names(dir.path()), vec!["data.bin"]);
    }

    #[test]
    fn overwrites_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.bin");
        write_atomic(&path, b"first").unwrap();
        write_atomic(&path, b"second").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
    }

    #[test]
    fn creates_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("dir").join("data.bin");
        write_atomic(&path, b"hi").unwrap();
        assert!(path.exists());
    }

    #[test]
    fn failed_persist_cleans_up_staging_file() {
        // Force `persist` to fail by parking a directory at the target
        // path — `rename(file, dir)` is rejected on both POSIX and
        // Windows.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blocked.bin");
        std::fs::create_dir(&path).unwrap();

        let err = write_atomic(&path, b"hi").unwrap_err();
        match err {
            FitsError::Io(_) => {}
            other => panic!("unexpected error: {other:?}"),
        }

        // Only the directory we put in the way should remain.
        assert_eq!(entry_names(dir.path()), vec!["blocked.bin"]);
    }

    /// Failure on a write to a path that already holds a successful
    /// prior write must leave the prior file's contents intact —
    /// atomic rename means the destination is either the old file or
    /// the new file, never torn or missing.
    #[cfg(unix)]
    #[test]
    fn failed_write_preserves_prior_file() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.bin");

        write_atomic(&path, b"first").unwrap();

        let original_perms = std::fs::metadata(dir.path()).unwrap().permissions();
        let mut readonly = original_perms.clone();
        readonly.set_mode(0o555);
        std::fs::set_permissions(dir.path(), readonly).unwrap();

        let err = write_atomic(&path, b"second").unwrap_err();

        // Restore so tempdir can clean up regardless of assertion outcomes.
        std::fs::set_permissions(dir.path(), original_perms).unwrap();

        match err {
            FitsError::Io(_) => {}
            other => panic!("unexpected error: {other:?}"),
        }

        assert_eq!(std::fs::read(&path).unwrap(), b"first");
    }

    #[test]
    fn write_atomic_with_streams_into_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("streamed.bin");
        write_atomic_with(&path, |w| {
            w.write_all(b"part1 ")?;
            w.write_all(b"part2")?;
            Ok(())
        })
        .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"part1 part2");
    }

    #[test]
    fn rejects_root_path_with_no_parent() {
        let err = write_atomic(Path::new("/"), b"x").unwrap_err();
        match err {
            FitsError::Io(io_err) => {
                assert_eq!(io_err.kind(), std::io::ErrorKind::InvalidInput);
            }
            other => panic!("expected Io(InvalidInput), got {other:?}"),
        }
    }
}
