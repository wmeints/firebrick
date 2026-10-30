//! Reading and writing the editors' JSONC `settings.json` files.
//!
//! Shared by the editor modules that keep sandbox hosts in an editor's user settings. Edits go
//! through `jsonc-parser`'s CST, so comments and formatting stay as the user wrote them.

use super::atomic_file;
use jsonc_parser::ParseOptions;
use jsonc_parser::cst::CstObject;
use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

/// Environment variable that overrides the editors' config root, so the `vm-tests` can keep the
/// daemon they run away from the developer's own editor settings on every platform.
const CONFIG_ROOT_ENV: &str = "FIREBRICK_EDITOR_CONFIG_ROOT";

/// Parse options that accept what the editors accept in `settings.json`: JSON with comments and
/// trailing commas. Anything else is left unchanged rather than rewritten.
pub(crate) const PARSE_OPTIONS: ParseOptions = ParseOptions {
    allow_comments: true,
    allow_trailing_commas: true,
    allow_loose_object_property_names: false,
    allow_missing_commas: false,
    allow_single_quoted_strings: false,
    allow_hexadecimal_numbers: false,
    allow_unary_plus_numbers: false,
    allow_bare_decimal_point_numbers: false,
    allow_non_finite_numbers: false,
    allow_extended_string_escapes: false,
};

/// Serializes syncs, so concurrent requests can't interleave their reads and writes of a file.
static SYNC_LOCK: Mutex<()> = Mutex::new(());

/// Takes the lock that serializes the syncs of all editor settings.
pub(crate) fn lock() -> MutexGuard<'static, ()> {
    // A sync that panicked can't leave a half-written file behind, so a poisoned lock is fine.
    SYNC_LOCK.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Returns `$FIREBRICK_EDITOR_CONFIG_ROOT` when it's set, or `platform_root` otherwise.
pub(crate) fn config_root_or(platform_root: impl FnOnce() -> Option<PathBuf>) -> Option<PathBuf> {
    std::env::var_os(CONFIG_ROOT_ENV)
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(platform_root)
}

/// Returns `$XDG_CONFIG_HOME`, or `~/.config` when it isn't set.
#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) fn xdg_config_home() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(home_config)
}

/// Returns `~/.config`.
#[cfg(unix)]
pub(crate) fn home_config() -> Option<PathBuf> {
    // `$HOME` comes from the user's own environment on the host.
    // foxguard: ignore[rs/no-path-traversal]
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config"))
}

/// Returns `%APPDATA%`, where Windows applications keep the settings that roam with the user.
#[cfg(windows)]
pub(crate) fn app_data() -> Option<PathBuf> {
    std::env::var_os("APPDATA")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
}

/// Reads a settings file, treating a missing file as empty. A symlink to a missing file is an
/// error, so the sync doesn't replace the link with a regular file.
pub(crate) fn read(path: &Path) -> io::Result<String> {
    match fs::read_to_string(path) {
        Err(err) if err.kind() == ErrorKind::NotFound && !path.is_symlink() => Ok(String::new()),
        result => result,
    }
}

/// Returns how often `key` appears in the object.
pub(crate) fn key_count(object: &CstObject, key: &str) -> usize {
    object
        .properties()
        .iter()
        .filter(|prop| prop.decoded_name().as_deref() == Some(key))
        .count()
}

/// Replaces a settings file atomically, so the editor never reads a half-written file. A
/// symlinked settings file (dotfiles) stays a symlink.
pub(crate) fn write(path: &Path, content: &str) -> io::Result<()> {
    // 0o666 minus the umask is the mode `fs::write` creates a file with.
    atomic_file::write(path, content, 0o666)
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonc_parser::cst::CstRootNode;
    use tempfile::TempDir;

    #[test]
    fn reads_missing_file_as_empty() {
        let dir = TempDir::new().unwrap();

        assert_eq!(read(&dir.path().join("settings.json")).unwrap(), "");
    }

    #[test]
    fn counts_duplicate_keys() {
        let root = CstRootNode::parse("{\"a\": 1, \"b\": 2, \"a\": 3}", &PARSE_OPTIONS).unwrap();
        let object = root.object_value().unwrap();

        assert_eq!(key_count(&object, "a"), 2);
        assert_eq!(key_count(&object, "c"), 0);
    }
}
