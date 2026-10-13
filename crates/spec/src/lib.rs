use std::collections::HashSet;
use std::fmt;
use std::net::IpAddr;
use std::str::FromStr;
use std::{fs, path::Path};

use serde::de::{self, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use std::result::Result;
use thiserror::Error;

/// Errors that can occur while loading a sandbox spec.
#[derive(Error, Debug)]
pub enum SandboxSpecError {
    #[error("can't find the source file")]
    FileNotFound,
    #[error("can't read contents of the input file")]
    CantReadInputFile(#[from] std::io::Error),
    #[error("can't parse input yaml: {0}")]
    InvalidSpec(#[from] serde_yaml::Error),
    #[error("can't serialize the spec")]
    CantSerialize(#[source] serde_yaml::Error),
    #[error("can't write the spec file")]
    CantWriteFile(#[source] std::io::Error),
}

/// Errors that can occur while parsing a memory or disk size.
#[derive(Error, Debug, PartialEq, Eq)]
pub enum SizeError {
    #[error(
        "invalid size `{0}`, expected a positive number with a unit, such as `512 MiB` or `4Gi`"
    )]
    Invalid(String),
    #[error("size `{0}` is too large")]
    TooLarge(String),
}

/// Image a sandbox runs when its spec doesn't name one: the `firebrick-base` image that the
/// release workflow publishes with the same version as this crate.
pub const DEFAULT_IMAGE: &str = concat!(
    "ghcr.io/wmeints/firebrick-base:v",
    env!("CARGO_PKG_VERSION")
);

/// A problem in a spec file, pinned to the 1-based line and column it occurs at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecDiagnostic {
    pub line: usize,
    pub column: usize,
    pub message: String,
}

impl SandboxSpecError {
    /// Returns the position and description of the problem when the spec content is invalid.
    pub fn diagnostic(&self) -> Option<SpecDiagnostic> {
        let SandboxSpecError::InvalidSpec(err) = self else {
            return None;
        };

        // Errors without a location concern the document as a whole.
        let (line, column) = err
            .location()
            .map_or((1, 1), |location| (location.line(), location.column()));

        // The yaml error message ends with the position, which we report separately.
        let message = err.to_string();
        let suffix = format!(" at line {line} column {column}");
        let message = message
            .strip_suffix(&suffix)
            .unwrap_or(&message)
            .to_string();

        Some(SpecDiagnostic {
            line,
            column,
            message,
        })
    }
}

/// Describes a sandbox as configured in a spec file.
#[derive(Serialize, Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct SandboxSpec {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resources: Option<SandboxResourcesSpec>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Whether the sandbox runs the image's `/sbin/init` as PID 1. Defaults to `true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub init: Option<bool>,
    /// Whether fbkd trusts and installs the workspace's mise tools when the sandbox starts.
    /// Defaults to `true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mise: Option<bool>,
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
}

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

/// Network settings of a sandbox. The rules only apply when the network is enabled and
/// `enforce` is `true`, but they are always validated.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NetworkSpec {
    /// Whether the sandbox gets a network device at all. Defaults to `true`; see
    /// [`NetworkSpec::is_enabled`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Whether outgoing traffic is denied unless a rule allows it. Defaults to `false`.
    #[serde(default)]
    pub enforce: bool,
    /// Destinations the sandbox may connect to.
    #[serde(default)]
    pub allow: Vec<NetworkRule>,
    /// Destinations the sandbox may not connect to, even when an `allow` rule matches them.
    #[serde(default)]
    pub deny: Vec<NetworkRule>,
}

impl NetworkSpec {
    /// Returns whether the sandbox gets a network device, which it does unless `enabled` is
    /// `false`.
    pub fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }

    /// Adds the rules to `allow` that aren't in it yet, and removes them from `deny`. Returns
    /// whether the rules changed.
    pub fn allow(&mut self, rules: &[NetworkRule]) -> bool {
        move_rules(rules, &mut self.allow, &mut self.deny)
    }

    /// Adds the rules to `deny` that aren't in it yet, and removes them from `allow`. Returns
    /// whether the rules changed.
    pub fn deny(&mut self, rules: &[NetworkRule]) -> bool {
        move_rules(rules, &mut self.deny, &mut self.allow)
    }
}

/// Adds the rules to `to` that aren't in it yet and removes them from `from`. Returns whether
/// either list changed.
fn move_rules(
    rules: &[NetworkRule],
    to: &mut Vec<NetworkRule>,
    from: &mut Vec<NetworkRule>,
) -> bool {
    let before = from.len();
    from.retain(|rule| !rules.contains(rule));
    let mut changed = from.len() != before;

    for rule in rules {
        if !to.contains(rule) {
            to.push(rule.clone());
            changed = true;
        }
    }

    changed
}

/// The destination of a network rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkRule {
    /// Exactly this host name, such as `github.com`.
    Domain(String),
    /// The domain and every subdomain, written as `*.example.com`.
    DomainSuffix(String),
    /// A single IPv4 or IPv6 address.
    Ip(IpAddr),
    /// A CIDR range, such as `192.168.10.0/24`.
    Cidr {
        /// Address of the range.
        address: IpAddr,
        /// Number of leading bits of the address that the range fixes.
        prefix: u8,
    },
}

/// A network rule that isn't a host name, `*.domain`, IP address or CIDR range.
#[derive(Error, Debug, Clone, PartialEq, Eq)]
#[error("invalid network rule \"{0}\": use a host name, *.domain, an IP address or a CIDR range")]
pub struct InvalidNetworkRule(pub String);

impl FromStr for NetworkRule {
    type Err = InvalidNetworkRule;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        parse_rule(value).ok_or_else(|| InvalidNetworkRule(value.to_string()))
    }
}

impl fmt::Display for NetworkRule {
    /// Writes the rule the way it is written in the spec.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NetworkRule::Domain(domain) => f.write_str(domain),
            NetworkRule::DomainSuffix(domain) => write!(f, "*.{domain}"),
            NetworkRule::Ip(address) => write!(f, "{address}"),
            NetworkRule::Cidr { address, prefix } => write!(f, "{address}/{prefix}"),
        }
    }
}

impl Serialize for NetworkRule {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for NetworkRule {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_str(NetworkRuleVisitor)
    }
}

/// Parses the rule while the parser still points at it, so a problem is reported at the
/// rule's line and column.
struct NetworkRuleVisitor;

impl Visitor<'_> for NetworkRuleVisitor {
    type Value = NetworkRule;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a host name, *.domain, an IP address or a CIDR range")
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<NetworkRule, E> {
        value.parse().map_err(E::custom)
    }
}

/// Parses a rule, or returns `None` when it has none of the supported forms.
fn parse_rule(value: &str) -> Option<NetworkRule> {
    if let Some(domain) = value.strip_prefix("*.") {
        // A single label such as `*.com` would match a whole top-level domain.
        return (is_host_name(domain) && domain.contains('.'))
            .then(|| NetworkRule::DomainSuffix(domain.to_string()));
    }

    if let Ok(address) = value.parse() {
        return Some(NetworkRule::Ip(address));
    }

    if let Some((address, prefix)) = value.split_once('/') {
        return parse_cidr(address, prefix);
    }

    is_host_name(value).then(|| NetworkRule::Domain(value.to_string()))
}

/// Parses the address and prefix length of a CIDR range.
fn parse_cidr(address: &str, prefix: &str) -> Option<NetworkRule> {
    let address: IpAddr = address.parse().ok()?;
    let prefix: u8 = prefix.parse().ok()?;
    let max_prefix = if address.is_ipv4() { 32 } else { 128 };

    (prefix <= max_prefix).then_some(NetworkRule::Cidr { address, prefix })
}

/// Whether the value is a DNS host name: dot-separated labels of letters, digits and hyphens.
fn is_host_name(value: &str) -> bool {
    value.len() <= 253 && value.split('.').all(is_host_label)
}

/// Whether the value is one label of a host name, such as `github` in `github.com`.
fn is_host_label(label: &str) -> bool {
    (1..=63).contains(&label.len())
        && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        && !label.starts_with('-')
        && !label.ends_with('-')
}

/// A host port on the host's loopback that forwards to a port on the sandbox's loopback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PortMapping {
    /// Port fbkd listens on at `localhost` on the host.
    pub host: u16,
    /// Port in the sandbox that connections to the host port reach.
    pub guest: u16,
}

/// A port mapping that isn't `<port>` or `<host>:<guest>` with ports from 1 to 65535.
#[derive(Error, Debug, Clone, PartialEq, Eq)]
#[error(
    "invalid port mapping \"{0}\": use <port> or \"<host>:<guest>\" with ports from 1 to 65535"
)]
pub struct InvalidPortMapping(pub String);

impl FromStr for PortMapping {
    type Err = InvalidPortMapping;

    /// Parses `3000` as host and guest port 3000, and `8080:5173` as host port 8080 and guest
    /// port 5173.
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let invalid = || InvalidPortMapping(value.to_string());

        match value.split_once(':') {
            Some((host, guest)) => Ok(PortMapping {
                host: parse_port(host).ok_or_else(invalid)?,
                guest: parse_port(guest).ok_or_else(invalid)?,
            }),
            None => parse_port(value)
                .map(|port| PortMapping {
                    host: port,
                    guest: port,
                })
                .ok_or_else(invalid),
        }
    }
}

impl fmt::Display for PortMapping {
    /// Writes the mapping as `<host>:<guest>`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.host, self.guest)
    }
}

/// Parses a port from 1 to 65535.
fn parse_port(value: &str) -> Option<u16> {
    value.parse().ok().filter(|port| *port > 0)
}

impl Serialize for PortMapping {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// Deserializes the port mappings, rejecting a host port that is listed more than once.
fn deserialize_ports<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<PortMapping>, D::Error> {
    deserializer.deserialize_seq(PortsVisitor)
}

/// Reads the mappings of a `ports` list.
struct PortsVisitor;

impl<'de> Visitor<'de> for PortsVisitor {
    type Value = Vec<PortMapping>;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a list of ports")
    }

    fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<PortMapping>, A::Error> {
        let mut ports = vec![];
        let mut host_ports = HashSet::new();

        while let Some(mapping) = seq.next_element_seed(UniquePort(&mut host_ports))? {
            ports.push(mapping);
        }

        Ok(ports)
    }
}

/// Reads one mapping and records its host port, failing when an earlier entry has it. The check
/// runs while the parser still points at the entry, so a duplicate is reported at its own line.
struct UniquePort<'a>(&'a mut HashSet<u16>);

impl<'de> de::DeserializeSeed<'de> for UniquePort<'_> {
    type Value = PortMapping;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<PortMapping, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl Visitor<'_> for UniquePort<'_> {
    type Value = PortMapping;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a port such as 3000 or a mapping such as \"8080:5173\"")
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<PortMapping, E> {
        self.visit_str(&value.to_string())
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<PortMapping, E> {
        self.visit_str(&value.to_string())
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<PortMapping, E> {
        let mapping: PortMapping = value.parse().map_err(E::custom)?;

        if !self.0.insert(mapping.host) {
            return Err(E::custom(format!(
                "host port {} is listed more than once",
                mapping.host
            )));
        }

        Ok(mapping)
    }
}

/// Size of the Docker volume when the spec doesn't set one.
fn default_docker_volume() -> String {
    "20 GiB".to_string()
}

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
fn deserialize_size<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
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

/// Deserializes the mounts, rejecting a guest path that is mounted more than once.
fn deserialize_mounts<'de, D: Deserializer<'de>>(
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

/// Loads and parses a sandbox spec from a YAML file.
pub fn from_file(path: &Path) -> Result<SandboxSpec, SandboxSpecError> {
    if !path.exists() {
        return Err(SandboxSpecError::FileNotFound);
    }

    let file_content = fs::read_to_string(path).map_err(SandboxSpecError::CantReadInputFile)?;

    from_str(&file_content)
}

/// Parses a sandbox spec from the YAML text of a spec file.
pub fn from_str(text: &str) -> Result<SandboxSpec, SandboxSpecError> {
    serde_yaml::from_str::<SandboxSpec>(text).map_err(SandboxSpecError::InvalidSpec)
}

/// Returns the ports with `port` in place of the one with the same host port, or added at the
/// end when there is none.
pub fn with_port(ports: &[PortMapping], port: PortMapping) -> Vec<PortMapping> {
    let mut ports = ports.to_vec();

    match ports.iter_mut().find(|p| p.host == port.host) {
        Some(existing) => *existing = port,
        None => ports.push(port),
    }

    ports
}

/// Returns the text of a spec file with `port` in its `ports` list, in place of the entry with
/// the same host port or added at the end. Adds a `ports` key when the spec has none. Only the
/// `ports` block changes, so comments and formatting elsewhere are kept. Fails when the text
/// isn't a valid spec.
pub fn add_port(text: &str, port: PortMapping) -> Result<String, SandboxSpecError> {
    let ports = from_str(text)?.ports;
    let mut lines = Lines::of(text);

    match (
        lines.ports_block(ports.len()),
        ports_index(&ports, port.host),
    ) {
        (None, _) => lines.write_block(&with_port(&ports, port)),
        (Some(block), Some(index)) => lines.set_entry(block.entries[index], port),
        (Some(block), None) => lines.insert_entry(&block, port),
    }

    lines.into_spec()
}

/// Returns the text of a spec file without the entry of the host port in its `ports` list, or
/// `None` when the spec doesn't list the host port. Writes `ports: []` when the last entry is
/// removed. Only the `ports` block changes, so comments and formatting elsewhere are kept.
/// Fails when the text isn't a valid spec.
pub fn remove_port(text: &str, host: u16) -> Result<Option<String>, SandboxSpecError> {
    let ports = from_str(text)?.ports;
    let Some(index) = ports_index(&ports, host) else {
        return Ok(None);
    };
    let remaining: Vec<PortMapping> = ports.iter().filter(|p| p.host != host).copied().collect();
    let mut lines = Lines::of(text);

    match lines.ports_block(ports.len()) {
        Some(block) if !remaining.is_empty() => lines.remove_line(block.entries[index]),
        _ => lines.write_block(&remaining),
    }

    lines.into_spec().map(Some)
}

/// Returns the position of the host port in the ports.
fn ports_index(ports: &[PortMapping], host: u16) -> Option<usize> {
    ports.iter().position(|p| p.host == host)
}

/// Writes a mapping the way Docker Compose does: `3000` when the host and guest port match,
/// and `"8080:5173"` otherwise.
fn compose_entry(port: PortMapping) -> String {
    if port.host == port.guest {
        port.host.to_string()
    } else {
        format!("\"{port}\"")
    }
}

/// The top-level `ports` key of a spec file written as a block list, one entry per line.
struct PortsBlock {
    /// Line of the `ports:` key.
    key: usize,
    /// Line of each entry, in the order of the list.
    entries: Vec<usize>,
}

/// The lines of a spec file, each with its line ending.
struct Lines(Vec<String>);

impl Lines {
    fn of(text: &str) -> Self {
        Self(text.split_inclusive('\n').map(str::to_string).collect())
    }

    /// Finds the `ports` key when it holds a block list with one line per entry, `count` entries
    /// in all.
    fn ports_block(&self, count: usize) -> Option<PortsBlock> {
        let key = self.ports_key()?;
        let after_key = self.0[key]["ports:".len()..].trim();
        let values: Vec<usize> = (key + 1..key + 1 + self.block_len(key))
            .filter(|line| is_value(&self.0[*line]))
            .collect();
        let one_entry_per_line = values.len() == count
            && values
                .iter()
                .all(|line| self.0[*line].trim_start().starts_with('-'));

        let block_list = after_key.is_empty() || after_key.starts_with('#');
        (block_list && one_entry_per_line).then_some(PortsBlock {
            key,
            entries: values,
        })
    }

    /// Returns the line of the top-level `ports` key.
    fn ports_key(&self) -> Option<usize> {
        self.0.iter().position(|line| line.starts_with("ports:"))
    }

    /// Replaces the `ports` key and its entries with a block list of the ports, keeping the
    /// comments between them, or appends one when the spec has no `ports` key.
    fn write_block(&mut self, ports: &[PortMapping]) {
        let block = ports_block_text(ports);

        let Some(key) = self.ports_key() else {
            self.ensure_trailing_newline();
            self.0.push(block);
            return;
        };

        let end = key + 1 + self.block_len(key);
        let comments: Vec<String> = self.0[key + 1..end]
            .iter()
            .filter(|line| !is_value(line))
            .cloned()
            .collect();

        self.0
            .splice(key..end, std::iter::once(block).chain(comments))
            .for_each(drop);
    }

    /// Returns the number of lines after the key that belong to its value.
    fn block_len(&self, key: usize) -> usize {
        self.0[key + 1..]
            .iter()
            .take_while(|line| in_block(line))
            .count()
    }

    /// Replaces the entry on the line with the port, keeping its indentation.
    fn set_entry(&mut self, line: usize, port: PortMapping) {
        self.0[line] = entry_line(indentation(&self.0[line]), port);
    }

    /// Adds the port after the last entry of the block, with the same indentation.
    fn insert_entry(&mut self, block: &PortsBlock, port: PortMapping) {
        let last = block.entries.last().copied().unwrap_or(block.key);
        let indent = block
            .entries
            .last()
            .map_or("  ", |line| indentation(&self.0[*line]))
            .to_string();

        self.ensure_newline_at(last);
        self.0.insert(last + 1, entry_line(&indent, port));
    }

    fn remove_line(&mut self, line: usize) {
        self.0.remove(line);
    }

    fn ensure_trailing_newline(&mut self) {
        if let Some(last) = self.0.len().checked_sub(1) {
            self.ensure_newline_at(last);
        }
    }

    fn ensure_newline_at(&mut self, line: usize) {
        if !self.0[line].ends_with('\n') {
            self.0[line].push('\n');
        }
    }

    /// Joins the lines and checks that they are still a valid spec.
    fn into_spec(self) -> Result<String, SandboxSpecError> {
        let text = self.0.concat();
        from_str(&text)?;

        Ok(text)
    }
}

/// Whether the line belongs to the value of the top-level key above it: it's indented, an
/// entry of a list at the key's own indentation, a comment or blank.
fn in_block(line: &str) -> bool {
    line.starts_with([' ', '\t', '-', '#']) || line.trim().is_empty()
}

/// Whether the line holds a value, rather than only a comment or whitespace.
fn is_value(line: &str) -> bool {
    let trimmed = line.trim_start();

    !trimmed.is_empty() && !trimmed.starts_with('#')
}

/// Returns the leading whitespace of the line.
fn indentation(line: &str) -> &str {
    &line[..line.len() - line.trim_start().len()]
}

fn entry_line(indent: &str, port: PortMapping) -> String {
    format!("{indent}- {}\n", compose_entry(port))
}

/// Returns a `ports` key with a block list of the ports, or `ports: []` without ports.
fn ports_block_text(ports: &[PortMapping]) -> String {
    if ports.is_empty() {
        return "ports: []\n".to_string();
    }

    let entries: String = ports.iter().map(|port| entry_line("  ", *port)).collect();

    format!("ports:\n{entries}")
}

/// Writes the spec to a YAML file, replacing the file's contents. Comments and formatting of an
/// existing file are lost, and fields that aren't set are left out.
pub fn to_file(spec: &SandboxSpec, path: &Path) -> Result<(), SandboxSpecError> {
    let content = serde_yaml::to_string(spec).map_err(SandboxSpecError::CantSerialize)?;

    fs::write(path, content).map_err(SandboxSpecError::CantWriteFile)
}

/// Creates a spec with the given name and default resources. It names no image, so the sandbox
/// runs the [`DEFAULT_IMAGE`] of the installed release, also after an update and `fbk upgrade`.
pub fn default_spec(name: String) -> SandboxSpec {
    SandboxSpec {
        name,
        image: None,
        resources: Some(SandboxResourcesSpec::default()),
        init: Some(true),
        mise: Some(true),
        volumes: VolumesSpec::default(),
        network: None,
        mounts: None,
        ports: vec![],
        clipboard: None,
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::io::Write;
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

        let spec = from_file(file.path()).unwrap();
        let resources = spec.resources.unwrap();

        assert_eq!(spec.name, "dev");
        assert_eq!(resources.cpu, 4);
        assert_eq!(resources.memory, "8Gi");
        assert_eq!(spec.image.as_deref(), Some("ubuntu:24.04"));
    }

    #[test]
    fn image_is_optional() {
        let file = write_spec("name: dev\nresources:\n  cpu: 2\n  memory: 4Gi\n");

        let spec = from_file(file.path()).unwrap();

        assert!(spec.image.is_none());
    }

    #[test]
    fn missing_file_returns_file_not_found() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("does-not-exist.yaml");

        let result = from_file(&path);

        assert!(matches!(result, Err(SandboxSpecError::FileNotFound)));
    }

    #[test]
    fn directory_returns_cant_read_input_file() {
        let dir = TempDir::new().unwrap();

        let result = from_file(dir.path());

        assert!(matches!(
            result,
            Err(SandboxSpecError::CantReadInputFile(_))
        ));
    }

    #[test]
    fn resources_are_optional() {
        let file = write_spec("name: dev\n");

        let spec = from_file(file.path()).unwrap();

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
    fn diagnostic_reports_position_of_syntax_error() {
        let file = write_spec("name: dev\n  bad: indent\n");

        let diagnostic = from_file(file.path()).unwrap_err().diagnostic().unwrap();

        assert_eq!((diagnostic.line, diagnostic.column), (2, 6));
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

        let spec = from_file(file.path()).unwrap();

        assert_eq!(spec.volumes, VolumesSpec::default());
        assert_eq!(spec.volumes.docker, "20 GiB");
    }

    #[test]
    fn docker_volume_is_optional() {
        let file = write_spec("name: dev\nvolumes: {}\n");

        let spec = from_file(file.path()).unwrap();

        assert_eq!(spec.volumes.docker, "20 GiB");
    }

    #[test]
    fn parses_docker_volume() {
        let file = write_spec(
            "name: dev\nresources:\n  cpu: 1\n  memory: 2GiB\nvolumes:\n  docker: 40GiB\n",
        );

        let spec = from_file(file.path()).unwrap();

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
        assert_eq!(spec.mise, Some(true));
        assert_eq!(spec.volumes, VolumesSpec::default());
        assert!(spec.network.is_none());
        assert!(spec.mounts.is_none());
        assert!(spec.ports.is_empty());
        assert_eq!(spec.clipboard, None);
    }

    #[test]
    fn init_is_optional() {
        let file = write_spec("name: dev\n");

        let spec = from_file(file.path()).unwrap();

        assert!(spec.init.is_none());
    }

    #[test]
    fn parses_init() {
        let file = write_spec("name: dev\nimage: alpine:3.22\ninit: false\n");

        let spec = from_file(file.path()).unwrap();

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

        let spec = from_file(file.path()).unwrap();

        assert!(spec.mise.is_none());
    }

    #[test]
    fn parses_mise() {
        let file = write_spec("name: dev\nmise: false\n");

        let spec = from_file(file.path()).unwrap();

        assert_eq!(spec.mise, Some(false));
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

        let spec = from_file(file.path()).unwrap();

        assert!(spec.clipboard.is_none());
    }

    #[test]
    fn parses_clipboard() {
        let file = write_spec("name: dev\nclipboard: true\n");

        let spec = from_file(file.path()).unwrap();

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

        let spec = from_file(file.path()).unwrap();

        assert!(spec.network.is_none());
    }

    #[test]
    fn network_fields_have_defaults() {
        let file = write_spec("name: dev\nnetwork: {}\n");

        let spec = from_file(file.path()).unwrap();

        assert_eq!(spec.network, Some(NetworkSpec::default()));
        assert!(!NetworkSpec::default().enforce);
        assert!(NetworkSpec::default().is_enabled());
    }

    #[test]
    fn parses_network_enabled() {
        for (value, expected) in [("false", false), ("true", true)] {
            let file = write_spec(&format!("name: dev\nnetwork:\n  enabled: {value}\n"));

            let network = from_file(file.path()).unwrap().network.unwrap();

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

        let network = from_file(file.path()).unwrap().network.unwrap();

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

        let spec = from_file(file.path()).unwrap();

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

        let spec = from_file(file.path()).unwrap();

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

            let spec = from_file(file.path()).unwrap();

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

        let mounts = from_file(file.path()).unwrap().mounts.unwrap();

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
    fn missing_file_has_no_diagnostic() {
        let dir = TempDir::new().unwrap();

        let result = from_file(&dir.path().join("does-not-exist.yaml"));

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
        let loaded = from_file(&path).unwrap();

        assert_eq!(loaded.name, "dev");
        assert_eq!(loaded.image, spec.image);
        assert_eq!(loaded.init, Some(true));
        assert_eq!(loaded.mise, Some(true));
        assert_eq!(loaded.volumes, spec.volumes);
        assert_eq!(loaded.network, spec.network);
    }

    #[test]
    fn to_file_leaves_out_unset_fields() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".firebrick.yml");
        let spec = from_file(write_spec("name: dev\n").path()).unwrap();

        to_file(&spec, &path).unwrap();
        let content = fs::read_to_string(&path).unwrap();

        assert!(!content.contains("null"), "{content}");
        assert!(!content.contains("network"), "{content}");
    }

    #[test]
    fn to_file_reports_write_failure() {
        let dir = TempDir::new().unwrap();

        let result = to_file(&default_spec("dev".to_string()), dir.path());

        assert!(matches!(result, Err(SandboxSpecError::CantWriteFile(_))));
    }
}
