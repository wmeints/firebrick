//! Network settings of sandboxes: removes the network device of a sandbox whose network is
//! disabled, and otherwise turns the egress rules of a spec into microsandbox's network policy,
//! with TLS interception and an HTTP deny response that tells the user how to allow a blocked
//! host.

use firebrick_spec::{NetworkRule, NetworkSpec};
use microsandbox::MicrosandboxError;
use microsandbox::sandbox::{SandboxBuilder, SandboxSpec};
use microsandbox_network::config::HttpDenyResponseFormat;
use microsandbox_network::policy::{
    Action, NetworkPolicy, Rule, RuleBuilder, RuleDestinationBuilder,
};
use std::net::IpAddr;
use url::Host;

/// Body of the `403 Forbidden` response to a denied HTTP or HTTPS request. microsandbox
/// replaces `{host}` with the denied host.
pub const DENY_MESSAGE: &str = "firebrick blocked the connection to {host}: the network policy \
     of this sandbox doesn't allow it. To allow it, run `fbk network allow {host}` on the host, \
     outside the sandbox.";

/// Label that records the network settings a sandbox was created with, so an update that
/// doesn't change them can leave the sandbox alone.
pub const RULES_LABEL: &str = "firebrick.network";

/// Returns the value of [`RULES_LABEL`] for the network settings. A disabled network ignores
/// its rules, so it is recorded as `disabled`. Rules that aren't enforced don't change the
/// sandbox, so they are all recorded as `off`.
pub fn rules_label(network: &NetworkSpec) -> String {
    if !network.is_enabled() {
        return "disabled".to_string();
    }

    if !network.enforce {
        return "off".to_string();
    }

    let join = |rules: &[NetworkRule]| {
        rules
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",")
    };

    format!(
        "allow={};deny={}",
        join(&network.allow),
        join(&network.deny)
    )
}

/// Returns the egress rules of a sandbox from its stored config. A sandbox whose label can't be
/// parsed, or one created before [`RULES_LABEL`] existed that has a network policy, counts as
/// enforced without allow rules, so callers that check a host against the rules fail closed.
pub fn rules_of(spec: &SandboxSpec) -> NetworkSpec {
    let unknown = || NetworkSpec {
        enforce: true,
        ..NetworkSpec::default()
    };

    match spec.labels.get(RULES_LABEL) {
        Some(label) => rules_from_label(label).unwrap_or_else(unknown),
        None if spec.network.policy.is_some() => unknown(),
        None => NetworkSpec::default(),
    }
}

/// Parses a value of [`RULES_LABEL`] back into rules. A disabled network comes back disabled
/// and enforced without rules, so it allows no host. Returns `None` for a value that
/// [`rules_label`] can't have written.
pub fn rules_from_label(value: &str) -> Option<NetworkSpec> {
    match value {
        "off" => return Some(NetworkSpec::default()),
        "disabled" => {
            return Some(NetworkSpec {
                enabled: Some(false),
                enforce: true,
                ..NetworkSpec::default()
            });
        }
        _ => {}
    }

    let (allow, deny) = value.strip_prefix("allow=")?.split_once(";deny=")?;

    Some(NetworkSpec {
        enforce: true,
        allow: parse_rules(allow)?,
        deny: parse_rules(deny)?,
        ..NetworkSpec::default()
    })
}

/// Parses a comma-separated list of rules, which may be empty.
fn parse_rules(value: &str) -> Option<Vec<NetworkRule>> {
    value
        .split(',')
        .filter(|rule| !rule.is_empty())
        .map(|rule| rule.parse().ok())
        .collect()
}

/// Whether the rules let the sandbox connect to the host, matched like [`policy`]: a matching
/// deny rule wins, then a matching allow rule, and anything else is denied. Rules that aren't
/// enforced allow every host. Domain rules only match host names and IP and CIDR rules only IP
/// addresses, because fbkd doesn't resolve names.
pub fn allows_host(network: &NetworkSpec, host: &Host<&str>) -> bool {
    let matches = |rule: &NetworkRule| rule_matches(rule, host);

    !network.enforce || (!network.deny.iter().any(matches) && network.allow.iter().any(matches))
}

/// Whether the rule's destination is the host.
fn rule_matches(rule: &NetworkRule, host: &Host<&str>) -> bool {
    match (rule, host) {
        (NetworkRule::Domain(domain), Host::Domain(name)) => {
            name.trim_end_matches('.').eq_ignore_ascii_case(domain)
        }
        (NetworkRule::DomainSuffix(suffix), Host::Domain(name)) => {
            is_same_or_subdomain(name.trim_end_matches('.'), suffix)
        }
        (NetworkRule::Ip(address), Host::Ipv4(ip)) => *address == IpAddr::V4(*ip),
        (NetworkRule::Ip(address), Host::Ipv6(ip)) => *address == IpAddr::V6(*ip),
        (NetworkRule::Cidr { address, prefix }, Host::Ipv4(ip)) => {
            in_cidr(*address, *prefix, IpAddr::V4(*ip))
        }
        (NetworkRule::Cidr { address, prefix }, Host::Ipv6(ip)) => {
            in_cidr(*address, *prefix, IpAddr::V6(*ip))
        }
        _ => false,
    }
}

/// Whether the name is the domain or one of its subdomains.
fn is_same_or_subdomain(name: &str, domain: &str) -> bool {
    let name = name.to_ascii_lowercase();
    let domain = domain.to_ascii_lowercase();

    name == domain || name.ends_with(&format!(".{domain}"))
}

/// Whether the IP address is in the network `address/prefix` of the same family.
fn in_cidr(address: IpAddr, prefix: u8, ip: IpAddr) -> bool {
    match (address, ip) {
        (IpAddr::V4(network), IpAddr::V4(ip)) => {
            same_prefix(network.to_bits().into(), ip.to_bits().into(), prefix, 32)
        }
        (IpAddr::V6(network), IpAddr::V6(ip)) => {
            same_prefix(network.to_bits(), ip.to_bits(), prefix, 128)
        }
        _ => false,
    }
}

/// Whether the first `prefix` of the `width` bits of both addresses are equal.
fn same_prefix(network: u128, ip: u128, prefix: u8, width: u32) -> bool {
    let shift = width.saturating_sub(u32::from(prefix));

    network.checked_shr(shift).unwrap_or(0) == ip.checked_shr(shift).unwrap_or(0)
}

/// Adds the network settings to a sandbox that is being created. A disabled network removes
/// the sandbox's network device and ignores the rules. Without `enforce`, the sandbox keeps
/// microsandbox's default policy and no TLS interception.
pub fn add_to_builder(
    builder: SandboxBuilder,
    network: &NetworkSpec,
) -> Result<SandboxBuilder, MicrosandboxError> {
    if !network.is_enabled() {
        return Ok(builder.disable_network());
    }

    if !network.enforce {
        return Ok(builder);
    }

    let policy = policy(network)?;

    // `tls(|t| t)` intercepts TLS on port 443, so HTTPS requests can get the deny response too.
    // Only the text format renders `deny_message`; the default JSON format ignores it.
    Ok(builder.network(|n| {
        n.policy(policy).tls(|t| t).http(|h| {
            h.deny_response(true)
                .deny_response_format(HttpDenyResponseFormat::Text)
                .deny_message(DENY_MESSAGE)
        })
    }))
}

/// Returns the policy that enforces the rules: all deny rules, then all allow rules, then DNS
/// to the sandbox's resolver, and deny for any other egress. Rules match first-come, so a deny
/// rule wins over an allow rule regardless of where either is written. microsandbox also
/// matches domain rules against DNS queries, so a host denied by a domain rule doesn't
/// resolve, while every other name does.
pub fn policy(network: &NetworkSpec) -> Result<NetworkPolicy, MicrosandboxError> {
    let mut policy = NetworkPolicy::builder()
        .default_egress(Action::Deny)
        .egress(|r| {
            for rule in &network.deny {
                add_rule(r.deny(), rule);
            }
            for rule in &network.allow {
                add_rule(r.allow(), rule);
            }
            r
        })
        .build()?;

    policy.rules.push(Rule::allow_dns());

    Ok(policy)
}

/// Commits a rule with the destination of the spec's rule.
fn add_rule<'a>(
    destination: RuleDestinationBuilder<'a>,
    rule: &NetworkRule,
) -> &'a mut RuleBuilder {
    match rule {
        NetworkRule::Domain(domain) => destination.domain(domain),
        NetworkRule::DomainSuffix(domain) => destination.domain_suffix(domain),
        NetworkRule::Ip(address) => destination.ip(address.to_string()),
        NetworkRule::Cidr { .. } => destination.cidr(rule.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::Secret;
    use microsandbox::sandbox::{Sandbox, SandboxConfig};
    use microsandbox_network::policy::{Destination, Direction};

    fn rules(values: &[&str]) -> Vec<NetworkRule> {
        values.iter().map(|value| value.parse().unwrap()).collect()
    }

    fn enforced_without_rules() -> NetworkSpec {
        enforced(&[], &[])
    }

    fn enforced(allow: &[&str], deny: &[&str]) -> NetworkSpec {
        NetworkSpec {
            enforce: true,
            allow: rules(allow),
            deny: rules(deny),
            ..NetworkSpec::default()
        }
    }

    fn disabled(network: NetworkSpec) -> NetworkSpec {
        NetworkSpec {
            enabled: Some(false),
            ..network
        }
    }

    /// Returns the config microsandbox would create a sandbox with after `add_to_builder` and the
    /// secrets.
    async fn config_with(network: &NetworkSpec, secrets: &[Secret]) -> SandboxConfig {
        let builder = Sandbox::builder("fbk-unit-network").image("alpine");
        let builder = add_to_builder(builder, network).unwrap();

        crate::secrets::add_to_builder(builder, secrets)
            .build()
            .await
            .unwrap()
    }

    /// Describes an egress rule as `<action> <destination>`.
    fn describe(rule: &Rule) -> String {
        assert_eq!(rule.direction, Direction::Egress);

        let action = match rule.action {
            Action::Allow => "allow",
            Action::Deny => "deny",
        };
        let destination = match &rule.destination {
            Destination::Domain(domain) => format!("domain {domain}"),
            Destination::DomainSuffix(domain) => format!("suffix {domain}"),
            Destination::Cidr(network) => format!("cidr {network}"),
            other => format!("{other:?}"),
        };

        format!("{action} {destination}")
    }

    fn allows(network: &NetworkSpec, host: &str) -> bool {
        let url = url::Url::parse(&format!("http://{host}/")).unwrap();
        allows_host(network, &url.host().unwrap())
    }

    #[test]
    fn rules_from_label_reads_what_rules_label_writes() {
        let enforced = enforced(&["github.com", "*.npmjs.org", "10.0.0.0/8"], &["1.2.3.4"]);
        let no_rules = enforced_without_rules();

        for network in [enforced, no_rules, NetworkSpec::default()] {
            assert_eq!(rules_from_label(&rules_label(&network)), Some(network));
        }
    }

    #[test]
    fn rules_from_label_rejects_other_values() {
        assert_eq!(rules_from_label(""), None);
        assert_eq!(rules_from_label("allow=github.com"), None);
        assert_eq!(rules_from_label("allow=not a rule;deny="), None);
    }

    #[test]
    fn disabled_network_allows_no_host() {
        let network = rules_from_label(&rules_label(&disabled(NetworkSpec::default()))).unwrap();

        assert!(!network.is_enabled());
        assert!(!allows(&network, "example.com"));
    }

    #[test]
    fn allows_every_host_when_not_enforced() {
        assert!(allows(&NetworkSpec::default(), "anything.example"));
        assert!(allows(&NetworkSpec::default(), "10.1.2.3"));
    }

    #[test]
    fn allows_hosts_that_match_an_allow_rule_only() {
        let network = enforced(&["github.com", "*.npmjs.org"], &[]);

        assert!(allows(&network, "github.com"));
        assert!(allows(&network, "GitHub.com."));
        assert!(allows(&network, "npmjs.org"));
        assert!(allows(&network, "registry.npmjs.org"));
        assert!(!allows(&network, "api.github.com"));
        assert!(!allows(&network, "evilnpmjs.org"));
        assert!(!allows(&network, "attacker.example"));
    }

    #[test]
    fn deny_rules_win_over_allow_rules() {
        let network = enforced(&["*.github.com"], &["gist.github.com"]);

        assert!(allows(&network, "api.github.com"));
        assert!(!allows(&network, "gist.github.com"));
    }

    #[test]
    fn ip_rules_match_ip_hosts_only() {
        let network = enforced(&["140.82.112.0/20", "2001:db8::1"], &["140.82.112.4"]);

        assert!(allows(&network, "140.82.113.1"));
        assert!(!allows(&network, "140.82.112.4"));
        assert!(!allows(&network, "140.82.128.1"));
        assert!(allows(&network, "[2001:db8::1]"));
        assert!(!allows(&network, "[2001:db8::2]"));
    }

    #[test]
    fn rules_label_lists_enforced_rules() {
        assert_eq!(
            rules_label(&enforced(&["github.com", "*.npmjs.org"], &["10.0.0.0/8"])),
            "allow=github.com,*.npmjs.org;deny=10.0.0.0/8"
        );
        assert_eq!(rules_label(&enforced(&[], &[])), "allow=;deny=");
    }

    #[test]
    fn rules_label_ignores_rules_that_are_not_enforced() {
        let off = NetworkSpec {
            enforce: false,
            ..enforced(&["github.com"], &[])
        };

        assert_eq!(rules_label(&off), "off");
        assert_eq!(rules_label(&NetworkSpec::default()), "off");
    }

    #[test]
    fn rules_label_records_a_disabled_network_without_rules() {
        assert_eq!(
            rules_label(&disabled(enforced(&["github.com"], &[]))),
            "disabled"
        );
        assert_eq!(rules_label(&disabled(NetworkSpec::default())), "disabled");
    }

    #[test]
    fn rules_label_of_an_explicitly_enabled_network_matches_the_default() {
        let enabled = |network: NetworkSpec| NetworkSpec {
            enabled: Some(true),
            ..network
        };

        assert_eq!(rules_label(&enabled(NetworkSpec::default())), "off");
        assert_eq!(
            rules_label(&enabled(enforced(&["github.com"], &[]))),
            "allow=github.com;deny="
        );
    }

    #[tokio::test]
    async fn disabled_network_removes_the_device_and_ignores_the_rules() {
        let config = config_with(&disabled(enforced(&["github.com"], &[])), &[]).await;

        assert!(!config.spec.network.enabled);
    }

    #[tokio::test]
    async fn secrets_do_not_enable_a_disabled_network() {
        let secret = Secret::new("GH_TOKEN".to_string(), "abc".to_string(), vec![]).unwrap();

        let config = config_with(&disabled(NetworkSpec::default()), &[secret]).await;

        assert!(!config.spec.network.enabled);
    }

    #[tokio::test]
    async fn enabled_network_keeps_the_device() {
        let config = config_with(&enforced(&["github.com"], &[]), &[]).await;

        assert!(config.spec.network.enabled);
    }

    #[tokio::test]
    async fn enforced_network_answers_denied_requests_with_the_deny_message() {
        let config = config_with(&enforced(&["github.com"], &[]), &[]).await;
        let http = &config.spec.network.http;

        assert!(http.deny_response);
        assert_eq!(http.deny_response_format, HttpDenyResponseFormat::Text);
        assert_eq!(http.deny_message.as_deref(), Some(DENY_MESSAGE));
    }

    #[test]
    fn policy_denies_egress_by_default() {
        let policy = policy(&enforced(&[], &[])).unwrap();

        assert_eq!(policy.default_egress, Action::Deny);
    }

    #[test]
    fn policy_allows_dns_last() {
        let policy = policy(&enforced(&["github.com"], &["gist.github.com"])).unwrap();

        assert_eq!(policy.rules.len(), 3);
        assert_eq!(
            format!("{:?}", policy.rules[2]),
            format!("{:?}", Rule::allow_dns())
        );
    }

    #[test]
    fn policy_puts_deny_rules_before_allow_rules() {
        let policy = policy(&enforced(
            &["*.github.com", "10.0.0.0/8"],
            &["gist.github.com", "10.1.2.3"],
        ))
        .unwrap();

        let (_dns, rules) = policy.rules.split_last().unwrap();
        let described: Vec<String> = rules.iter().map(describe).collect();

        assert_eq!(
            described,
            [
                "deny domain gist.github.com",
                "deny cidr 10.1.2.3/32",
                "allow suffix github.com",
                "allow cidr 10.0.0.0/8",
            ]
        );
    }

    #[test]
    fn policy_maps_each_rule_form_to_its_destination() {
        let policy = policy(&enforced(
            &[
                "github.com",
                "*.githubusercontent.com",
                "140.82.112.4",
                "2001:db8::1",
                "192.168.10.0/24",
            ],
            &[],
        ))
        .unwrap();

        let (_dns, rules) = policy.rules.split_last().unwrap();
        let described: Vec<String> = rules.iter().map(describe).collect();

        assert_eq!(
            described,
            [
                "allow domain github.com",
                "allow suffix githubusercontent.com",
                "allow cidr 140.82.112.4/32",
                "allow cidr 2001:db8::1/128",
                "allow cidr 192.168.10.0/24",
            ]
        );
    }

    #[test]
    fn deny_message_names_the_host_and_the_command() {
        assert_eq!(
            DENY_MESSAGE.replace("{host}", "example.org"),
            "firebrick blocked the connection to example.org: the network policy of this \
             sandbox doesn't allow it. To allow it, run `fbk network allow example.org` on the \
             host, outside the sandbox."
        );
    }
}
