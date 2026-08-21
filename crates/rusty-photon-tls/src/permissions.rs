use std::path::Path;

use tracing::debug;

use crate::error::Result;

/// Set file permissions to owner-only read/write (0600).
///
/// On Unix systems, this restricts access to the file owner.
/// On non-Unix systems, this is a no-op with a debug log.
///
/// # Errors
///
/// Returns the I/O error if setting the permissions fails (Unix only).
pub fn set_restricted_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(path, perms)?;
        debug!("Set permissions 0600 on {}", path.display());
    }
    #[cfg(not(unix))]
    {
        debug!(
            "Skipping permission restriction on non-Unix platform: {}",
            path.display()
        );
    }
    Ok(())
}

/// Refuse a symlink target (best-effort — checked before the open):
/// doctor runs as root on packaged hosts, so a pki-dir writer must not be
/// able to redirect a provisioning write to an arbitrary target.
///
/// # Errors
///
/// Returns a [`crate::error::TlsError`] if `path` is a symlink.
pub fn refuse_symlink(path: &Path) -> Result<()> {
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() {
            return Err(crate::error::TlsError::Other(format!(
                "refusing to write {}: it is a symlink",
                path.display()
            )));
        }
    }
    Ok(())
}

/// Create `path` restricted from the first instant (truncating any
/// previous content) and return the handle, refusing a symlink target.
///
/// On Unix the file is born 0600 via `OpenOptions::mode` — a chmod after
/// creation leaves a window where another process can open the umask-mode
/// file and hold the fd across the later restriction. `mode` only applies
/// when the file is newly created, so an existing file (a rotation
/// overwrite) is restricted explicitly too.
///
/// # Errors
///
/// Returns a [`crate::error::TlsError`] if `path` is a symlink, or the
/// I/O error if creating or restricting the file fails.
pub fn create_restricted(path: &Path) -> Result<std::fs::File> {
    refuse_symlink(path)?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    set_restricted_permissions(path)?;
    Ok(file)
}

/// Write `contents` to a file that is restricted before any byte reaches
/// disk — secret bytes never exist under the umask-default mode, and no
/// pre-write fd can outlive the restriction.
///
/// # Errors
///
/// Returns a [`crate::error::TlsError`] if `path` is a symlink, or the
/// I/O error if creating, restricting, or writing the file fails.
pub fn write_restricted(path: &Path, contents: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut file = create_restricted(path)?;
    file.write_all(contents)?;
    Ok(())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt as _;

    #[cfg(unix)]
    #[test]
    fn write_restricted_refuses_a_symlink_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::write(&target, "existing").unwrap();
        let link = dir.path().join("secret.key");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let err = write_restricted(&link, b"KEY").unwrap_err();
        assert!(err.to_string().contains("symlink"), "{err}");
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"existing",
            "the symlink target must be untouched"
        );
    }

    #[test]
    fn write_restricted_writes_the_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.key");
        write_restricted(&path, b"KEY-BYTES").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"KEY-BYTES");
        #[cfg(unix)]
        {
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "mode {mode:o}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn set_restricted_permissions_sets_0600() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("secret.key");
        std::fs::write(&file_path, "secret").unwrap();

        set_restricted_permissions(&file_path).unwrap();

        let meta = std::fs::metadata(&file_path).unwrap();
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "expected 0600 but got {mode:o}");
    }

    #[cfg(unix)]
    #[test]
    fn set_restricted_permissions_on_nonexistent_file_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("does-not-exist");
        let result = set_restricted_permissions(&file_path);
        assert!(result.is_err());
    }
}
