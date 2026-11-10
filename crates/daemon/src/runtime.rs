//! Installation of the microsandbox runtime (`msb` and `libkrunfw`) the daemon runs sandboxes with.
//!
//! The runtime archive is embedded in `fbkd` at build time, so installing it never needs
//! network access.

use anyhow::{Context, Result, bail};
use microsandbox::config::GlobalConfig;
use microsandbox::setup::{InstallOptions, InstallSource, ResolvedRuntime, RuntimeOrigin, Version};
use sha2::{Digest as _, Sha256};

/// The SHA-256 of the `libkrunfw` embedded in `fbkd`, which `build.rs` records when the build
/// supplies the runtime through `MSB_EMBED_ARTIFACTS_DIR`. The Windows release embeds its own
/// `libkrunfw.dll` next to microsandbox's `msb.exe` that way (ADR 0061).
const EMBEDDED_LIBKRUNFW_SHA256: Option<&str> = option_env!("FBKD_EMBEDDED_LIBKRUNFW_SHA256");

/// Makes sure the microsandbox runtime matches the one embedded in `fbkd`.
///
/// A missing runtime is extracted from the embedded archive, and a runtime in the microsandbox
/// home with another version, or with another `libkrunfw` than the embedded one, is replaced by
/// it. A runtime configured outside the home with another version, or a partial installation, is
/// an error.
pub async fn ensure(config: &GlobalConfig) -> Result<ResolvedRuntime> {
    ensure_with(config, EMBEDDED_LIBKRUNFW_SHA256).await
}

/// Implements [`ensure`], comparing the library with `embedded_library`, the SHA-256 of the
/// embedded `libkrunfw` when it's known.
async fn ensure_with(
    config: &GlobalConfig,
    embedded_library: Option<&str>,
) -> Result<ResolvedRuntime> {
    let runtime = microsandbox::setup::ensure_runtime(config, embedded(false))
        .await
        .context("failed to ensure the microsandbox runtime is installed")?;
    let expected = expected_version()?;
    let installed = installed_version(&runtime)?;
    let current = installed.as_ref() == Some(&expected)
        && !library_differs(&runtime, embedded_library).await?;
    let runtime = match runtime.origin {
        _ if current => runtime,
        RuntimeOrigin::Home => upgrade(config, installed, &expected).await?,
        _ => bail!(
            "microsandbox runtime {} has version {}, but fbkd needs {expected}",
            runtime.msb_path.display(),
            describe(installed.as_ref())
        ),
    };
    tracing::info!(
        path = %runtime.msb_path.display(),
        origin = ?runtime.origin,
        "microsandbox runtime ready"
    );
    Ok(runtime)
}

/// Replaces the runtime in the microsandbox home with the embedded one.
async fn upgrade(
    config: &GlobalConfig,
    installed: Option<Version>,
    expected: &Version,
) -> Result<ResolvedRuntime> {
    tracing::info!(
        from = describe(installed.as_ref()),
        to = %expected,
        "upgrading microsandbox runtime"
    );
    microsandbox::setup::install_runtime(config, embedded(true))
        .await
        .context("failed to upgrade the microsandbox runtime")
}

/// Returns the options that install the embedded runtime archive.
fn embedded(force: bool) -> InstallOptions {
    InstallOptions {
        source: InstallSource::EmbeddedArchive,
        force,
        ..Default::default()
    }
}

/// Returns the runtime version embedded in `fbkd`.
fn expected_version() -> Result<Version> {
    let version = InstallOptions::default().version;
    Version::parse(&version)
        .with_context(|| format!("invalid embedded microsandbox runtime version {version}"))
}

/// Reads the version of the resolved `msb`; `None` means it predates version metadata.
///
/// An unreadable `msb` in the microsandbox home also counts as `None`, so it gets replaced.
fn installed_version(runtime: &ResolvedRuntime) -> Result<Option<Version>> {
    match microsandbox::setup::resolve_runtime_version(&runtime.msb_path) {
        Ok(version) => Ok(version),
        Err(error) if runtime.origin == RuntimeOrigin::Home => {
            tracing::warn!(%error, "failed to read the microsandbox runtime version");
            Ok(None)
        }
        Err(error) => Err(error).with_context(|| {
            format!(
                "failed to read the version of {}",
                runtime.msb_path.display()
            )
        }),
    }
}

/// Returns whether a runtime in the microsandbox home has another `libkrunfw` than the embedded
/// one, so a home that an earlier `fbkd` installed with the same `msb` gets the new library.
///
/// A library configured outside the home is the user's choice, such as a kernel under test, and
/// an unreadable library in the home counts as different, so it gets replaced.
async fn library_differs(runtime: &ResolvedRuntime, embedded: Option<&str>) -> Result<bool> {
    let Some(embedded) = embedded else {
        return Ok(false);
    };
    if runtime.origin != RuntimeOrigin::Home {
        return Ok(false);
    }
    let path = runtime.libkrunfw_path.clone();
    let digest = tokio::task::spawn_blocking(move || sha256_of(&path))
        .await
        .context("failed to hash the installed libkrunfw")?;
    match digest {
        Ok(digest) if digest == embedded => Ok(false),
        Ok(_) => {
            tracing::info!(path = %runtime.libkrunfw_path.display(), "installed libkrunfw differs from the embedded one");
            Ok(true)
        }
        Err(error) => {
            tracing::warn!(%error, "failed to read the installed libkrunfw");
            Ok(true)
        }
    }
}

/// Returns the SHA-256 of a file as lowercase hex, the format `build.rs` records.
fn sha256_of(path: &std::path::Path) -> std::io::Result<String> {
    Ok(format!("{:x}", Sha256::digest(std::fs::read(path)?)))
}

/// Formats an installed version for messages.
fn describe(version: Option<&Version>) -> String {
    version.map_or_else(|| "unknown".to_owned(), Version::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes a copy of the test binary, an executable without an `msb` version section.
    fn replace_with_unversioned_binary(path: &std::path::Path) {
        std::fs::copy(std::env::current_exe().unwrap(), path).unwrap();
    }

    fn modified(path: &std::path::Path) -> std::time::SystemTime {
        std::fs::metadata(path).unwrap().modified().unwrap()
    }

    fn config_in(home: &tempfile::TempDir) -> GlobalConfig {
        GlobalConfig {
            home: Some(home.path().to_path_buf()),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn installs_embedded_runtime_when_absent() {
        let home = tempfile::tempdir().unwrap();

        let runtime = ensure(&config_in(&home)).await.unwrap();

        assert_eq!(runtime.origin, RuntimeOrigin::Installed);
        assert!(runtime.msb_path.starts_with(home.path()));
        assert!(runtime.msb_path.is_file());
        assert!(runtime.libkrunfw_path.is_file());
    }

    #[tokio::test]
    async fn reuses_installed_runtime() {
        let home = tempfile::tempdir().unwrap();
        let config = config_in(&home);
        let installed = ensure(&config).await.unwrap();

        let runtime = ensure(&config).await.unwrap();

        assert_eq!(runtime.origin, RuntimeOrigin::Home);
        assert_eq!(runtime.msb_path, installed.msb_path);
        assert_eq!(modified(&runtime.msb_path), modified(&installed.msb_path));
    }

    #[tokio::test]
    async fn upgrades_outdated_home_runtime() {
        let home = tempfile::tempdir().unwrap();
        let config = config_in(&home);
        let installed = ensure(&config).await.unwrap();
        replace_with_unversioned_binary(&installed.msb_path);

        let runtime = ensure(&config).await.unwrap();

        assert_eq!(runtime.msb_path, installed.msb_path);
        let version = microsandbox::setup::resolve_runtime_version(&runtime.msb_path).unwrap();
        assert_eq!(version, Some(expected_version().unwrap()));
    }

    #[tokio::test]
    async fn replaces_unreadable_home_runtime() {
        let home = tempfile::tempdir().unwrap();
        let config = config_in(&home);
        let installed = ensure(&config).await.unwrap();
        std::fs::write(&installed.msb_path, "#!/bin/sh\n").unwrap();

        let runtime = ensure(&config).await.unwrap();

        let version = microsandbox::setup::resolve_runtime_version(&runtime.msb_path).unwrap();
        assert_eq!(version, Some(expected_version().unwrap()));
    }

    #[tokio::test]
    async fn accepts_current_explicit_runtime() {
        let source = tempfile::tempdir().unwrap();
        let installed = ensure(&config_in(&source)).await.unwrap();
        let home = tempfile::tempdir().unwrap();
        let mut config = config_in(&home);
        config.paths.msb = Some(installed.msb_path.clone());
        config.paths.libkrunfw = Some(installed.libkrunfw_path);

        let runtime = ensure(&config).await.unwrap();

        assert_eq!(runtime.origin, RuntimeOrigin::Configuration);
        assert_eq!(runtime.msb_path, installed.msb_path);
        assert!(!home.path().join("bin").exists());
    }

    #[tokio::test]
    async fn rejects_outdated_explicit_runtime() {
        let home = tempfile::tempdir().unwrap();
        let explicit = tempfile::tempdir().unwrap();
        let msb = explicit.path().join("msb");
        let libkrunfw = explicit.path().join("libkrunfw.so");
        replace_with_unversioned_binary(&msb);
        std::fs::write(&libkrunfw, b"").unwrap();
        let mut config = config_in(&home);
        config.paths.msb = Some(msb.clone());
        config.paths.libkrunfw = Some(libkrunfw);

        let error = ensure(&config).await.unwrap_err();

        assert!(format!("{error:#}").contains("fbkd needs"), "{error:#}");
        assert!(!home.path().join("bin").exists());
    }

    #[tokio::test]
    async fn rejects_partial_runtime() {
        let home = tempfile::tempdir().unwrap();
        let config = config_in(&home);
        let installed = ensure(&config).await.unwrap();
        std::fs::remove_file(&installed.libkrunfw_path).unwrap();

        let error = ensure(&config).await.unwrap_err();

        assert!(format!("{error:#}").contains("expected both"), "{error:#}");
    }

    #[tokio::test]
    async fn replaces_home_runtime_with_another_libkrunfw() {
        let home = tempfile::tempdir().unwrap();
        let config = config_in(&home);
        let installed = ensure(&config).await.unwrap();
        let embedded = sha256_of(&installed.libkrunfw_path).unwrap();
        std::fs::write(&installed.libkrunfw_path, b"a stock libkrunfw").unwrap();

        let runtime = ensure_with(&config, Some(&embedded)).await.unwrap();

        assert_eq!(runtime.libkrunfw_path, installed.libkrunfw_path);
        assert_eq!(sha256_of(&runtime.libkrunfw_path).unwrap(), embedded);
    }

    #[tokio::test]
    async fn keeps_home_runtime_with_embedded_libkrunfw() {
        let home = tempfile::tempdir().unwrap();
        let config = config_in(&home);
        let installed = ensure(&config).await.unwrap();
        let embedded = sha256_of(&installed.libkrunfw_path).unwrap();

        let runtime = ensure_with(&config, Some(&embedded)).await.unwrap();

        assert_eq!(runtime.origin, RuntimeOrigin::Home);
        assert_eq!(
            modified(&runtime.libkrunfw_path),
            modified(&installed.libkrunfw_path)
        );
    }

    #[tokio::test]
    async fn keeps_libkrunfw_of_explicit_runtime() {
        let source = tempfile::tempdir().unwrap();
        let installed = ensure(&config_in(&source)).await.unwrap();
        std::fs::write(&installed.libkrunfw_path, b"a kernel under test").unwrap();
        let home = tempfile::tempdir().unwrap();
        let mut config = config_in(&home);
        config.paths.msb = Some(installed.msb_path.clone());
        config.paths.libkrunfw = Some(installed.libkrunfw_path.clone());

        let runtime = ensure_with(&config, Some(&"0".repeat(64))).await.unwrap();

        assert_eq!(runtime.origin, RuntimeOrigin::Configuration);
        let library = std::fs::read(&runtime.libkrunfw_path).unwrap();
        assert_eq!(library, b"a kernel under test");
    }

    /// The Windows runtime pairs microsandbox's `msb.exe` with a `libkrunfw.dll` built from the
    /// libkrunfw commit of that microsandbox release, so a microsandbox upgrade needs new pins.
    #[test]
    fn windows_runtime_is_pinned_to_the_microsandbox_version() {
        let pins = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../scripts/libkrunfw-windows/versions.env"
        ))
        .unwrap();

        let pinned = pins
            .lines()
            .find_map(|line| line.strip_prefix("MICROSANDBOX_VERSION="))
            .unwrap();

        assert_eq!(pinned, expected_version().unwrap().to_string());
    }
}
