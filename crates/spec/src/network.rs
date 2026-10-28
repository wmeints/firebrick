//! The `network` section of a spec and its `allow` and `deny` rules: host names, `*.domain`
//! suffixes, IP addresses and CIDR ranges.

use std::fmt;
use std::net::IpAddr;
use std::str::FromStr;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use std::result::Result;
use thiserror::Error;

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
