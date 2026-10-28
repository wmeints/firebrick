//! The sandbox spec, `.firebrick.yml`: parses and validates it, reads and writes the file
//! without following symlinks, and edits its ports and migrates older specs in the text, so the
//! rest of the file stays as the user wrote it.

use std::collections::BTreeMap;
use std::fmt;

use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use std::result::Result;

mod edit;
mod env;
mod error;
mod file;
mod mounts;
mod names;
mod network;
mod ports;

pub use edit::{add_port, migrate_mise, remove_port, with_port};
use env::deserialize_env;
pub use env::{InvalidEnvName, RESERVED_ENV_PREFIX, validate_env_name};
pub use error::{SandboxSpecError, SizeError, SpecDiagnostic};
pub use file::{exists, from_file, read_text, to_file, write_text};
use mounts::deserialize_mounts;
pub use mounts::{InvalidGuestPath, MountSpec, guest_mount_path};
pub use names::{
    InvalidImage, InvalidSandboxName, MAX_SANDBOX_NAME_BYTES, parse_size_mib, validate_image,
    validate_sandbox_name,
};
use names::{deserialize_image, deserialize_sandbox_name, deserialize_size};
pub use network::{InvalidNetworkRule, NetworkRule, NetworkSpec};
use ports::deserialize_ports;
pub use ports::{InvalidPortMapping, PortMapping};

/// Image a sandbox runs when its spec doesn't name one: the `firebrick-base` image that the
/// release workflow publishes with the same version as this crate.
pub const DEFAULT_IMAGE: &str = concat!(
    "ghcr.io/wmeints/firebrick-base:v",
    env!("CARGO_PKG_VERSION")
);

/// Guest path the directory of `mise.config` is mounted at, read-only.
pub const MISE_DOTFILES_PATH: &str = "/opt/firebrick/dotfiles";

/// Describes a sandbox as configured in a spec file.
#[derive(Serialize, Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct SandboxSpec {
    /// Name of the sandbox, checked by [`validate_sandbox_name`].
    #[serde(deserialize_with = "deserialize_sandbox_name")]
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resources: Option<SandboxResourcesSpec>,
    /// OCI reference of the image the sandbox runs, checked by [`validate_image`].
    #[serde(
        default,
        deserialize_with = "deserialize_image",
        skip_serializing_if = "Option::is_none"
    )]
    pub image: Option<String>,
    /// Whether the sandbox runs the image's `/sbin/init` as PID 1. Defaults to `true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub init: Option<bool>,
    /// How fbkd uses mise when the sandbox starts. Without it, mise is enabled without dotfiles.
    #[serde(
        default,
        deserialize_with = "deserialize_mise",
        skip_serializing_if = "Option::is_none"
    )]
    pub mise: Option<MiseSpec>,
    /// Sizes of the volumes the sandbox gets. Missing fields use their defaults.
    #[serde(default)]
    pub volumes: VolumesSpec,
    /// Egress rules of the sandbox. Without it, the sandbox gets microsandbox's default policy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network: Option<NetworkSpec>,
    /// Host ports forwarded to the sandbox's loopback, written like Docker Compose: `3000` or
    /// `"8080:5173"`. No host port appears twice.
    #[serde(
        default,
        deserialize_with = "deserialize_ports",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub ports: Vec<PortMapping>,
    /// Extra host directories to bind mount into the sandbox, besides the workspace.
    #[serde(
        default,
        deserialize_with = "deserialize_mounts",
        skip_serializing_if = "Option::is_none"
    )]
    pub mounts: Option<Vec<MountSpec>>,
    /// Whether programs in the sandbox can read and write the host clipboard. Defaults to
    /// `false`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clipboard: Option<bool>,
    /// Environment variables set in every process in the sandbox, with their values as written:
    /// numbers and booleans become their text, and `$VAR` isn't expanded. Names are checked by
    /// [`validate_env_name`].
    #[serde(
        default,
        deserialize_with = "deserialize_env",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub env: BTreeMap<String, String>,
}

/// How fbkd uses mise when the sandbox starts.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MiseSpec {
    /// Whether fbkd applies the dotfiles and trusts and installs the workspace's mise tools when
    /// the sandbox starts. Defaults to `true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Host directory with a `mise.toml` whose `[dotfiles]` fbkd applies in the sandbox:
    /// absolute, starting with `~`, or relative to the spec file's directory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<String>,
}

impl MiseSpec {
    /// Whether mise is enabled, which it is unless `enabled` is `false`.
    pub fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }
}

/// CPU and memory resources assigned to a sandbox.
#[derive(Serialize, Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct SandboxResourcesSpec {
    pub cpu: u8,
    /// Memory size with a binary unit, such as `512 MiB` or `4Gi`.
    #[serde(deserialize_with = "deserialize_size")]
    pub memory: String,
}

impl Default for SandboxResourcesSpec {
    /// Returns the resources a sandbox gets when its spec doesn't set them.
    fn default() -> Self {
        Self {
            cpu: 2,
            memory: "4 GiB".to_string(),
        }
    }
}

/// Sizes of the volumes `fbkd` attaches to a sandbox, with binary units such as `20 GiB`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VolumesSpec {
    /// Size of the disk mounted at `/var/lib/docker`.
    #[serde(
        default = "default_docker_volume",
        deserialize_with = "deserialize_size"
    )]
    pub docker: String,
}

impl Default for VolumesSpec {
    /// Returns the volume sizes a sandbox gets when its spec doesn't set them.
    fn default() -> Self {
        Self {
            docker: default_docker_volume(),
        }
    }
}

/// Size of the Docker volume when the spec doesn't set one.
fn default_docker_volume() -> String {
    "20 GiB".to_string()
}

fn deserialize_mise<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<MiseSpec>, D::Error> {
    deserializer.deserialize_option(MiseVisitor)
}

/// Reads the `mise` section, and refuses the boolean it was before it became a map, at the
/// boolean's line and column.
struct MiseVisitor;

impl<'de> Visitor<'de> for MiseVisitor {
    type Value = Option<MiseSpec>;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a map with the mise settings")
    }

    fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_any(self)
    }

    fn visit_bool<E: de::Error>(self, value: bool) -> Result<Self::Value, E> {
        Err(E::custom(format!(
            "mise is a map now: write `mise: {{ enabled: {value} }}` to set mise.enabled"
        )))
    }

    fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
        MiseSpec::deserialize(de::value::MapAccessDeserializer::new(map)).map(Some)
    }
}

/// Parses a sandbox spec from the YAML text of a spec file.
pub fn from_str(text: &str) -> Result<SandboxSpec, SandboxSpecError> {
    serde_yaml::from_str::<SandboxSpec>(text).map_err(SandboxSpecError::InvalidSpec)
}

/// Creates a spec with the given name and default resources. It names no image, so the sandbox
/// runs the [`DEFAULT_IMAGE`] of the installed release, also after an update and `fbk upgrade`.
pub fn default_spec(name: String) -> SandboxSpec {
    SandboxSpec {
        name,
        image: None,
        resources: Some(SandboxResourcesSpec::default()),
        init: Some(true),
        mise: Some(MiseSpec {
            enabled: Some(true),
            config: None,
        }),
        volumes: VolumesSpec::default(),
        network: None,
        mounts: None,
        ports: vec![],
        clipboard: None,
        env: BTreeMap::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use std::net::IpAddr;
    use std::path::Path;
    use tempfile::{NamedTempFile, TempDir};

    fn write_spec(content: &str) -> NamedTempFile {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(content.as_bytes()).unwrap();
        file
    }

    #[test]
    fn parses_full_spec() {
        let file =
            write_spec("name: dev\nresources:\n  cpu: 4\n  memory: 8Gi\nimage: ubuntu:24.04\n");

        let spec = from_file(file.path()).unwrap().unwrap();
        let resources = spec.resources.unwrap();

        assert_eq!(spec.name, "dev");
        assert_eq!(resources.cpu, 4);
        assert_eq!(resources.memory, "8Gi");
        assert_eq!(spec.image.as_deref(), Some("ubuntu:24.04"));
    }

    #[test]
    fn image_is_optional() {
        let file = write_spec("name: dev\nresources:\n  cpu: 2\n  memory: 4Gi\n");

        let spec = from_file(file.path()).unwrap().unwrap();

        assert!(spec.image.is_none());
    }

    #[test]
    fn missing_file_returns_none() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("does-not-exist.yaml");

        let spec = from_file(&path).unwrap();

        assert!(spec.is_none());
    }

    #[test]
    fn directory_is_not_a_regular_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".firebrick.yml");
        fs::create_dir(&path).unwrap();

        let err = from_file(&path).unwrap_err();

        assert_eq!(
            err.to_string(),
            ".firebrick.yml isn't a regular file; firebrick only uses a regular file in the \
             workspace"
        );
    }

    #[cfg(unix)]
    #[test]
    fn read_text_refuses_a_fifo_instead_of_hanging() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".firebrick.yml");
        let status = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());

        let result = read_text(&path);

        assert!(matches!(result, Err(SandboxSpecError::NotRegularFile(_))));
    }

    #[test]
    fn exists_reports_a_regular_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".firebrick.yml");
        assert!(!exists(&path).unwrap());

        fs::write(&path, "name: dev\n").unwrap();
        assert!(exists(&path).unwrap());
    }

    #[test]
    fn exists_refuses_a_symlink() {
        let Some((_link_dir, link, _target)) = symlinked_spec("missing.yml") else {
            return;
        };
        assert!(matches!(exists(&link), Err(SandboxSpecError::Symlink(_))));
    }

    #[test]
    fn exists_refuses_a_directory() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".firebrick.yml");
        fs::create_dir(&path).unwrap();

        assert!(matches!(
            exists(&path),
            Err(SandboxSpecError::NotRegularFile(_))
        ));
    }

    #[test]
    fn read_failure_names_the_path_and_keeps_the_cause() {
        let file = write_spec("name: dev\n");
        fs::write(file.path(), [0xff, 0xfe]).unwrap();

        let err = read_text(file.path()).unwrap_err();

        assert_eq!(
            err.to_string(),
            format!("can't read {}", file.path().display())
        );
        assert!(std::error::Error::source(&err).is_some());
    }

    #[test]
    fn resources_are_optional() {
        let file = write_spec("name: dev\n");

        let spec = from_file(file.path()).unwrap().unwrap();

        assert!(spec.resources.is_none());
    }

    #[test]
    fn missing_required_field_returns_invalid_spec() {
        let file = write_spec("image: ubuntu:24.04\n");

        let result = from_file(file.path());

        assert!(matches!(result, Err(SandboxSpecError::InvalidSpec(_))));
    }

    #[test]
    fn malformed_yaml_returns_invalid_spec() {
        let file = write_spec("name: [unclosed\n");

        let result = from_file(file.path());

        assert!(matches!(result, Err(SandboxSpecError::InvalidSpec(_))));
    }

    #[test]
    fn cpu_out_of_range_returns_invalid_spec() {
        let file = write_spec("name: dev\nresources:\n  cpu: 256\n  memory: 4Gi\n");

        let result = from_file(file.path());

        assert!(matches!(result, Err(SandboxSpecError::InvalidSpec(_))));
    }

    #[test]
    fn unknown_field_returns_invalid_spec() {
        let file = write_spec("name: dev\nimgae: ubuntu:24.04\n");

        let result = from_file(file.path());

        assert!(matches!(result, Err(SandboxSpecError::InvalidSpec(_))));
    }

    #[test]
    fn diagnostic_reports_position_of_invalid_value() {
        let file = write_spec("name: dev\nresources:\n  cpu: 256\n  memory: 4Gi\n");

        let diagnostic = from_file(file.path()).unwrap_err().diagnostic().unwrap();

        assert_eq!(diagnostic.line, 3);
        assert_eq!(diagnostic.column, 8);
        assert_eq!(
            diagnostic.message,
            "resources.cpu: invalid value: integer `256`, expected u8"
        );
    }

    #[test]
    fn invalid_name_returns_diagnostic() {
        let long = "a".repeat(129);
        let names = [
            "\"../../x\"",
            "\"\"",
            "a/b",
            "\"..\"",
            "-dev",
            ".dev",
            "dév",
            "\"x\\e]0;pwned\\a\"",
            &long,
        ];

        for name in names {
            let diagnostic = from_str(&format!("name: {name}\n"))
                .unwrap_err()
                .diagnostic()
                .unwrap();

            assert_eq!((diagnostic.line, diagnostic.column), (1, 7), "{name:?}");
            assert!(
                diagnostic.message.starts_with("name: invalid sandbox name"),
                "{}",
                diagnostic.message
            );
        }
    }

    #[test]
    fn parses_names_microsandbox_accepts() {
        let longest = "a".repeat(128);

        for name in ["my-project", "My.App_2", "firebrick-8a5eda", "0", &longest] {
            let spec = from_str(&format!("name: {name}\n")).unwrap();

            assert_eq!(spec.name, name);
        }
    }

    #[test]
    fn invalid_image_returns_diagnostic() {
        let images = [
            "Ubuntu:24.04",
            "ubuntu:24.04 latest",
            "\"ubuntu\\e]0;pwned\\a\"",
            "ubuntu@sha256:abc",
            "-ubuntu",
        ];

        for image in images {
            let diagnostic = from_str(&format!("name: dev\nimage: {image}\n"))
                .unwrap_err()
                .diagnostic()
                .unwrap();

            assert_eq!((diagnostic.line, diagnostic.column), (2, 8), "{image:?}");
            assert!(
                diagnostic.message.starts_with("image: invalid image"),
                "{}",
                diagnostic.message
            );
        }
    }

    #[test]
    fn parses_images_microsandbox_accepts() {
        let digest = format!("sha256:{}", "a".repeat(64));
        let images = [
            "alpine",
            "ubuntu:24.04",
            "ghcr.io/wmeints/firebrick-base:v0.1.0",
            "localhost:5000/firebrick-base:dev",
            "my_org/app:V1.0-RC",
            DEFAULT_IMAGE,
            &format!("alpine@{digest}"),
            &format!("ghcr.io/wmeints/firebrick-base:v0.1.0@{digest}"),
        ];

        for image in images {
            let spec = from_str(&format!("name: dev\nimage: {image}\n")).unwrap();

            assert_eq!(spec.image.as_deref(), Some(image));
        }
    }

    #[test]
    fn null_or_empty_image_means_no_image() {
        for image in ["~", "\"\""] {
            let spec = from_str(&format!("name: dev\nimage: {image}\n")).unwrap();

            assert!(spec.image.is_none(), "{image:?}");
        }
    }

    #[test]
    fn invalid_image_diagnostic_says_what_is_wrong() {
        // 40 hex digits pass the reference format, but a sha256 digest has 64.
        let image = format!("ubuntu@sha256:{}", "a".repeat(40));

        let diagnostic = from_str(&format!("name: dev\nimage: {image}\n"))
            .unwrap_err()
            .diagnostic()
            .unwrap();

        assert_eq!(
            diagnostic.message,
            format!(
                "image: invalid image {image:?}: invalid checksum digest length; use an OCI \
                 reference such as `ubuntu:24.04` or `ghcr.io/org/image:tag`"
            )
        );
    }

    #[test]
    fn diagnostic_strips_escape_sequences_from_quoted_spec_text() {
        let diagnostic = from_str("name: dev\n\"bogus\\e]0;pwned\\a\\x9b31m\": 1\n")
            .unwrap_err()
            .diagnostic()
            .unwrap();

        assert!(
            diagnostic
                .message
                .starts_with("unknown field `bogus`, expected one of"),
            "{:?}",
            diagnostic.message
        );
        assert!(!diagnostic.message.contains(char::is_control));
    }

    #[test]
    fn misindented_line_after_name_is_folded_into_the_name() {
        // YAML folds a more-indented line into the plain scalar before it, so the name becomes
        // `dev bad`, and the name check rejects it before the parser reaches the `:`.
        let diagnostic = from_str("name: dev\n  bad: indent\n")
            .unwrap_err()
            .diagnostic()
            .unwrap();

        assert_eq!((diagnostic.line, diagnostic.column), (1, 7));
        assert!(
            diagnostic
                .message
                .starts_with("name: invalid sandbox name \"dev bad\""),
            "{}",
            diagnostic.message
        );
    }

    #[test]
    fn diagnostic_reports_position_of_syntax_error() {
        // The mount's host is free text, so the parser folds the misindented line into it and
        // fails at its `:` rather than a field check rejecting the folded value first.
        let file =
            write_spec("mounts:\n  - host: dev\n      bad: indent\n    guest: /x\nname: dev\n");

        let diagnostic = from_file(file.path()).unwrap_err().diagnostic().unwrap();

        assert_eq!((diagnostic.line, diagnostic.column), (3, 10));
        assert_eq!(
            diagnostic.message,
            "mapping values are not allowed in this context"
        );
        assert!(!diagnostic.message.contains("at line"));
    }

    #[test]
    fn parses_sizes() {
        let cases = [
            ("512 MiB", 512),
            ("512Mi", 512),
            ("4 GiB", 4096),
            ("4Gi", 4096),
            (" 2GiB ", 2048),
        ];

        for (input, expected) in cases {
            assert_eq!(parse_size_mib(input), Ok(expected), "{input:?}");
        }
    }

    #[test]
    fn rejects_invalid_sizes() {
        for input in ["", "lots", "4", "4 GB", "0 GiB", "-1 GiB", "1.5 GiB", "GiB"] {
            assert!(
                matches!(parse_size_mib(input), Err(SizeError::Invalid(_))),
                "{input:?} should be rejected"
            );
        }
    }

    #[test]
    fn rejects_sizes_that_overflow() {
        assert!(matches!(
            parse_size_mib("4194304 GiB"),
            Err(SizeError::TooLarge(_))
        ));
    }

    #[test]
    fn invalid_memory_returns_diagnostic() {
        let file = write_spec("name: dev\nresources:\n  cpu: 2\n  memory: lots\n");

        let diagnostic = from_file(file.path()).unwrap_err().diagnostic().unwrap();

        assert_eq!((diagnostic.line, diagnostic.column), (4, 11));
        assert!(
            diagnostic.message.contains("invalid size `lots`"),
            "{}",
            diagnostic.message
        );
    }

    #[test]
    fn volumes_are_optional() {
        let file = write_spec("name: dev\n");

        let spec = from_file(file.path()).unwrap().unwrap();

        assert_eq!(spec.volumes, VolumesSpec::default());
        assert_eq!(spec.volumes.docker, "20 GiB");
    }

    #[test]
    fn docker_volume_is_optional() {
        let file = write_spec("name: dev\nvolumes: {}\n");

        let spec = from_file(file.path()).unwrap().unwrap();

        assert_eq!(spec.volumes.docker, "20 GiB");
    }

    #[test]
    fn parses_docker_volume() {
        let file = write_spec(
            "name: dev\nresources:\n  cpu: 1\n  memory: 2GiB\nvolumes:\n  docker: 40GiB\n",
        );

        let spec = from_file(file.path()).unwrap().unwrap();

        assert_eq!(spec.volumes.docker, "40GiB");
    }

    #[test]
    fn invalid_docker_volume_returns_diagnostic() {
        for size in ["20 GB", "0 GiB"] {
            let file = write_spec(&format!("name: dev\nvolumes:\n  docker: {size}\n"));

            let diagnostic = from_file(file.path()).unwrap_err().diagnostic().unwrap();

            assert_eq!((diagnostic.line, diagnostic.column), (3, 11), "{size:?}");
            assert!(
                diagnostic
                    .message
                    .starts_with(&format!("volumes.docker: invalid size `{size}`")),
                "{}",
                diagnostic.message
            );
        }
    }

    #[test]
    fn unknown_volume_returns_invalid_spec() {
        let file = write_spec("name: dev\nvolumes:\n  dokcer: 20GiB\n");

        let result = from_file(file.path());

        assert!(matches!(result, Err(SandboxSpecError::InvalidSpec(_))));
    }

    #[test]
    fn default_spec_leaves_out_the_image_and_uses_default_resources() {
        let spec = default_spec("dev".to_string());
        let resources = spec.resources.unwrap();

        assert_eq!(spec.image, None);
        assert_eq!((resources.cpu, resources.memory.as_str()), (2, "4 GiB"));
        assert_eq!(spec.init, Some(true));
        assert_eq!(
            spec.mise,
            Some(MiseSpec {
                enabled: Some(true),
                config: None
            })
        );
        assert_eq!(spec.volumes, VolumesSpec::default());
        assert!(spec.network.is_none());
        assert!(spec.mounts.is_none());
        assert!(spec.ports.is_empty());
        assert_eq!(spec.clipboard, None);
    }

    #[test]
    fn init_is_optional() {
        let file = write_spec("name: dev\n");

        let spec = from_file(file.path()).unwrap().unwrap();

        assert!(spec.init.is_none());
    }

    #[test]
    fn parses_init() {
        let file = write_spec("name: dev\nimage: alpine:3.22\ninit: false\n");

        let spec = from_file(file.path()).unwrap().unwrap();

        assert_eq!(spec.init, Some(false));
    }

    #[test]
    fn invalid_init_returns_diagnostic() {
        let file = write_spec("name: dev\ninit: sometimes\n");

        let diagnostic = from_file(file.path()).unwrap_err().diagnostic().unwrap();

        assert_eq!(diagnostic.line, 2);
        assert!(
            diagnostic.message.contains("init"),
            "{}",
            diagnostic.message
        );
    }

    #[test]
    fn mise_is_optional() {
        let file = write_spec("name: dev\n");

        let spec = from_file(file.path()).unwrap().unwrap();

        assert!(spec.mise.is_none());
    }

    #[test]
    fn parses_mise() {
        let file = write_spec("name: dev\nmise:\n  enabled: false\n  config: ~/dotfiles\n");

        let spec = from_file(file.path()).unwrap().unwrap();

        assert_eq!(
            spec.mise,
            Some(MiseSpec {
                enabled: Some(false),
                config: Some("~/dotfiles".to_string()),
            })
        );
    }

    #[test]
    fn mise_fields_are_optional_and_mise_is_enabled_by_default() {
        let file = write_spec("name: dev\nmise: {}\n");

        let mise = from_file(file.path()).unwrap().unwrap().mise.unwrap();

        assert_eq!(mise, MiseSpec::default());
        assert!(mise.is_enabled());
    }

    #[test]
    fn mise_is_disabled_only_by_enabled_false() {
        let mise = |enabled| MiseSpec {
            enabled,
            config: None,
        };

        assert!(mise(None).is_enabled());
        assert!(mise(Some(true)).is_enabled());
        assert!(!mise(Some(false)).is_enabled());
    }

    #[test]
    fn boolean_mise_returns_diagnostic_naming_mise_enabled() {
        let file = write_spec("name: dev\nmise: false\n");

        let diagnostic = from_file(file.path()).unwrap_err().diagnostic().unwrap();

        assert_eq!((diagnostic.line, diagnostic.column), (2, 7));
        assert!(
            diagnostic.message.contains("mise.enabled"),
            "{}",
            diagnostic.message
        );
    }

    #[test]
    fn unknown_mise_field_returns_diagnostic() {
        let file = write_spec("name: dev\nmise:\n  dotfiles: ~/dotfiles\n");

        let diagnostic = from_file(file.path()).unwrap_err().diagnostic().unwrap();

        assert_eq!(diagnostic.line, 3);
        assert!(
            diagnostic.message.contains("dotfiles"),
            "{}",
            diagnostic.message
        );
    }

    #[test]
    fn migrate_mise_rewrites_only_the_boolean_line() {
        let text = "# my sandbox\nname: dev\nmise: false # no tools\nports:\n  - 3000\n";

        let (migrated, enabled) = migrate_mise(text).unwrap();

        assert!(!enabled);
        assert_eq!(
            migrated,
            "# my sandbox\nname: dev\nmise: # no tools\n  enabled: false\nports:\n  - 3000\n"
        );
        assert_eq!(
            from_str(&migrated).unwrap().mise,
            Some(MiseSpec {
                enabled: Some(false),
                config: None,
            })
        );
    }

    #[test]
    fn migrate_mise_reads_yaml_booleans_without_a_trailing_newline() {
        assert_eq!(
            migrate_mise("name: dev\nmise: True"),
            Some(("name: dev\nmise:\n  enabled: true\n".to_string(), true))
        );
    }

    #[test]
    fn migrate_mise_keeps_crlf_line_endings() {
        assert_eq!(
            migrate_mise("name: dev\r\nmise: false\r\n"),
            Some((
                "name: dev\r\nmise:\r\n  enabled: false\r\n".to_string(),
                false
            ))
        );
    }

    #[test]
    fn mise_dotfiles_mount_is_read_only_at_the_dotfiles_path() {
        assert_eq!(
            MountSpec::mise_dotfiles("/home/me/dotfiles"),
            MountSpec {
                host: "/home/me/dotfiles".to_string(),
                guest: "/opt/firebrick/dotfiles".to_string(),
                readonly: true,
            }
        );
    }

    #[test]
    fn migrate_mise_leaves_other_specs_alone() {
        for text in [
            "name: dev\n",
            "name: dev\nmise:\n  enabled: false\n",
            "name: dev\nmise: { enabled: false }\n",
            "name: dev\nenv:\n  mise: false\n",
            "name: dev\nmise: sometimes\n",
        ] {
            assert_eq!(migrate_mise(text), None, "{text}");
        }
    }

    #[test]
    fn invalid_mise_returns_diagnostic() {
        let file = write_spec("name: dev\nmise: sometimes\n");

        let diagnostic = from_file(file.path()).unwrap_err().diagnostic().unwrap();

        assert_eq!((diagnostic.line, diagnostic.column), (2, 7));
        assert!(
            diagnostic.message.contains("mise"),
            "{}",
            diagnostic.message
        );
    }

    #[test]
    fn clipboard_is_optional() {
        let file = write_spec("name: dev\n");

        let spec = from_file(file.path()).unwrap().unwrap();

        assert!(spec.clipboard.is_none());
    }

    #[test]
    fn parses_clipboard() {
        let file = write_spec("name: dev\nclipboard: true\n");

        let spec = from_file(file.path()).unwrap().unwrap();

        assert_eq!(spec.clipboard, Some(true));
    }

    #[test]
    fn invalid_clipboard_returns_diagnostic() {
        let file = write_spec("name: dev\nclipboard: always\n");

        let diagnostic = from_file(file.path()).unwrap_err().diagnostic().unwrap();

        assert_eq!((diagnostic.line, diagnostic.column), (2, 12));
        assert!(
            diagnostic.message.contains("clipboard"),
            "{}",
            diagnostic.message
        );
    }

    #[test]
    fn network_is_optional() {
        let file = write_spec("name: dev\n");

        let spec = from_file(file.path()).unwrap().unwrap();

        assert!(spec.network.is_none());
    }

    #[test]
    fn network_fields_have_defaults() {
        let file = write_spec("name: dev\nnetwork: {}\n");

        let spec = from_file(file.path()).unwrap().unwrap();

        assert_eq!(spec.network, Some(NetworkSpec::default()));
        assert!(!NetworkSpec::default().enforce);
        assert!(NetworkSpec::default().is_enabled());
    }

    #[test]
    fn parses_network_enabled() {
        for (value, expected) in [("false", false), ("true", true)] {
            let file = write_spec(&format!("name: dev\nnetwork:\n  enabled: {value}\n"));

            let network = from_file(file.path()).unwrap().unwrap().network.unwrap();

            assert_eq!(network.enabled, Some(expected));
            assert_eq!(network.is_enabled(), expected);
        }
    }

    #[test]
    fn diagnostic_reports_position_of_non_boolean_network_enabled() {
        let file = write_spec("name: dev\nnetwork:\n  enabled: maybe\n");

        let diagnostic = from_file(file.path()).unwrap_err().diagnostic().unwrap();

        assert_eq!((diagnostic.line, diagnostic.column), (3, 12));
        assert!(
            diagnostic.message.starts_with("network.enabled: "),
            "{}",
            diagnostic.message
        );
    }

    #[test]
    fn parses_network() {
        let file = write_spec(concat!(
            "name: dev\n",
            "network:\n",
            "  enforce: true\n",
            "  allow:\n",
            "    - github.com\n",
            "    - \"*.githubusercontent.com\"\n",
            "  deny:\n",
            "    - gist.github.com\n",
        ));

        let network = from_file(file.path()).unwrap().unwrap().network.unwrap();

        assert!(network.enforce);
        assert_eq!(
            network.allow,
            [
                NetworkRule::Domain("github.com".to_string()),
                NetworkRule::DomainSuffix("githubusercontent.com".to_string()),
            ]
        );
        assert_eq!(
            network.deny,
            [NetworkRule::Domain("gist.github.com".to_string())]
        );
    }

    #[test]
    fn parses_addresses_and_ranges() {
        let ip = |value: &str| value.parse::<IpAddr>().unwrap();
        let cases = [
            ("140.82.112.4", NetworkRule::Ip(ip("140.82.112.4"))),
            ("2001:db8::1", NetworkRule::Ip(ip("2001:db8::1"))),
            (
                "192.168.10.0/24",
                NetworkRule::Cidr {
                    address: ip("192.168.10.0"),
                    prefix: 24,
                },
            ),
            (
                "2001:db8::/32",
                NetworkRule::Cidr {
                    address: ip("2001:db8::"),
                    prefix: 32,
                },
            ),
        ];

        for (input, expected) in cases {
            assert_eq!(input.parse(), Ok(expected), "{input:?}");
        }
    }

    #[test]
    fn network_rule_displays_as_written() {
        for rule in [
            "github.com",
            "*.example.com",
            "10.0.0.1",
            "10.0.0.0/8",
            "::1",
            "fd00::/8",
        ] {
            assert_eq!(rule.parse::<NetworkRule>().unwrap().to_string(), rule);
        }
    }

    #[test]
    fn rejects_invalid_network_rules() {
        for rule in [
            "",
            "*",
            "*.",
            "*.com",
            "foo.*.com",
            "github.*",
            "https://github.com",
            "github.com:443",
            "github.com/path",
            "10.0.0.0/33",
            "::/129",
            "10.0.0/8",
            "10.0.0.0/",
            "-github.com",
            "github..com",
            "git hub.com",
        ] {
            assert_eq!(
                rule.parse::<NetworkRule>(),
                Err(InvalidNetworkRule(rule.to_string())),
                "{rule:?} should be rejected"
            );
        }
    }

    #[test]
    fn invalid_network_rule_returns_diagnostic() {
        let file = write_spec(
            "name: dev\nnetwork:\n  allow:\n    - github.com\n    - https://github.com\n",
        );

        let diagnostic = from_file(file.path()).unwrap_err().diagnostic().unwrap();

        assert_eq!((diagnostic.line, diagnostic.column), (5, 7));
        assert!(
            diagnostic.message.ends_with(
                "invalid network rule \"https://github.com\": use a host name, *.domain, \
                 an IP address or a CIDR range"
            ),
            "{}",
            diagnostic.message
        );
    }

    #[test]
    fn unknown_network_field_returns_invalid_spec() {
        let file = write_spec("name: dev\nnetwork:\n  enfroce: true\n");

        let result = from_file(file.path());

        assert!(matches!(result, Err(SandboxSpecError::InvalidSpec(_))));
    }

    #[test]
    fn ports_are_optional() {
        let file = write_spec("name: dev\n");

        let spec = from_file(file.path()).unwrap().unwrap();

        assert!(spec.ports.is_empty());
    }

    fn port(host: u16, guest: u16) -> PortMapping {
        PortMapping { host, guest }
    }

    #[test]
    fn with_port_adds_a_new_host_port_at_the_end() {
        assert_eq!(
            with_port(&[port(3000, 3000)], port(8080, 5173)),
            [port(3000, 3000), port(8080, 5173)]
        );
    }

    #[test]
    fn with_port_replaces_the_guest_port_of_a_listed_host_port() {
        assert_eq!(
            with_port(&[port(3000, 3000), port(8080, 5173)], port(3000, 4000)),
            [port(3000, 4000), port(8080, 5173)]
        );
    }

    const SPEC_WITH_PORTS: &str = "# my sandbox\n\
        name: dev # the name\n\
        ports:\n\
        \x20 # web\n\
        \x20 - 3000\n\
        \x20 - \"8080:5173\" # vite\n\
        \n\
        # resources\n\
        image: alpine:3.22\n";

    #[test]
    fn add_port_appends_to_an_existing_list_and_keeps_comments() {
        let edited = add_port(SPEC_WITH_PORTS, port(9000, 9001)).unwrap();

        assert_eq!(
            edited,
            "# my sandbox\n\
             name: dev # the name\n\
             ports:\n\
             \x20 # web\n\
             \x20 - 3000\n\
             \x20 - \"8080:5173\" # vite\n\
             \x20 - \"9000:9001\"\n\
             \n\
             # resources\n\
             image: alpine:3.22\n"
        );
    }

    #[test]
    fn add_port_replaces_the_entry_of_a_listed_host_port() {
        let edited = add_port(SPEC_WITH_PORTS, port(8080, 8080)).unwrap();

        assert_eq!(
            edited,
            "# my sandbox\n\
             name: dev # the name\n\
             ports:\n\
             \x20 # web\n\
             \x20 - 3000\n\
             \x20 - 8080\n\
             \n\
             # resources\n\
             image: alpine:3.22\n"
        );
    }

    #[test]
    fn add_port_keeps_the_indentation_of_a_list_at_the_key_level() {
        let edited = add_port("name: dev\nports:\n- 3000\n", port(4000, 4000)).unwrap();

        assert_eq!(edited, "name: dev\nports:\n- 3000\n- 4000\n");
    }

    #[test]
    fn add_port_adds_a_missing_ports_key() {
        let edited =
            add_port("name: dev # the name\nimage: alpine:3.22", port(3000, 3000)).unwrap();

        assert_eq!(
            edited,
            "name: dev # the name\nimage: alpine:3.22\nports:\n  - 3000\n"
        );
    }

    #[test]
    fn add_port_rewrites_a_flow_list_as_a_block_list() {
        let edited = add_port(
            "name: dev\nports: [3000]\nimage: alpine:3.22\n",
            port(8080, 5173),
        )
        .unwrap();

        assert_eq!(
            edited,
            "name: dev\nports:\n  - 3000\n  - \"8080:5173\"\nimage: alpine:3.22\n"
        );
    }

    #[test]
    fn add_port_rejects_an_invalid_spec() {
        let result = add_port("image: alpine:3.22\n", port(3000, 3000));

        assert!(matches!(result, Err(SandboxSpecError::InvalidSpec(_))));
    }

    #[test]
    fn remove_port_removes_the_entry_and_keeps_comments() {
        let edited = remove_port(SPEC_WITH_PORTS, 3000).unwrap().unwrap();

        assert_eq!(
            edited,
            "# my sandbox\n\
             name: dev # the name\n\
             ports:\n\
             \x20 # web\n\
             \x20 - \"8080:5173\" # vite\n\
             \n\
             # resources\n\
             image: alpine:3.22\n"
        );
    }

    #[test]
    fn remove_port_writes_an_empty_list_for_the_last_entry() {
        let edited = remove_port("name: dev\nports:\n  # web\n  - 3000\nimage: x\n", 3000)
            .unwrap()
            .unwrap();

        assert_eq!(edited, "name: dev\nports: []\n  # web\nimage: x\n");
        assert!(from_str(&edited).unwrap().ports.is_empty());
    }

    #[test]
    fn remove_port_returns_none_for_an_unlisted_host_port() {
        assert_eq!(remove_port(SPEC_WITH_PORTS, 5173).unwrap(), None);
        assert_eq!(remove_port("name: dev\n", 3000).unwrap(), None);
    }

    #[test]
    fn parses_ports() {
        let file = write_spec("name: dev\nports:\n  - 3000\n  - \"8080:5173\"\n");

        let spec = from_file(file.path()).unwrap().unwrap();

        assert_eq!(
            spec.ports,
            [
                PortMapping {
                    host: 3000,
                    guest: 3000
                },
                PortMapping {
                    host: 8080,
                    guest: 5173
                },
            ]
        );
    }

    #[test]
    fn port_mapping_displays_host_and_guest() {
        let mapping: PortMapping = "3000".parse().unwrap();

        assert_eq!(mapping.to_string(), "3000:3000");
        assert_eq!("3000:3000".parse(), Ok(mapping));
    }

    #[test]
    fn rejects_invalid_port_mappings() {
        for mapping in [
            "", "0", "65536", "-1", "abc", "80:", ":80", "0:80", "80:0", "1:2:3", "80 : 81",
        ] {
            assert_eq!(
                mapping.parse::<PortMapping>(),
                Err(InvalidPortMapping(mapping.to_string())),
                "{mapping:?} should be rejected"
            );
        }
    }

    #[test]
    fn invalid_port_returns_diagnostic() {
        for (entry, column) in [("0", 5), ("65536", 5), ("\"8080:abc\"", 5), ("web", 5)] {
            let file = write_spec(&format!("name: dev\nports:\n  - 3000\n  - {entry}\n"));

            let diagnostic = from_file(file.path()).unwrap_err().diagnostic().unwrap();

            assert_eq!((diagnostic.line, diagnostic.column), (4, column), "{entry}");
            assert!(
                diagnostic.message.contains("invalid port mapping"),
                "{}",
                diagnostic.message
            );
        }
    }

    #[test]
    fn port_of_wrong_type_returns_diagnostic() {
        let file = write_spec("name: dev\nports:\n  - {host: 80}\n");

        let diagnostic = from_file(file.path()).unwrap_err().diagnostic().unwrap();

        assert_eq!(diagnostic.line, 3);
        assert!(
            diagnostic.message.contains("a port such as 3000"),
            "{}",
            diagnostic.message
        );
    }

    #[test]
    fn duplicate_host_port_returns_diagnostic() {
        let file = write_spec("name: dev\nports:\n  - 3000\n  - \"3000:4000\"\n");

        let diagnostic = from_file(file.path()).unwrap_err().diagnostic().unwrap();

        assert_eq!((diagnostic.line, diagnostic.column), (4, 5));
        assert!(
            diagnostic
                .message
                .ends_with("host port 3000 is listed more than once"),
            "{}",
            diagnostic.message
        );
    }

    #[test]
    fn mounts_are_optional() {
        for content in ["name: dev\n", "name: dev\nmounts: null\n"] {
            let file = write_spec(content);

            let spec = from_file(file.path()).unwrap().unwrap();

            assert!(spec.mounts.is_none(), "{content:?}");
        }
    }

    #[test]
    fn parses_mounts() {
        let file = write_spec(concat!(
            "name: dev\n",
            "mounts:\n",
            "  - host: ../shared-lib\n",
            "    guest: /workspaces/shared-lib\n",
            "  - host: ~/datasets/images\n",
            "    guest: /data/images\n",
            "    readonly: true\n",
        ));

        let mounts = from_file(file.path()).unwrap().unwrap().mounts.unwrap();

        assert_eq!(
            mounts,
            [
                MountSpec {
                    host: "../shared-lib".to_string(),
                    guest: "/workspaces/shared-lib".to_string(),
                    readonly: false,
                },
                MountSpec {
                    host: "~/datasets/images".to_string(),
                    guest: "/data/images".to_string(),
                    readonly: true,
                },
            ]
        );
    }

    #[test]
    fn mount_without_guest_returns_invalid_spec() {
        let file = write_spec("name: dev\nmounts:\n  - host: ../lib\n");

        let diagnostic = from_file(file.path()).unwrap_err().diagnostic().unwrap();

        assert_eq!(diagnostic.line, 3);
        assert!(
            diagnostic.message.contains("missing field `guest`"),
            "{}",
            diagnostic.message
        );
    }

    #[test]
    fn unknown_mount_field_returns_invalid_spec() {
        let file =
            write_spec("name: dev\nmounts:\n  - host: ../lib\n    guest: /lib\n    ro: true\n");

        let result = from_file(file.path());

        assert!(matches!(result, Err(SandboxSpecError::InvalidSpec(_))));
    }

    #[test]
    fn invalid_guest_path_returns_diagnostic() {
        for guest in ["data", "./data", "/"] {
            let file = write_spec(&format!(
                "name: dev\nmounts:\n  - host: ../data\n    guest: {guest}\n"
            ));

            let diagnostic = from_file(file.path()).unwrap_err().diagnostic().unwrap();

            assert_eq!((diagnostic.line, diagnostic.column), (4, 12), "{guest:?}");
            assert_eq!(
                diagnostic.message,
                "mounts[0].guest: guest path must be an absolute path other than /"
            );
        }
    }

    #[test]
    fn guest_mount_path_normalizes_the_path() {
        let cases = [
            ("/data", "/data"),
            ("/data/", "/data"),
            ("/data/./images", "/data/images"),
            ("//data//images/.", "/data/images"),
        ];

        for (input, expected) in cases {
            assert_eq!(
                guest_mount_path(input).as_deref(),
                Ok(expected),
                "{input:?}"
            );
        }
    }

    #[test]
    fn guest_mount_path_rejects_paths_microsandbox_refuses() {
        let cases = [
            ("data", InvalidGuestPath::NotAbsolute),
            ("", InvalidGuestPath::NotAbsolute),
            ("/", InvalidGuestPath::NotAbsolute),
            ("/./", InvalidGuestPath::NotAbsolute),
            ("/data/../etc", InvalidGuestPath::ParentDir),
            ("/data:v1", InvalidGuestPath::Separator),
            ("/data;v1", InvalidGuestPath::Separator),
            ("/data,v1", InvalidGuestPath::Separator),
        ];

        for (input, expected) in cases {
            assert_eq!(guest_mount_path(input), Err(expected), "{input:?}");
        }
    }

    #[test]
    fn duplicate_guest_path_ignores_trailing_slash() {
        let file = write_spec(concat!(
            "name: dev\n",
            "mounts:\n",
            "  - host: ../a\n",
            "    guest: /data/images\n",
            "  - host: ../b\n",
            "    guest: /data/images/\n",
        ));

        let diagnostic = from_file(file.path()).unwrap_err().diagnostic().unwrap();

        assert_eq!(
            diagnostic.message,
            "mounts: guest path /data/images is mounted more than once"
        );
    }

    #[test]
    fn invalid_guest_path_reports_parent_dir() {
        let file = write_spec("name: dev\nmounts:\n  - host: ../a\n    guest: /data/../etc\n");

        let diagnostic = from_file(file.path()).unwrap_err().diagnostic().unwrap();

        assert_eq!((diagnostic.line, diagnostic.column), (4, 12));
        assert_eq!(
            diagnostic.message,
            "mounts[0].guest: guest path must not contain .."
        );
    }

    #[test]
    fn duplicate_guest_path_returns_diagnostic() {
        let file = write_spec(concat!(
            "name: dev\n",
            "mounts:\n",
            "  - host: ../a\n",
            "    guest: /data/images\n",
            "  - host: ../b\n",
            "    guest: /data/images\n",
        ));

        let diagnostic = from_file(file.path()).unwrap_err().diagnostic().unwrap();

        assert_eq!((diagnostic.line, diagnostic.column), (3, 3));
        assert_eq!(
            diagnostic.message,
            "mounts: guest path /data/images is mounted more than once"
        );
    }

    #[test]
    fn unreadable_file_has_no_diagnostic() {
        let dir = TempDir::new().unwrap();

        let result = from_file(dir.path());

        assert!(result.unwrap_err().diagnostic().is_none());
    }

    fn rules(values: &[&str]) -> Vec<NetworkRule> {
        values.iter().map(|value| value.parse().unwrap()).collect()
    }

    fn network(allow: &[&str], deny: &[&str]) -> NetworkSpec {
        NetworkSpec {
            enforce: false,
            allow: rules(allow),
            deny: rules(deny),
            ..NetworkSpec::default()
        }
    }

    #[test]
    fn allow_adds_rules_and_removes_them_from_deny() {
        let mut spec = network(&["github.com"], &["example.org", "10.0.0.0/8"]);

        assert!(spec.allow(&rules(&["example.org", "*.npmjs.org"])));

        assert_eq!(
            spec,
            network(
                &["github.com", "example.org", "*.npmjs.org"],
                &["10.0.0.0/8"]
            )
        );
    }

    #[test]
    fn deny_adds_rules_and_removes_them_from_allow() {
        let mut spec = network(&["github.com", "1.1.1.1"], &[]);

        assert!(spec.deny(&rules(&["1.1.1.1"])));

        assert_eq!(spec, network(&["github.com"], &["1.1.1.1"]));
    }

    #[test]
    fn allow_skips_rules_that_are_already_allowed() {
        let mut spec = network(&["github.com"], &[]);

        assert!(!spec.allow(&rules(&["github.com"])));
        assert!(spec.allow(&rules(&["example.org", "example.org"])));

        assert_eq!(spec, network(&["github.com", "example.org"], &[]));
    }

    #[test]
    fn deny_reports_no_change_for_rules_that_are_already_denied() {
        let mut spec = network(&[], &["example.org"]);

        assert!(!spec.deny(&rules(&["example.org"])));
        assert_eq!(spec, network(&[], &["example.org"]));
    }

    #[test]
    fn to_file_round_trips_the_spec() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".firebrick.yml");
        let mut spec = default_spec("dev".to_string());
        spec.network = Some(NetworkSpec {
            enabled: Some(false),
            enforce: true,
            ..network(
                &["github.com", "*.npmjs.org", "10.0.0.0/8"],
                &["2001:db8::1"],
            )
        });

        to_file(&spec, &path).unwrap();
        let loaded = from_file(&path).unwrap().unwrap();

        assert_eq!(loaded.name, "dev");
        assert_eq!(loaded.image, spec.image);
        assert_eq!(loaded.init, Some(true));
        assert_eq!(
            loaded.mise,
            Some(MiseSpec {
                enabled: Some(true),
                config: None
            })
        );
        assert_eq!(loaded.volumes, spec.volumes);
        assert_eq!(loaded.network, spec.network);
    }

    #[test]
    fn to_file_leaves_out_unset_fields() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".firebrick.yml");
        let spec = from_file(write_spec("name: dev\n").path())
            .unwrap()
            .unwrap();

        to_file(&spec, &path).unwrap();
        let content = fs::read_to_string(&path).unwrap();

        assert!(!content.contains("null"), "{content}");
        assert!(!content.contains("network"), "{content}");
    }

    #[test]
    fn to_file_reports_write_failure() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("missing").join(".firebrick.yml");

        let err = to_file(&default_spec("dev".to_string()), &path).unwrap_err();

        assert_eq!(err.to_string(), format!("can't write {}", path.display()));
        assert!(matches!(err, SandboxSpecError::CantWrite { .. }));
    }

    /// Creates `.firebrick.yml` in a new directory as a symlink to `target` in that directory,
    /// or returns `None` when this Windows can't create symlinks (see [`symlink_file`]).
    fn symlinked_spec(target: &str) -> Option<(TempDir, std::path::PathBuf, std::path::PathBuf)> {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".firebrick.yml");
        let target = dir.path().join(target);
        symlink_file(&target, &path).then_some((dir, path, target))
    }

    /// Creates `link` as a symlink to the file `target`, which may not exist. Returns `false`
    /// when Windows refuses, because creating a symlink takes administrator rights or Developer
    /// Mode there, so the test can stop. With `FIREBRICK_TEST_SYMLINKS` set, as in CI, that
    /// fails the test instead.
    fn symlink_file(target: &Path, link: &Path) -> bool {
        #[cfg(unix)]
        let created = std::os::unix::fs::symlink(target, link);
        #[cfg(windows)]
        let created = std::os::windows::fs::symlink_file(target, link);

        match created {
            Ok(()) => true,
            Err(err) if cfg!(windows) && std::env::var_os("FIREBRICK_TEST_SYMLINKS").is_none() => {
                eprintln!(
                    "can't create a symlink ({err}), so the test stops; set FIREBRICK_TEST_SYMLINKS=1 to fail it"
                );
                false
            }
            Err(err) => panic!("can't create a symlink: {err}"),
        }
    }

    const SYMLINK_ERROR: &str =
        ".firebrick.yml is a symlink; firebrick only uses a regular file in the workspace";

    #[test]
    fn read_text_refuses_a_symlink() {
        let Some((_dir, path, target)) = symlinked_spec("notes.txt") else {
            return;
        };
        fs::write(&target, "name: dev\n").unwrap();

        let err = read_text(&path).unwrap_err();

        assert_eq!(err.to_string(), SYMLINK_ERROR);
    }

    #[test]
    fn read_text_refuses_a_dangling_symlink() {
        let Some((_dir, path, _target)) = symlinked_spec("missing.yml") else {
            return;
        };

        let err = read_text(&path).unwrap_err();

        assert_eq!(err.to_string(), SYMLINK_ERROR);
    }

    #[test]
    fn read_text_returns_none_for_a_missing_file() {
        let dir = TempDir::new().unwrap();

        let text = read_text(&dir.path().join(".firebrick.yml")).unwrap();

        assert_eq!(text, None);
    }

    #[test]
    fn from_file_refuses_a_symlink() {
        let Some((_dir, path, target)) = symlinked_spec("notes.txt") else {
            return;
        };
        fs::write(&target, "name: dev\n").unwrap();

        let result = from_file(&path);

        assert!(matches!(result, Err(SandboxSpecError::Symlink(_))));
    }

    #[test]
    fn write_text_refuses_a_symlink_and_leaves_its_target_alone() {
        let Some((dir, path, target)) = symlinked_spec("notes.txt") else {
            return;
        };
        fs::write(&target, "precious host content\n").unwrap();

        let err = write_text(&path, "name: dev\n").unwrap_err();

        assert_eq!(err.to_string(), SYMLINK_ERROR);
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            "precious host content\n"
        );
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[test]
    fn write_text_refuses_a_dangling_symlink() {
        let Some((_dir, path, target)) = symlinked_spec("missing.yml") else {
            return;
        };

        let err = write_text(&path, "name: dev\n").unwrap_err();

        assert_eq!(err.to_string(), SYMLINK_ERROR);
        assert!(!target.exists());
    }

    #[test]
    fn write_text_replaces_a_regular_file_without_leaving_a_temp_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".firebrick.yml");
        fs::write(&path, "name: old\n").unwrap();

        write_text(&path, "name: new\n").unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "name: new\n");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn env_is_empty_without_an_env_section() {
        for content in ["name: dev\n", "name: dev\nenv:\n", "name: dev\nenv: {}\n"] {
            let spec = from_str(content).unwrap();

            assert!(spec.env.is_empty(), "{content:?}");
        }
    }

    #[test]
    fn env_keeps_values_as_written() {
        let spec = from_str(
            "name: dev\nenv:\n  NODE_ENV: development\n  RUST_LOG: \"debug,hyper=info\"\n  \
             GREETING: \"$HOME\"\n  BRACED: \"${HOME}\"\n  EMPTY: \"\"\n  _under: x\n",
        )
        .unwrap();

        assert_eq!(
            spec.env,
            BTreeMap::from(
                [
                    ("NODE_ENV", "development"),
                    ("RUST_LOG", "debug,hyper=info"),
                    ("GREETING", "$HOME"),
                    ("BRACED", "${HOME}"),
                    ("EMPTY", ""),
                    ("_under", "x"),
                ]
                .map(|(name, value)| (name.to_string(), value.to_string()))
            )
        );
    }

    #[test]
    fn env_turns_numbers_and_booleans_into_text() {
        let spec =
            from_str("name: dev\nenv:\n  PORT: 3000\n  OFFSET: -1\n  DEBUG: true\n  RATIO: 0.5\n")
                .unwrap();

        assert_eq!(spec.env["PORT"], "3000");
        assert_eq!(spec.env["OFFSET"], "-1");
        assert_eq!(spec.env["DEBUG"], "true");
        assert_eq!(spec.env["RATIO"], "0.5");
    }

    #[test]
    fn env_value_that_isnt_a_scalar_returns_diagnostic() {
        let values = [("[1]", 8), ("{a: 1}", 8), ("~", 8), ("null", 8), ("", 7)];

        for (value, column) in values {
            let diagnostic = from_str(&format!("name: dev\nenv:\n  FOO: {value}\n"))
                .unwrap_err()
                .diagnostic()
                .unwrap();

            assert_eq!(
                (diagnostic.line, diagnostic.column),
                (3, column),
                "{value:?}"
            );
            assert_eq!(
                diagnostic.message, "env.FOO: expected a string, number or boolean",
                "{value:?}"
            );
        }
    }

    #[test]
    fn invalid_env_name_returns_diagnostic() {
        for name in ["1X", "A-B", "\"A B\"", "\"\"", "dé", "\"x\\e]0;pwned\\a\""] {
            let diagnostic = from_str(&format!("name: dev\nenv:\n  {name}: x\n"))
                .unwrap_err()
                .diagnostic()
                .unwrap();

            assert_eq!((diagnostic.line, diagnostic.column), (3, 3), "{name:?}");
            assert!(
                diagnostic.message.starts_with("env: invalid variable name"),
                "{}",
                diagnostic.message
            );
            assert!(
                !diagnostic.message.contains('\x1b'),
                "{}",
                diagnostic.message
            );
        }
    }

    #[test]
    fn invalid_env_name_diagnostic_names_the_variable() {
        let diagnostic = from_str("name: dev\nenv:\n  A-B: x\n")
            .unwrap_err()
            .diagnostic()
            .unwrap();

        assert_eq!(
            diagnostic.message,
            "env: invalid variable name \"A-B\": use ASCII letters, digits and '_', not \
             starting with a digit"
        );
    }

    #[test]
    fn reserved_env_name_returns_diagnostic() {
        let diagnostic = from_str("name: dev\nenv:\n  PORT: 1\n  MSB_FOO: x\n")
            .unwrap_err()
            .diagnostic()
            .unwrap();

        assert_eq!((diagnostic.line, diagnostic.column), (4, 3));
        assert_eq!(
            diagnostic.message,
            "env: MSB_FOO: the MSB_ prefix is reserved"
        );
    }

    #[test]
    fn duplicate_env_name_returns_diagnostic() {
        let diagnostic = from_str("name: dev\nenv:\n  A: x\n  A: y\n")
            .unwrap_err()
            .diagnostic()
            .unwrap();

        assert_eq!((diagnostic.line, diagnostic.column), (4, 3));
        assert_eq!(diagnostic.message, "env: A is listed more than once");
    }

    #[test]
    fn env_that_isnt_a_map_returns_diagnostic() {
        let diagnostic = from_str("name: dev\nenv: [A]\n")
            .unwrap_err()
            .diagnostic()
            .unwrap();

        assert_eq!((diagnostic.line, diagnostic.column), (2, 6));
        assert_eq!(
            diagnostic.message,
            "env: invalid type: sequence, expected a map of environment variables"
        );
    }

    #[test]
    fn validate_env_name_accepts_shell_names() {
        for name in ["A", "_", "node_env", "PATH2", "MSB"] {
            assert_eq!(validate_env_name(name), Ok(()), "{name:?}");
        }
    }

    #[test]
    fn validate_env_name_rejects_the_reserved_prefix() {
        assert_eq!(
            validate_env_name("MSB_TOKEN"),
            Err(InvalidEnvName::Reserved("MSB_TOKEN".to_string()))
        );
    }

    #[test]
    fn default_spec_writes_no_env_section() {
        let text = serde_yaml::to_string(&default_spec("dev".to_string())).unwrap();

        assert!(!text.contains("env"), "{text}");
    }
}
