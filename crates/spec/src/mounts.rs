//! The `mounts` of a spec: host directories and the absolute guest paths they're mounted at,
//! each guest path at most once.

use super::MISE_DOTFILES_PATH;
use std::collections::HashSet;
use std::fmt;

use serde::de::{self, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use std::result::Result;
use thiserror::Error;

/// A host directory mounted into the sandbox when it's created.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MountSpec {
    /// The host directory: absolute, starting with `~`, or relative to the spec file's
    /// directory.
    pub host: String,
    /// Absolute guest path the directory shows up at, normalized by [`guest_mount_path`].
    #[serde(deserialize_with = "deserialize_guest_path")]
    pub guest: String,
    /// Whether the directory is mounted read-only. Defaults to `false`.
    #[serde(default)]
    pub readonly: bool,
}

impl MountSpec {
    /// Returns the read-only mount of the `mise.config` directory at [`MISE_DOTFILES_PATH`],
    /// which the CLI approves and the daemon creates.
    pub fn mise_dotfiles(host: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            guest: MISE_DOTFILES_PATH.to_string(),
            readonly: true,
        }
    }
}

/// Deserializes the mounts, rejecting a guest path that is mounted more than once.
pub(super) fn deserialize_mounts<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Vec<MountSpec>>, D::Error> {
    deserializer.deserialize_option(MountsVisitor)
}

/// Checks the guest paths while the parser still points at the list, so a duplicate is
/// reported at the list's line and column instead of at the start of the document.
struct MountsVisitor;

impl<'de> Visitor<'de> for MountsVisitor {
    type Value = Option<Vec<MountSpec>>;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a list of mounts")
    }

    fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_seq(self)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut mounts = Vec::new();

        while let Some(mount) = seq.next_element::<MountSpec>()? {
            mounts.push(mount);
        }

        if let Some(guest) = duplicate_guest(&mounts) {
            return Err(de::Error::custom(format!(
                "guest path {guest} is mounted more than once"
            )));
        }

        Ok(Some(mounts))
    }
}

/// Returns the first guest path that more than one mount uses.
fn duplicate_guest(mounts: &[MountSpec]) -> Option<&str> {
    let mut guests = HashSet::new();

    mounts
        .iter()
        .map(|mount| mount.guest.as_str())
        .find(|guest| !guests.insert(*guest))
}

/// Deserializes a guest path, rejecting one that isn't absolute or is `/`.
fn deserialize_guest_path<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    deserializer.deserialize_str(GuestPathVisitor)
}

/// Checks the guest path while the parser still points at it, so a problem is reported at
/// the path's line and column.
struct GuestPathVisitor;

impl Visitor<'_> for GuestPathVisitor {
    type Value = String;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("an absolute guest path")
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<String, E> {
        guest_mount_path(value).map_err(E::custom)
    }
}

/// A guest mount path that microsandbox can't mount a directory at.
#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum InvalidGuestPath {
    #[error("guest path must be an absolute path other than /")]
    NotAbsolute,
    #[error("guest path must not contain ..")]
    ParentDir,
    #[error("guest path must not contain ':', ';' or ','")]
    Separator,
}

/// Checks a guest mount path and returns it normalized the way microsandbox stores it: without
/// `.` parts, repeated slashes or a trailing slash. Rejects a relative path, `/`, a path with
/// `..`, and a path with `:`, `;` or `,`, which microsandbox refuses.
pub fn guest_mount_path(guest: &str) -> Result<String, InvalidGuestPath> {
    if !guest.starts_with('/') {
        return Err(InvalidGuestPath::NotAbsolute);
    }

    if guest.contains([':', ';', ',']) {
        return Err(InvalidGuestPath::Separator);
    }

    let parts: Vec<&str> = guest
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect();

    if parts.contains(&"..") {
        return Err(InvalidGuestPath::ParentDir);
    }

    if parts.is_empty() {
        return Err(InvalidGuestPath::NotAbsolute);
    }

    Ok(format!("/{}", parts.join("/")))
}
