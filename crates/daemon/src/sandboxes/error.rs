//! The errors of the sandbox management, and the messages that tell the user what went wrong
//! when microsandbox fails to create, connect to or remove a sandbox.

use microsandbox::{MicrosandboxError, Sandbox};
use microsandbox_image::ImageError;
use oci_client::errors::{OciDistributionError, OciEnvelope, OciErrorCode};
use thiserror::Error;

/// Errors of sandbox management. Each variant carries the message the client sees.
#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum SandboxError {
    /// The sandbox or secret doesn't exist.
    #[error("{0}")]
    NotFound(String),
    /// The request has an invalid value.
    #[error("{0}")]
    InvalidArgument(String),
    /// The sandbox isn't in a state that allows the operation.
    #[error("{0}")]
    FailedPrecondition(String),
    /// A service the operation needs, such as an image registry, can't be reached.
    #[error("{0}")]
    Unavailable(String),
    /// microsandbox or the secret store failed.
    #[error("{0}")]
    Internal(String),
}

impl SandboxError {
    pub(super) fn not_found(message: impl Into<String>) -> Self {
        Self::NotFound(message.into())
    }

    pub(super) fn invalid_argument(message: impl Into<String>) -> Self {
        Self::InvalidArgument(message.into())
    }

    pub(super) fn failed_precondition(message: impl Into<String>) -> Self {
        Self::FailedPrecondition(message.into())
    }

    fn unavailable(message: impl Into<String>) -> Self {
        Self::Unavailable(message.into())
    }

    pub(super) fn internal(message: impl Into<String>) -> Self {
        Self::Internal(message.into())
    }
}

/// Logs why connecting to a sandbox failed and returns the error the client sees. A stopped
/// sandbox is a normal situation, so only other failures are logged as errors.
pub(super) fn connect_failed(name: &str, err: &MicrosandboxError) -> SandboxError {
    if matches!(err, MicrosandboxError::SandboxNotRunning(_)) {
        tracing::warn!(error = ?err, "failed to connect to sandbox {name}");
    } else {
        tracing::error!(error = ?err, "failed to connect to sandbox {name}");
    }

    SandboxError::failed_precondition("sandbox is not running")
}

/// Returns the error for a failed remove, logging failures other than a running sandbox.
pub(super) fn remove_failed(name: &str, err: MicrosandboxError) -> SandboxError {
    match err {
        MicrosandboxError::SandboxStillRunning(_) => SandboxError::failed_precondition(format!(
            "sandbox {name} is running; stop it first or remove it with force"
        )),
        err => {
            tracing::error!(error = ?err, "failed to remove sandbox {name}");
            SandboxError::internal("failed to remove sandbox")
        }
    }
}

/// Logs why a forced remove couldn't stop the sandbox and returns the error the client sees.
pub(super) fn stop_before_remove_failed(name: &str, err: &MicrosandboxError) -> SandboxError {
    tracing::error!(error = ?err, "failed to stop sandbox {name} before removing it");
    SandboxError::internal("failed to stop sandbox before removing it")
}

/// Returns the error for a failed create. A sandbox whose init failed to boot is removed, so
/// a start with `init: false` can create it again.
pub(super) async fn create_failed(
    name: &str,
    image: &str,
    err: &MicrosandboxError,
) -> SandboxError {
    let error = create_error(image, err);
    if matches!(error, SandboxError::FailedPrecondition(_))
        && let Err(err) = Sandbox::remove(name).await
    {
        tracing::warn!("failed to remove sandbox {name} after a failed create: {err}");
    }

    error
}

/// Turns a failed create of a sandbox from `image` into the error the client sees.
///
/// microsandbox only reports a missing init as text, so this matches on its message; other
/// failures that aren't about pulling the image stay internal.
pub(super) fn create_error(image: &str, err: &MicrosandboxError) -> SandboxError {
    match err {
        MicrosandboxError::Image(ImageError::Registry(err)) => registry_error(image, err),
        err if err.to_string().contains("handoff failed") => SandboxError::failed_precondition(
            "failed to create sandbox: the image has no /sbin/init; add one or set init: false \
             in .firebrick.yml",
        ),
        _ => SandboxError::internal("failed to create sandbox"),
    }
}

/// Turns a registry error while pulling `image` into the error the client sees. Registries
/// such as Docker Hub and GHCR answer an unknown repository with "unauthorized", so they don't
/// reveal private ones, so that error can also mean the image doesn't exist.
fn registry_error(image: &str, err: &OciDistributionError) -> SandboxError {
    match err {
        OciDistributionError::UnauthorizedError { .. } => SandboxError::not_found(format!(
            "failed to create sandbox: image {image} doesn't exist, or its registry needs a login"
        )),
        OciDistributionError::ImageManifestNotFoundError(_) => image_not_found(image),
        OciDistributionError::RegistryError { envelope, .. } if is_not_found(envelope) => {
            image_not_found(image)
        }
        OciDistributionError::RequestError(_) => SandboxError::unavailable(format!(
            "failed to create sandbox: couldn't reach the registry of image {image}; check the \
             network connection"
        )),
        _ => SandboxError::internal(format!(
            "failed to create sandbox: failed to pull image {image}"
        )),
    }
}

/// Returns the error for an image the registry doesn't have.
fn image_not_found(image: &str) -> SandboxError {
    SandboxError::not_found(format!(
        "failed to create sandbox: image {image} doesn't exist"
    ))
}

/// Whether the registry's errors say that the repository or the tag doesn't exist.
fn is_not_found(envelope: &OciEnvelope) -> bool {
    envelope.errors.iter().any(|error| {
        matches!(
            error.code,
            OciErrorCode::ManifestUnknown | OciErrorCode::NameUnknown | OciErrorCode::NotFound
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const IMAGE: &str = "alpine:0.0.404";

    /// Returns the error of a pull whose registry answered with the error code.
    fn registry_error_with_code(code: &str) -> MicrosandboxError {
        let envelope = serde_yaml::from_str(&format!("errors: [{{code: {code}}}]")).unwrap();

        MicrosandboxError::Image(ImageError::Registry(OciDistributionError::RegistryError {
            envelope,
            url: "https://registry.example/v2/alpine/manifests/0.0.404".to_string(),
        }))
    }

    #[test]
    fn create_error_explains_missing_init() {
        let error = create_error(
            IMAGE,
            &MicrosandboxError::Custom(
                "guest initialization failed: handoff failed: init error: no init binary found"
                    .to_string(),
            ),
        );

        assert!(
            matches!(&error, SandboxError::FailedPrecondition(message) if message.contains("set init: false")),
            "{error:?}"
        );
    }

    #[test]
    fn create_error_hides_other_failures() {
        assert_eq!(
            create_error(IMAGE, &MicrosandboxError::Custom("boom".to_string())),
            SandboxError::internal("failed to create sandbox")
        );
    }

    #[test]
    fn create_error_names_an_unknown_image() {
        for code in ["MANIFEST_UNKNOWN", "NAME_UNKNOWN", "NOT_FOUND"] {
            assert_eq!(
                create_error(IMAGE, &registry_error_with_code(code)),
                SandboxError::not_found(
                    "failed to create sandbox: image alpine:0.0.404 doesn't exist"
                ),
                "{code}"
            );
        }
    }

    #[test]
    fn create_error_names_an_image_the_registry_refuses() {
        let err = MicrosandboxError::Image(ImageError::Registry(
            OciDistributionError::UnauthorizedError {
                url: "https://ghcr.io/v2/user/private/manifests/1".to_string(),
            },
        ));

        assert_eq!(
            create_error(IMAGE, &err),
            SandboxError::not_found(
                "failed to create sandbox: image alpine:0.0.404 doesn't exist, or its registry \
                 needs a login"
            )
        );
    }

    #[test]
    fn create_error_names_the_image_of_other_registry_errors() {
        assert_eq!(
            create_error(IMAGE, &registry_error_with_code("DENIED")),
            SandboxError::internal("failed to create sandbox: failed to pull image alpine:0.0.404")
        );
    }
}
