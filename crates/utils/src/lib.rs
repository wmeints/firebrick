//! File locations, local endpoints, naming rules and text helpers shared by the Firebrick CLI
//! and daemon.

pub mod ipc;
mod paths;
mod private_fs;
mod process;
mod text;
#[cfg(windows)]
mod win32;

pub use paths::{
    HOME_VAR, NO_CONFIG_DIR, NO_DATA_DIR, NO_LOG_DIR, approvals_path, clipboard_socket_dir,
    daemon_lock_path, log_dir, missing_dir, msb_home, netlog_socket_dir, network_log_dir,
    secrets_path, socket_path, ssh_dir,
};
#[cfg(unix)]
pub use private_fs::create_private_dir;
pub use private_fs::{make_private_dir, write_private};
pub use process::{keep_stdio_from_children, try_lock_daemon};
pub use text::{sanitize_label, strip_control};

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    /// Returns a lookup for an environment that holds only `vars`.
    pub(crate) fn env<'a>(vars: &'a [(&str, &str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |key| {
            vars.iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| OsString::from(value))
        }
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use crate::win32;
    use std::fs;
    use std::path::Path;

    /// Returns the access list of the path in Security Descriptor Definition Language, as
    /// `icacls /save` writes it, which doesn't depend on the display language.
    pub(crate) fn sddl(path: &Path) -> String {
        let out = tempfile::TempDir::new().unwrap();
        let saved = out.path().join("acl");
        let status = std::process::Command::new("icacls")
            .arg(path)
            .arg("/save")
            .arg(&saved)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        let bytes = fs::read(saved).unwrap();
        let wide: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes(*pair))
            .collect();
        String::from_utf16_lossy(&wide)
    }

    /// Asserts that the access list grants access to the user, the system (`SY`) and the
    /// administrators (`BA`) only, and to nobody else, such as the built-in users or everyone.
    pub(crate) fn assert_private(acl: &str) {
        assert_trustees(acl, &["BA", "SY"]);
    }

    /// Asserts that the access list in Security Descriptor Definition Language grants access to
    /// the user and the `others` only, in any order.
    pub(crate) fn assert_trustees(acl: &str, others: &[&str]) {
        let user = win32::Sid::current_user().unwrap().string().unwrap();
        // The trustee is the last field of each `(A;flags;rights;;;trustee)` entry.
        let mut trustees: Vec<&str> = acl
            .split('(')
            .skip(1)
            .filter_map(|entry| entry.split(')').next()?.rsplit(';').next())
            .collect();
        trustees.sort_unstable();

        let mut expected = others.to_vec();
        expected.push(user_trustee(&user));
        expected.sort_unstable();
        assert_eq!(trustees, expected, "{acl}");
    }

    /// Returns how SDDL names the user as a trustee: by SID, or as `LA` for the built-in local
    /// administrator account (RID 500), which CI runners use.
    fn user_trustee(user: &str) -> &str {
        if user.ends_with("-500") { "LA" } else { user }
    }

    /// Returns a temp directory outside the user's profile, in `C:\`, whose access list it
    /// inherits, like a directory that `XDG_DATA_HOME` points to outside the profile. The
    /// profile's temp directory is private already, so it would hide a missing restriction.
    pub(crate) fn outside_profile() -> tempfile::TempDir {
        tempfile::TempDir::new_in(r"C:\").unwrap()
    }
}
