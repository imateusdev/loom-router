//! Atomic, permission-tight file writes for anything that may carry a
//! credential.
//!
//! Three call sites used to roll their own tmp+rename, and only one of them
//! tightened permissions - so `~/.codex/config.toml`, which carries the
//! local proxy token, was written world-readable (0644) while
//! `~/.loomrouter/config.json` was correctly 0600. A readable token defeats
//! the point of having one: `proxy::auth_gate` exists precisely so that no
//! other local process can spend the stored API keys.
//!
//! Keeping the write in one place also keeps the per-OS difference in one
//! place. Unix gets real modes; Windows relies on the file living under the
//! user's profile, because tightening a DACL needs an ACL edit this crate
//! does not do yet. That gap is documented rather than silently implied -
//! see `restrict_permissions`.

use std::io;
use std::path::{Path, PathBuf};

/// Mode for files and directories that may hold credentials.
#[cfg(unix)]
const PRIVATE_FILE_MODE: u32 = 0o600;
#[cfg(unix)]
const PRIVATE_DIR_MODE: u32 = 0o700;

/// Create `dir` and every missing parent, private to the current user.
///
/// An existing directory is tightened too: a config dir created by an older
/// build (or by hand) should not stay group-readable just because it was
/// already there.
pub fn create_dir_private(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(PRIVATE_DIR_MODE)
            .create(dir)?;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(PRIVATE_DIR_MODE));
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)?;
    }
    Ok(())
}

/// Best-effort tightening of an existing file to owner-only.
///
/// Unix sets the mode directly. On Windows this is deliberately a no-op:
/// restricting access means rewriting the DACL, which is out of scope here,
/// so the file's protection is whatever the user's profile directory gives
/// it. Callers must not treat a successful return as "the file is private"
/// on Windows.
pub fn restrict_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(PRIVATE_FILE_MODE));
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

/// `<path>.tmp`, as a sibling so the final rename never crosses a
/// filesystem boundary (a cross-device rename is not atomic and fails).
fn temp_sibling(path: &Path) -> PathBuf {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    PathBuf::from(tmp)
}

/// Create the temp file already private, then write into it.
///
/// The mode is set at creation rather than after the write, so the content
/// is never briefly world-readable - a window a later `set_permissions`
/// cannot close.
fn write_private_temp(tmp: &Path, contents: &[u8]) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(PRIVATE_FILE_MODE)
            .open(tmp)?;
        f.write_all(contents)?;
        f.sync_all()?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(tmp, contents)?;
    }
    Ok(())
}

/// Atomically replace `path` with `contents`, owner-only.
///
/// Writes a sibling temp file and renames it over the target, so a crash
/// mid-write leaves either the old file or a stale `.tmp` - never a
/// half-written config.
pub fn write_private(path: &Path, contents: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            create_dir_private(parent)?;
        }
    }
    let tmp = temp_sibling(path);
    write_private_temp(&tmp, contents)?;
    // Windows cannot rename over an existing destination.
    #[cfg(windows)]
    let _ = std::fs::remove_file(path);
    std::fs::rename(&tmp, path)?;
    // The rename preserves the temp file's mode on Unix; this is a backstop
    // for the non-Unix path and for pre-existing targets.
    restrict_permissions(path);
    Ok(())
}

/// Like [`write_private`], but first copies an existing target to
/// `<path>.bak`.
///
/// The backup is what makes the Windows remove+rename window survivable, and
/// it is the documented recovery path when a managed block gets mangled. It
/// inherits the source permissions via `copy`, then is tightened explicitly
/// so a backup of a secret is never more readable than the original.
pub fn write_private_with_backup(path: &Path, contents: &[u8]) -> io::Result<()> {
    if path.exists() {
        let mut bak = path.as_os_str().to_owned();
        bak.push(".bak");
        let bak = PathBuf::from(bak);
        std::fs::copy(path, &bak)?;
        restrict_permissions(&bak);
    }
    write_private(path, contents)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn write_private_creates_the_file_and_its_parent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("config.json");
        write_private(&path, b"{}").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{}");
    }

    #[test]
    fn write_private_replaces_existing_content_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        write_private(&path, b"old").unwrap();
        write_private(&path, b"new").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        // The temp file must not survive a successful write.
        assert!(!temp_sibling(&path).exists());
    }

    #[test]
    fn backup_keeps_the_previous_contents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write_private_with_backup(&path, b"first").unwrap();
        // No backup on first write: there was nothing to preserve.
        assert!(!dir.path().join("config.toml.bak").exists());
        write_private_with_backup(&path, b"second").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("config.toml.bak")).unwrap(),
            "first"
        );
    }

    #[cfg(unix)]
    #[test]
    fn secrets_are_written_owner_only() {
        // Regression: ~/.codex/config.toml carries the local proxy token and
        // used to land at 0644, so any local process could read it and spend
        // the stored API keys through the proxy.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write_private(&path, b"token = \"secret\"").unwrap();
        assert_eq!(mode_of(&path), 0o600, "secret file must be owner-only");
    }

    #[cfg(unix)]
    #[test]
    fn backups_are_never_looser_than_the_original() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write_private_with_backup(&path, b"first").unwrap();
        write_private_with_backup(&path, b"second").unwrap();
        assert_eq!(mode_of(&dir.path().join("config.toml.bak")), 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn created_directories_are_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("loomrouter");
        create_dir_private(&nested).unwrap();
        assert_eq!(mode_of(&nested), 0o700);
    }

    #[cfg(unix)]
    #[test]
    fn an_existing_loose_directory_is_tightened() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("loose");
        std::fs::create_dir_all(&nested).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&nested, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        create_dir_private(&nested).unwrap();
        assert_eq!(mode_of(&nested), 0o700);
    }
}
