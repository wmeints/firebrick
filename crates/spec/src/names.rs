//! The free-text values of a spec that need a fixed form: the sandbox name, the image
//! reference and sizes such as `8Gi`.

use super::error::SizeError;
use std::fmt;

use serde::Deserializer;
use serde::de::{self, Visitor};
use std::result::Result;
use thiserror::Error;

/// Parses a memory or disk size such as `512 MiB`, `512Mi`, `4 GiB` or `4Gi` into mebibytes.
pub fn parse_size_mib(value: &str) -> Result<u32, SizeError> {
    let invalid = || SizeError::Invalid(value.to_string());

    let trimmed = value.trim();
    let digits_end = trimmed
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(trimmed.len());
    let (amount, unit) = trimmed.split_at(digits_end);

    let amount: u64 = amount.parse().map_err(|_| invalid())?;
    let multiplier: u64 = match unit.trim_start() {
        "Mi" | "MiB" => 1,
        "Gi" | "GiB" => 1024,
        _ => return Err(invalid()),
    };

    if amount == 0 {
        return Err(invalid());
    }

    amount
        .checked_mul(multiplier)
        .and_then(|mib| u32::try_from(mib).ok())
        .ok_or_else(|| SizeError::TooLarge(value.to_string()))
}

/// Deserializes a size, rejecting values that `parse_size_mib` can't read.
pub(super) fn deserialize_size<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<String, D::Error> {
    deserializer.deserialize_str(SizeVisitor)
}

/// Checks the size while the parser still points at the value, so a problem is
/// reported at the value's line and column instead of at the enclosing mapping.
struct SizeVisitor;

impl Visitor<'_> for SizeVisitor {
    type Value = String;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("a size such as `512 MiB` or `4Gi`")
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<String, E> {
        parse_size_mib(value).map_err(E::custom)?;

        Ok(value.to_string())
    }
}

/// The longest sandbox name microsandbox accepts, in bytes.
pub const MAX_SANDBOX_NAME_BYTES: usize = 128;

/// Checks a sandbox name with microsandbox's rule: 1 to [`MAX_SANDBOX_NAME_BYTES`] bytes, an
/// ASCII letter or digit first, then only ASCII letters, digits, `.`, `-` and `_`. A valid
/// name is always a single path component, so fbkd can name files after it.
pub fn validate_sandbox_name(name: &str) -> Result<(), InvalidSandboxName> {
    let starts_alphanumeric = name.starts_with(|c: char| c.is_ascii_alphanumeric());
    let charset_ok = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));

    if starts_alphanumeric && charset_ok && name.len() <= MAX_SANDBOX_NAME_BYTES {
        Ok(())
    } else {
        Err(InvalidSandboxName(name.to_string()))
    }
}

/// Deserializes a sandbox name, rejecting one [`validate_sandbox_name`] refuses.
pub(super) fn deserialize_sandbox_name<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<String, D::Error> {
    deserializer.deserialize_str(SandboxNameVisitor)
}

/// Checks the sandbox name while the parser still points at it, so a problem is reported at
/// the name's line and column.
struct SandboxNameVisitor;

impl Visitor<'_> for SandboxNameVisitor {
    type Value = String;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a sandbox name")
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<String, E> {
        validate_sandbox_name(value).map_err(E::custom)?;
        Ok(value.to_string())
    }
}

/// Checks an image reference with the OCI reference grammar that microsandbox parses it with:
/// an optional registry, a lowercase repository name, and an optional tag and digest. A valid
/// reference holds no whitespace or control characters, so it's safe to show in a message.
pub fn validate_image(image: &str) -> Result<(), InvalidImage> {
    oci_spec::distribution::Reference::try_from(image)
        .map(drop)
        .map_err(|source| InvalidImage {
            image: image.to_string(),
            source,
        })
}

/// Deserializes an optional image reference, rejecting one [`validate_image`] refuses.
pub(super) fn deserialize_image<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    deserializer.deserialize_option(ImageVisitor)
}

/// Checks the image reference while the parser still points at it, so a problem is reported
/// at the image's line and column.
struct ImageVisitor;

impl<'de> Visitor<'de> for ImageVisitor {
    type Value = Option<String>;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("an image reference")
    }

    fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_str(self)
    }

    /// An empty image means the default image, like a missing one.
    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        if value.is_empty() {
            return Ok(None);
        }

        validate_image(value).map_err(E::custom)?;
        Ok(Some(value.to_string()))
    }
}

/// A sandbox name that microsandbox refuses or that fbkd can't use as a file name.
#[derive(Error, Debug, Clone, PartialEq, Eq)]
#[error(
    "invalid sandbox name {0:?}: use 1 to 128 ASCII letters, digits, '.', '-' and '_', \
     starting with a letter or digit"
)]
pub struct InvalidSandboxName(pub String);

/// An image reference that microsandbox can't pull, with the reason the OCI grammar gives.
#[derive(Error, Debug, PartialEq, Eq)]
#[error(
    "invalid image {image:?}: {source}; use an OCI reference such as `ubuntu:24.04` or \
     `ghcr.io/org/image:tag`"
)]
pub struct InvalidImage {
    /// The refused reference.
    pub image: String,
    /// What is wrong with it. Its messages hold no input, so the error stays safe to show.
    #[source]
    pub source: oci_spec::distribution::ParseError,
}
