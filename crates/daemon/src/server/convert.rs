//! Conversions between the gRPC messages and the sandbox management types: checks and parses
//! the requests, and turns results, progress, network records and errors into responses.

use crate::api::{
    EnvVars, GitIdentity, ImagePullProgress, MiseInstallProgress, Mount, NetworkActivity,
    NetworkActivityKind, NetworkPolicy, PortForward, PortForwardFailure, PortForwards,
    SandboxResources, SandboxStarted, SandboxStatus, SandboxSummary, SandboxUpgraded,
    SecretSummary, StartSandboxRequest, StartSandboxResponse, UpgradeSandboxResponse,
    start_sandbox_response, upgrade_sandbox_response,
};
use crate::forward::ForwardFailure;
use crate::guest::netlog::{ConnectionEntry, DnsEntry, Protocol, Record};
use crate::sandboxes::provision::git;
use crate::sandboxes::provision::mise::MiseUpdate;
use crate::sandboxes::provision::network;
use crate::sandboxes::provision::pull::PullUpdate;
use crate::sandboxes::{
    Resources, SandboxError, SandboxInfo, StartProgress, StartSandbox, Started, Upgrade,
};
use crate::secrets::Secret;
use anyhow::Result;
use firebrick_spec::{
    MountSpec, NetworkRule, NetworkSpec, PortMapping, SandboxResourcesSpec, VolumesSpec,
};
use std::collections::{BTreeMap, HashSet};
use tonic::Status;
use url::Host;

/// The settings of a start request that fbkd parses before it starts the sandbox.
#[derive(Debug, Default)]
pub(super) struct ParsedStart {
    network: NetworkSpec,
    ports: Option<Vec<PortMapping>>,
    mounts: Vec<MountSpec>,
    env: Option<BTreeMap<String, String>>,
}

impl ParsedStart {
    /// Checks the name and image of a start request and parses its settings, failing with
    /// `InvalidArgument` when one of them is invalid.
    pub(super) fn of(request: &StartSandboxRequest) -> Result<Self, Status> {
        check_name(&request.name)?;
        check_image(&request.image)?;

        Ok(Self {
            network: network_spec(request.network.as_ref())?,
            ports: request.ports.as_ref().map(port_mappings).transpose()?,
            mounts: mount_specs(&request.mounts),
            env: request.env.as_ref().map(env_vars).transpose()?,
        })
    }
}

/// Reads the sandbox to start from a request with its parsed settings. A request without
/// `init` runs the image's init, and one without `mise` installs the workspace's mise tools. A
/// request without `clipboard` doesn't share the host clipboard.
pub(super) fn start_sandbox_from<'a>(
    request: &'a StartSandboxRequest,
    parsed: &'a ParsedStart,
) -> StartSandbox<'a> {
    StartSandbox {
        name: &request.name,
        workspace: &request.workspace,
        image: &request.image,
        init: request.init.unwrap_or(true),
        mise: request.mise.unwrap_or(true),
        network: &parsed.network,
        ports: parsed.ports.as_deref(),
        mounts: &parsed.mounts,
        dotfiles: Some(request.mise_config.as_str()).filter(|path| !path.is_empty()),
        clipboard: request.clipboard.unwrap_or(false),
        env: parsed.env.as_ref(),
        git_identity: request.git_identity.as_ref().map(git_identity),
    }
}

/// Reads the git identity of a request.
fn git_identity(identity: &GitIdentity) -> git::GitIdentity<'_> {
    git::GitIdentity {
        name: identity.name.as_deref(),
        email: identity.email.as_deref(),
    }
}

/// Reads the environment variables of a request, failing with `InvalidArgument` on a name
/// that [`firebrick_spec::validate_env_name`] refuses or a value with a NUL character.
fn env_vars(env: &EnvVars) -> Result<BTreeMap<String, String>, Status> {
    env.vars
        .iter()
        .map(|(name, value)| {
            firebrick_spec::validate_env_name(name)
                .map_err(|err| Status::invalid_argument(err.to_string()))?;

            if value.contains('\0') {
                return Err(Status::invalid_argument(format!(
                    "env variable {name} has a NUL character"
                )));
            }

            Ok((name.clone(), value.clone()))
        })
        .collect()
}

/// Parses the ports of a request, failing with `InvalidArgument` on a port outside 1 to 65535
/// or a host port that is listed more than once.
fn port_mappings(ports: &PortForwards) -> Result<Vec<PortMapping>, Status> {
    let mut host_ports = HashSet::new();

    ports
        .ports
        .iter()
        .map(|port| {
            let mapping = PortMapping {
                host: port_number(port.host)?,
                guest: port_number(port.guest)?,
            };

            if !host_ports.insert(mapping.host) {
                return Err(Status::invalid_argument(format!(
                    "host port {} is listed more than once",
                    mapping.host
                )));
            }

            Ok(mapping)
        })
        .collect()
}

/// Converts a port from the request, failing with `InvalidArgument` outside 1 to 65535.
pub(super) fn port_number(port: u32) -> Result<u16, Status> {
    u16::try_from(port)
        .ok()
        .filter(|port| *port > 0)
        .ok_or_else(|| Status::invalid_argument(format!("invalid port {port}: use 1 to 65535")))
}

/// Converts the forwards of a sandbox to the message that ends the start stream.
pub(super) fn sandbox_started(started: Started) -> SandboxStarted {
    let report = started.forwards;

    SandboxStarted {
        env_pending_restart: started.env_pending_restart,
        forwards: report.open.into_iter().map(port_forward).collect(),
        failed_forwards: report
            .failed
            .into_iter()
            .map(|ForwardFailure { port, reason }| PortForwardFailure {
                port: Some(port_forward(port)),
                reason,
            })
            .collect(),
    }
}

/// Wraps a message in the response of the start stream.
pub(super) fn start_sandbox_response(
    message: start_sandbox_response::Message,
) -> StartSandboxResponse {
    StartSandboxResponse {
        message: Some(message),
    }
}

/// Converts the progress of an image pull to its protobuf message.
pub(super) fn image_pull_progress(update: PullUpdate) -> ImagePullProgress {
    ImagePullProgress {
        image: update.image,
        downloaded_bytes: update.downloaded_bytes,
        total_bytes: update.total_bytes,
        complete: update.complete,
    }
}

/// Converts the progress of a start to a response of the start stream.
pub(super) fn start_progress_response(progress: StartProgress) -> StartSandboxResponse {
    match progress {
        StartProgress::Pull(update) => pull_progress_response(update),
        StartProgress::Mise(update) => mise_progress_response(update),
    }
}

/// Converts the progress of installing the mise tools to a response of the start stream.
fn mise_progress_response(update: MiseUpdate) -> StartSandboxResponse {
    start_sandbox_response(start_sandbox_response::Message::MiseProgress(
        MiseInstallProgress {
            line: update.line,
            complete: update.complete,
            warning: update.warning.unwrap_or_default(),
        },
    ))
}

/// Converts the progress of an image pull to a response of the start stream.
fn pull_progress_response(update: PullUpdate) -> StartSandboxResponse {
    start_sandbox_response(start_sandbox_response::Message::PullProgress(
        image_pull_progress(update),
    ))
}

/// Wraps a message in the response of the upgrade stream.
pub(super) fn upgrade_sandbox_response(
    message: upgrade_sandbox_response::Message,
) -> UpgradeSandboxResponse {
    UpgradeSandboxResponse {
        message: Some(message),
    }
}

/// Converts the result of an upgrade to the message that ends the upgrade stream.
pub(super) fn sandbox_upgraded(upgrade: Upgrade) -> UpgradeSandboxResponse {
    upgrade_sandbox_response(upgrade_sandbox_response::Message::Upgraded(
        SandboxUpgraded {
            upgraded: upgrade.upgraded,
            old_image: upgrade.old_image,
            new_image: upgrade.new_image,
        },
    ))
}

/// Converts a mapping to its protobuf message.
fn port_forward(port: PortMapping) -> PortForward {
    PortForward {
        host: port.host.into(),
        guest: port.guest.into(),
    }
}

/// Reads the extra mounts of a request. The sandbox manager checks their paths.
fn mount_specs(mounts: &[Mount]) -> Vec<MountSpec> {
    mounts.iter().map(MountSpec::from).collect()
}

/// Parses the egress rules of a request. A request without them gets microsandbox's default
/// policy. Fails with `InvalidArgument` on the first invalid rule.
pub(super) fn network_spec(network: Option<&NetworkPolicy>) -> Result<NetworkSpec, Status> {
    let Some(network) = network else {
        return Ok(NetworkSpec::default());
    };

    Ok(NetworkSpec {
        enabled: network.enabled,
        enforce: network.enforce,
        allow: parse_rules(&network.allow)?,
        deny: parse_rules(&network.deny)?,
    })
}

/// Parses network rules, failing with `InvalidArgument` on the first invalid one.
fn parse_rules(rules: &[String]) -> Result<Vec<NetworkRule>, Status> {
    rules
        .iter()
        .map(|rule| {
            rule.parse::<NetworkRule>()
                .map_err(|err| Status::invalid_argument(err.to_string()))
        })
        .collect()
}

/// Converts the record of a sandbox to its entries, sorted by last seen, newest first, each
/// with whether the rules allow its destination: domain rules for a DNS lookup, IP and CIDR
/// rules for a connection.
pub(super) fn network_activity(record: Record, rules: &NetworkSpec) -> Vec<NetworkActivity> {
    let dns = record
        .dns
        .into_iter()
        .map(|entry| dns_activity(entry, rules));
    let connections = record
        .connections
        .into_iter()
        .map(|entry| connection_activity(entry, rules));
    let mut entries: Vec<NetworkActivity> = dns.chain(connections).collect();

    entries.sort_by_key(|entry| std::cmp::Reverse(entry.last_seen));

    entries
}

/// Converts a DNS lookup of the record to its protobuf entry.
fn dns_activity(entry: DnsEntry, rules: &NetworkSpec) -> NetworkActivity {
    NetworkActivity {
        kind: NetworkActivityKind::Dns.into(),
        allowed: network::allows_host(rules, &Host::Domain(entry.name.as_str())),
        destination: entry.name,
        addresses: entry.addresses.iter().map(ToString::to_string).collect(),
        ports: Vec::new(),
        count: entry.count,
        first_seen: entry.first_seen.timestamp(),
        last_seen: entry.last_seen.timestamp(),
    }
}

/// Converts the connection attempts of the record to their protobuf entry.
fn connection_activity(entry: ConnectionEntry, rules: &NetworkSpec) -> NetworkActivity {
    let kind = match entry.protocol {
        Protocol::Tcp => NetworkActivityKind::Tcp,
        Protocol::Udp => NetworkActivityKind::Udp,
    };

    NetworkActivity {
        kind: kind.into(),
        destination: entry.ip.to_string(),
        addresses: Vec::new(),
        ports: entry.ports.into_iter().map(u32::from).collect(),
        count: entry.count,
        first_seen: entry.first_seen.timestamp(),
        last_seen: entry.last_seen.timestamp(),
        allowed: network::allows_host(rules, &Host::Ipv4(entry.ip)),
    }
}

impl From<SandboxError> for Status {
    fn from(err: SandboxError) -> Self {
        match err {
            SandboxError::NotFound(message) => Status::not_found(message),
            SandboxError::InvalidArgument(message) => Status::invalid_argument(message),
            SandboxError::FailedPrecondition(message) => Status::failed_precondition(message),
            SandboxError::Unavailable(message) => Status::unavailable(message),
            SandboxError::Internal(message) => Status::internal(message),
        }
    }
}

/// Fails with `InvalidArgument` when the sandbox name isn't one microsandbox accepts. fbkd names
/// files after sandboxes, so every request checks the name before it touches the file system or
/// microsandbox.
pub(super) fn check_name(name: &str) -> Result<(), Status> {
    firebrick_spec::validate_sandbox_name(name)
        .map_err(|err| Status::invalid_argument(err.to_string()))
}

/// Fails with `InvalidArgument` when the image isn't an OCI reference. Empty means the default
/// image. fbkd shows the image in status messages and logs, so a request checks it before the
/// image reaches either.
pub(super) fn check_image(image: &str) -> Result<(), Status> {
    if image.is_empty() {
        return Ok(());
    }

    firebrick_spec::validate_image(image).map_err(|err| Status::invalid_argument(err.to_string()))
}

/// Converts a sandbox to its protobuf summary.
pub(super) fn sandbox_summary(sandbox: SandboxInfo) -> SandboxSummary {
    SandboxSummary {
        name: sandbox.name,
        status: map_sandbox_status(sandbox.status),
        hostname: sandbox.hostname.unwrap_or_default(),
    }
}

/// Converts a secret to its protobuf summary, without its value.
pub(super) fn secret_summary(secret: &Secret) -> SecretSummary {
    SecretSummary {
        name: secret.name().to_string(),
        allowed_hosts: secret.allowed_hosts().to_vec(),
        sandbox: secret.sandbox().map(str::to_string),
    }
}

/// Converts the requested resources and volumes to vCPUs and MiB, using the defaults for
/// missing resources, missing volumes and an empty volume size.
pub(super) fn sandbox_resources(request: &StartSandboxRequest) -> Result<Resources, SandboxError> {
    let resources = request.resources.clone().unwrap_or_else(|| {
        let defaults = SandboxResourcesSpec::default();

        SandboxResources {
            cpu: defaults.cpu.into(),
            memory: defaults.memory,
        }
    });
    let docker_volume = request
        .volumes
        .as_ref()
        .map(|volumes| volumes.docker.clone())
        .filter(|size| !size.is_empty())
        .unwrap_or_else(|| VolumesSpec::default().docker);

    let cpus = u8::try_from(resources.cpu)
        .ok()
        .filter(|cpus| *cpus > 0)
        .ok_or_else(|| SandboxError::InvalidArgument("cpu must be between 1 and 255".into()))?;

    Ok(Resources {
        cpus,
        memory_mib: parse_size_mib(&resources.memory)?,
        docker_volume_mib: parse_size_mib(&docker_volume)?,
    })
}

/// Parses a requested size into MiB, reporting a bad size as an invalid argument.
fn parse_size_mib(size: &str) -> Result<u32, SandboxError> {
    firebrick_spec::parse_size_mib(size)
        .map_err(|err| SandboxError::InvalidArgument(err.to_string()))
}

/// Converts a microsandbox status to its protobuf status value, and a missing one to `Unknown`.
pub(super) fn map_sandbox_status(s: Option<microsandbox::sandbox::SandboxStatus>) -> i32 {
    let status = match s {
        Some(microsandbox::sandbox::SandboxStatus::Running) => SandboxStatus::Running,
        Some(microsandbox::sandbox::SandboxStatus::Stopped) => SandboxStatus::Stopped,
        Some(microsandbox::sandbox::SandboxStatus::Created) => SandboxStatus::Stopped,
        Some(microsandbox::sandbox::SandboxStatus::Starting) => SandboxStatus::Starting,
        Some(microsandbox::sandbox::SandboxStatus::Paused) => SandboxStatus::Paused,
        Some(microsandbox::sandbox::SandboxStatus::Draining) => SandboxStatus::Stopping,
        Some(microsandbox::sandbox::SandboxStatus::Crashed) => SandboxStatus::Crashed,
        None => SandboxStatus::Unknown,
    };

    status.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::SandboxVolumes;
    use crate::forward::ForwardReport;
    use microsandbox::sandbox::SandboxStatus as MsbStatus;

    #[test]
    fn map_sandbox_status_maps_every_variant() {
        let cases = [
            (Some(MsbStatus::Running), SandboxStatus::Running),
            (Some(MsbStatus::Stopped), SandboxStatus::Stopped),
            (Some(MsbStatus::Created), SandboxStatus::Stopped),
            (Some(MsbStatus::Starting), SandboxStatus::Starting),
            (Some(MsbStatus::Paused), SandboxStatus::Paused),
            (Some(MsbStatus::Draining), SandboxStatus::Stopping),
            (Some(MsbStatus::Crashed), SandboxStatus::Crashed),
            (None, SandboxStatus::Unknown),
        ];

        for (input, expected) in cases {
            assert_eq!(map_sandbox_status(input), expected as i32);
        }
    }

    #[test]
    fn map_sandbox_status_produces_valid_proto_values() {
        let value = map_sandbox_status(Some(MsbStatus::Draining));
        assert_eq!(SandboxStatus::try_from(value), Ok(SandboxStatus::Stopping));
    }

    #[test]
    fn start_sandbox_from_request_defaults_init_to_true() {
        let request = StartSandboxRequest::default();

        assert!(start_sandbox_from(&request, &ParsedStart::default()).init);
    }

    #[test]
    fn start_sandbox_from_request_defaults_mise_to_true() {
        let request = StartSandboxRequest::default();

        assert!(start_sandbox_from(&request, &ParsedStart::default()).mise);
    }

    #[test]
    fn start_sandbox_from_request_keeps_mise() {
        let request = StartSandboxRequest {
            mise: Some(false),
            ..Default::default()
        };

        assert!(!start_sandbox_from(&request, &ParsedStart::default()).mise);
    }

    #[test]
    fn start_sandbox_from_request_keeps_init() {
        let request = StartSandboxRequest {
            name: "dev".to_string(),
            image: "alpine:3.22".to_string(),
            workspace: "/home/user/project".to_string(),
            init: Some(false),
            ..Default::default()
        };

        let parsed = ParsedStart::default();
        let start = start_sandbox_from(&request, &parsed);

        assert_eq!(
            (start.name, start.image, start.workspace, start.init),
            ("dev", "alpine:3.22", "/home/user/project", false)
        );
    }

    #[test]
    fn port_mappings_converts_the_ports() {
        let ports = PortForwards {
            ports: vec![
                PortForward {
                    host: 3000,
                    guest: 3000,
                },
                PortForward {
                    host: 8080,
                    guest: 5173,
                },
            ],
        };

        assert_eq!(
            port_mappings(&ports).unwrap(),
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
    fn port_mappings_rejects_invalid_and_duplicate_ports() {
        let port = |host, guest| PortForward { host, guest };

        for ports in [
            vec![port(0, 80)],
            vec![port(80, 0)],
            vec![port(65536, 80)],
            vec![port(80, 65536)],
            vec![port(80, 80), port(80, 81)],
        ] {
            let status = port_mappings(&PortForwards {
                ports: ports.clone(),
            })
            .unwrap_err();

            assert_eq!(status.code(), tonic::Code::InvalidArgument, "{ports:?}");
        }
    }

    fn mise_update(line: &str, complete: bool) -> StartProgress {
        StartProgress::Mise(MiseUpdate {
            line: line.to_string(),
            complete,
            warning: None,
        })
    }

    fn mise_response(line: &str, complete: bool) -> StartSandboxResponse {
        start_sandbox_response(start_sandbox_response::Message::MiseProgress(
            MiseInstallProgress {
                line: line.to_string(),
                complete,
                warning: String::new(),
            },
        ))
    }

    fn pull_update() -> StartProgress {
        StartProgress::Pull(PullUpdate {
            image: "alpine:3.22".to_string(),
            downloaded_bytes: 100,
            total_bytes: Some(400),
            complete: false,
        })
    }

    fn pull_response() -> StartSandboxResponse {
        StartSandboxResponse {
            message: Some(start_sandbox_response::Message::PullProgress(
                ImagePullProgress {
                    image: "alpine:3.22".to_string(),
                    downloaded_bytes: 100,
                    total_bytes: Some(400),
                    complete: false,
                },
            )),
        }
    }

    #[test]
    fn start_progress_response_converts_each_kind_of_progress() {
        let cases = [
            (
                mise_update("mise node@22 download", false),
                mise_response("mise node@22 download", false),
            ),
            (mise_update("", true), mise_response("", true)),
            (pull_update(), pull_response()),
        ];

        for (progress, expected) in cases {
            assert_eq!(
                start_progress_response(progress.clone()),
                expected,
                "{progress:?}"
            );
        }
    }

    #[test]
    fn pull_progress_response_keeps_the_progress() {
        let update = PullUpdate {
            image: "alpine:3.22".to_string(),
            downloaded_bytes: 100,
            total_bytes: None,
            complete: true,
        };

        assert_eq!(
            pull_progress_response(update),
            StartSandboxResponse {
                message: Some(start_sandbox_response::Message::PullProgress(
                    ImagePullProgress {
                        image: "alpine:3.22".to_string(),
                        downloaded_bytes: 100,
                        total_bytes: None,
                        complete: true,
                    }
                )),
            }
        );
    }

    #[test]
    fn sandbox_started_lists_open_and_failed_forwards() {
        let mapping = |host, guest| PortMapping { host, guest };
        let report = ForwardReport {
            open: vec![mapping(8080, 5173)],
            failed: vec![ForwardFailure {
                port: mapping(3000, 3000),
                reason: "Address in use".to_string(),
            }],
        };

        let response = sandbox_started(Started {
            forwards: report,
            env_pending_restart: true,
        });

        assert!(response.env_pending_restart);
        assert_eq!(response.forwards, [port_forward(mapping(8080, 5173))]);
        assert_eq!(
            response.failed_forwards,
            [PortForwardFailure {
                port: Some(port_forward(mapping(3000, 3000))),
                reason: "Address in use".to_string(),
            }]
        );
    }

    fn env_request(pairs: &[(&str, &str)]) -> EnvVars {
        EnvVars {
            vars: pairs
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect(),
        }
    }

    #[test]
    fn env_vars_keep_names_and_values() {
        let env = env_vars(&env_request(&[("GREETING", "$HOME"), ("EMPTY", "")])).unwrap();

        assert_eq!(
            env,
            BTreeMap::from([
                ("EMPTY".to_string(), String::new()),
                ("GREETING".to_string(), "$HOME".to_string()),
            ])
        );
    }

    #[test]
    fn env_vars_reject_invalid_names() {
        for name in ["1X", "A-B", ""] {
            let status = env_vars(&env_request(&[(name, "x")])).unwrap_err();

            assert_eq!(status.code(), tonic::Code::InvalidArgument, "{name:?}");
            assert!(
                status
                    .message()
                    .starts_with(&format!("invalid variable name {name:?}")),
                "{}",
                status.message()
            );
        }
    }

    #[test]
    fn env_vars_reject_reserved_names() {
        let status = env_vars(&env_request(&[("MSB_TOKEN", "x")])).unwrap_err();

        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert_eq!(status.message(), "MSB_TOKEN: the MSB_ prefix is reserved");
    }

    #[test]
    fn env_vars_reject_a_nul_character() {
        let status = env_vars(&env_request(&[("A", "a\0b")])).unwrap_err();

        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert_eq!(status.message(), "env variable A has a NUL character");
    }

    #[test]
    fn start_sandbox_from_request_keeps_env() {
        let request = StartSandboxRequest::default();
        let env = BTreeMap::from([("A".to_string(), "1".to_string())]);
        let parsed = ParsedStart {
            env: Some(env.clone()),
            ..ParsedStart::default()
        };

        let start = start_sandbox_from(&request, &parsed);

        assert_eq!(start.env, Some(&env));
    }

    #[test]
    fn mount_specs_keep_the_paths_and_readonly() {
        let mounts = [Mount {
            host: "/home/user/lib".to_string(),
            guest: "/workspaces/lib".to_string(),
            readonly: true,
        }];

        assert_eq!(
            mount_specs(&mounts),
            [MountSpec {
                host: "/home/user/lib".to_string(),
                guest: "/workspaces/lib".to_string(),
                readonly: true,
            }]
        );
    }

    #[test]
    fn network_spec_without_network_does_not_enforce() {
        assert_eq!(network_spec(None).unwrap(), NetworkSpec::default());
    }

    #[test]
    fn network_spec_parses_the_rules() {
        let network = NetworkPolicy {
            enforce: true,
            allow: vec!["*.github.com".to_string(), "10.0.0.0/8".to_string()],
            deny: vec!["gist.github.com".to_string()],
            ..Default::default()
        };

        let spec = network_spec(Some(&network)).unwrap();

        assert!(spec.enforce);
        assert!(spec.is_enabled());
        assert_eq!(
            spec.allow,
            [
                NetworkRule::DomainSuffix("github.com".to_string()),
                NetworkRule::Cidr {
                    address: "10.0.0.0".parse().unwrap(),
                    prefix: 8
                },
            ]
        );
        assert_eq!(
            spec.deny,
            [NetworkRule::Domain("gist.github.com".to_string())]
        );
    }

    #[test]
    fn network_spec_passes_on_the_network_switch() {
        for enabled in [None, Some(true), Some(false)] {
            let network = NetworkPolicy {
                enabled,
                ..Default::default()
            };

            assert_eq!(network_spec(Some(&network)).unwrap().enabled, enabled);
        }
    }

    #[test]
    fn network_spec_rejects_invalid_rules() {
        for network in [
            NetworkPolicy {
                allow: vec!["https://github.com".to_string()],
                ..Default::default()
            },
            NetworkPolicy {
                deny: vec!["https://github.com".to_string()],
                ..Default::default()
            },
        ] {
            let status = network_spec(Some(&network)).unwrap_err();

            assert_eq!(status.code(), tonic::Code::InvalidArgument);
            assert_eq!(
                status.message(),
                "invalid network rule \"https://github.com\": use a host name, *.domain, an IP \
                 address or a CIDR range"
            );
        }
    }

    #[test]
    fn sandbox_errors_map_to_status_codes_and_messages() {
        let cases = [
            (SandboxError::NotFound("a".into()), tonic::Code::NotFound),
            (
                SandboxError::InvalidArgument("b".into()),
                tonic::Code::InvalidArgument,
            ),
            (
                SandboxError::FailedPrecondition("c".into()),
                tonic::Code::FailedPrecondition,
            ),
            (
                SandboxError::Unavailable("e".into()),
                tonic::Code::Unavailable,
            ),
            (SandboxError::Internal("d".into()), tonic::Code::Internal),
        ];

        for (err, code) in cases {
            let message = err.to_string();
            let status = Status::from(err);

            assert_eq!(status.code(), code);
            assert_eq!(status.message(), message);
        }
    }

    /// Returns a start request with the resources and Docker volume size.
    fn resources_request(cpu: u32, memory: &str, docker: Option<&str>) -> StartSandboxRequest {
        StartSandboxRequest {
            resources: Some(SandboxResources {
                cpu,
                memory: memory.to_string(),
            }),
            volumes: docker.map(|docker| SandboxVolumes {
                docker: docker.to_string(),
            }),
            ..Default::default()
        }
    }

    #[test]
    fn sandbox_resources_converts_request() {
        let request = resources_request(4, "8 GiB", Some("40 GiB"));

        assert_eq!(
            sandbox_resources(&request).unwrap(),
            Resources {
                cpus: 4,
                memory_mib: 8192,
                docker_volume_mib: 40960,
            }
        );
    }

    #[test]
    fn sandbox_resources_uses_default_docker_volume_when_missing_or_empty() {
        for docker in [None, Some("")] {
            let request = resources_request(4, "8 GiB", docker);

            assert_eq!(
                sandbox_resources(&request).unwrap().docker_volume_mib,
                20480,
                "{docker:?}"
            );
        }
    }

    #[test]
    fn sandbox_resources_falls_back_to_defaults() {
        assert_eq!(
            sandbox_resources(&StartSandboxRequest::default()).unwrap(),
            Resources {
                cpus: 2,
                memory_mib: 4096,
                docker_volume_mib: 20480,
            }
        );
    }

    #[test]
    fn sandbox_resources_rejects_invalid_values() {
        let cases = [
            (0, "1 GiB", None),
            (256, "1 GiB", None),
            (2, "lots", None),
            (2, "", None),
            (2, "1 GiB", Some("20 GB")),
            (2, "1 GiB", Some("0 GiB")),
        ];

        for (cpu, memory, docker) in cases {
            let request = resources_request(cpu, memory, docker);

            assert_eq!(
                Status::from(sandbox_resources(&request).unwrap_err()).code(),
                tonic::Code::InvalidArgument,
                "cpu {cpu}, memory {memory:?}, docker {docker:?} should be rejected"
            );
        }
    }

    /// Returns the time `seconds` after 2026-10-11 14:00:00 UTC.
    fn at(seconds: i64) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp(1_791_727_200 + seconds, 0).unwrap()
    }

    fn dns_entry(name: &str, last_seen: i64) -> DnsEntry {
        DnsEntry {
            name: name.to_string(),
            addresses: ["140.82.112.6".parse().unwrap()].into(),
            count: 12,
            first_seen: at(0),
            last_seen: at(last_seen),
        }
    }

    fn connection_entry(ip: &str, protocol: Protocol, last_seen: i64) -> ConnectionEntry {
        ConnectionEntry {
            ip: ip.parse().unwrap(),
            protocol,
            ports: [443, 80].into(),
            count: 3,
            first_seen: at(0),
            last_seen: at(last_seen),
        }
    }

    fn record() -> Record {
        Record {
            dns: vec![
                dns_entry("api.github.com", 131),
                dns_entry("evil.example.com", 220),
            ],
            connections: vec![
                connection_entry("140.82.112.6", Protocol::Tcp, 130),
                connection_entry("10.0.0.1", Protocol::Udp, 5),
            ],
        }
    }

    fn enforced(allow: &[&str]) -> NetworkSpec {
        NetworkSpec {
            enforce: true,
            allow: allow.iter().map(|rule| rule.parse().unwrap()).collect(),
            ..NetworkSpec::default()
        }
    }

    /// Describes each entry as `<destination> <allowed>`.
    fn allowed(entries: &[NetworkActivity]) -> Vec<String> {
        entries
            .iter()
            .map(|entry| format!("{} {}", entry.destination, entry.allowed))
            .collect()
    }

    #[test]
    fn network_activity_sorts_entries_newest_first() {
        let entries = network_activity(record(), &NetworkSpec::default());
        let destinations: Vec<&str> = entries.iter().map(|e| e.destination.as_str()).collect();

        assert_eq!(
            destinations,
            [
                "evil.example.com",
                "api.github.com",
                "140.82.112.6",
                "10.0.0.1"
            ]
        );
    }

    #[test]
    fn network_activity_converts_the_entries() {
        let entries = network_activity(record(), &NetworkSpec::default());

        assert_eq!(
            entries[1],
            NetworkActivity {
                kind: NetworkActivityKind::Dns.into(),
                destination: "api.github.com".to_string(),
                addresses: vec!["140.82.112.6".to_string()],
                ports: vec![],
                count: 12,
                first_seen: 1_791_727_200,
                last_seen: 1_791_727_331,
                allowed: true,
            }
        );
        assert_eq!(
            entries[3],
            NetworkActivity {
                kind: NetworkActivityKind::Udp.into(),
                destination: "10.0.0.1".to_string(),
                addresses: vec![],
                ports: vec![80, 443],
                count: 3,
                first_seen: 1_791_727_200,
                last_seen: 1_791_727_205,
                allowed: true,
            }
        );
        assert_eq!(entries[2].kind(), NetworkActivityKind::Tcp);
    }

    #[test]
    fn network_activity_evaluates_domain_rules_for_dns_and_ip_rules_for_connections() {
        let rules = enforced(&["api.github.com", "140.82.112.0/20"]);

        assert_eq!(
            allowed(&network_activity(record(), &rules)),
            [
                "evil.example.com false",
                "api.github.com true",
                "140.82.112.6 true",
                "10.0.0.1 false"
            ]
        );
    }

    #[test]
    fn network_activity_allows_every_entry_when_the_rules_are_not_enforced() {
        let rules = NetworkSpec {
            enforce: false,
            ..enforced(&["api.github.com"])
        };

        let entries = network_activity(record(), &rules);

        assert!(entries.iter().all(|entry| entry.allowed), "{entries:?}");
    }

    #[test]
    fn network_activity_of_an_empty_record_is_empty() {
        assert_eq!(
            network_activity(Record::default(), &NetworkSpec::default()),
            []
        );
    }

    #[test]
    fn sandbox_upgraded_keeps_the_result() {
        let response = sandbox_upgraded(Upgrade {
            upgraded: true,
            old_image: "alpine:3.21".to_string(),
            new_image: "alpine:3.22".to_string(),
        });

        assert_eq!(
            response.message,
            Some(upgrade_sandbox_response::Message::Upgraded(
                SandboxUpgraded {
                    upgraded: true,
                    old_image: "alpine:3.21".to_string(),
                    new_image: "alpine:3.22".to_string(),
                }
            ))
        );
    }
}
