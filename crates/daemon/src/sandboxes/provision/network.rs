//! Network settings of sandboxes: removes the network device of a sandbox whose network is
//! disabled, and otherwise turns the egress rules of a spec into microsandbox's network policy,
//! with TLS interception and an HTTP deny response that tells the user how to allow a blocked
//! host.

use firebrick_spec::{NetworkRule, NetworkSpec, ProxyEndpoint, ProxySpec};
use microsandbox::MicrosandboxError;
use microsandbox::sandbox::{SandboxBuilder, SandboxSpec};
use microsandbox_network::config::HttpDenyResponseFormat;
use microsandbox_network::policy::{
    Action, NetworkPolicy, Rule, RuleBuilder, RuleDestinationBuilder,
};
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;
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
    let rules = policy_label(network);
    match &network.proxy {
        Some(proxy) => format!("{rules};proxy={proxy}"),
        None => rules,
    }
}

/// Records the effective policy separately from the proxy, including when policy is off.
fn policy_label(network: &NetworkSpec) -> String {
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
    let (value, proxy) = match value.split_once(";proxy=") {
        Some((rules, proxy)) => (rules, Some(proxy.parse().ok()?)),
        None => (value, None),
    };
    let mut network = policy_from_label(value)?;
    network.proxy = proxy;
    Some(network)
}

/// Reads the policy portion, accepting labels written before outbound proxies existed.
fn policy_from_label(value: &str) -> Option<NetworkSpec> {
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
pub async fn add_to_builder(
    builder: SandboxBuilder,
    network: &NetworkSpec,
) -> Result<SandboxBuilder, MicrosandboxError> {
    if !network.is_enabled() {
        return Ok(builder.disable_network());
    }

    let builder = with_proxy(builder, network.proxy.as_ref()).await?;

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

/// Adds the host-side transport without replacing policy or TLS settings.
async fn with_proxy(
    builder: SandboxBuilder,
    proxy: Option<&ProxySpec>,
) -> Result<SandboxBuilder, MicrosandboxError> {
    let Some(proxy) = proxy else {
        return Ok(builder);
    };
    let resolved = resolve_proxy_with(proxy, async {
        tokio::net::lookup_host(proxy.endpoint().to_string())
            .await
            .map(Iterator::collect)
    })
    .await?;
    Ok(match resolved {
        ProxySpec::HttpConnect(address) => builder.proxy(|p| p.http_connect(address.to_string())),
        ProxySpec::Socks4(address) => builder.proxy(|p| p.socks4(address.to_string())),
        ProxySpec::Socks5(address) => builder.proxy(|p| p.socks5(address.to_string())),
    })
}

/// Resolves the proxy on the host, bounded in time, and never falls back to a direct transport.
async fn resolve_proxy_with(
    proxy: &ProxySpec,
    lookup: impl std::future::Future<Output = std::io::Result<Vec<SocketAddr>>>,
) -> Result<ProxySpec, MicrosandboxError> {
    if matches!(proxy.endpoint(), ProxyEndpoint::Ip(_)) {
        return Ok(proxy.clone());
    }
    let error = |reason| {
        MicrosandboxError::InvalidConfig(format!(
            "cannot resolve network.proxy endpoint {} on the host: {reason}",
            proxy.endpoint()
        ))
    };
    let addresses = tokio::time::timeout(Duration::from_secs(5), lookup)
        .await
        .map_err(|_| error("DNS lookup timed out".to_string()))?
        .map_err(|err| error(err.to_string()))?;
    let address = select_proxy_address(proxy, addresses)
        .ok_or_else(|| error("no usable address (SOCKS4 requires IPv4)".to_string()))?;
    Ok(proxy.with_address(address))
}

/// Prefer IPv4 when both families are returned; SOCKS4 requires it.
fn select_proxy_address(proxy: &ProxySpec, addresses: Vec<SocketAddr>) -> Option<SocketAddr> {
    addresses
        .into_iter()
        .filter(|address| !proxy.requires_ipv4() || address.is_ipv4())
        .min_by_key(|address| address.is_ipv6())
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

    #[test]
    fn proxy_labels_round_trip_with_and_without_enforcement() {
        for enforce in [false, true] {
            let network = NetworkSpec {
                enforce,
                proxy: Some("http://127.0.0.1:3128".parse().unwrap()),
                ..NetworkSpec::default()
            };
            assert_eq!(rules_from_label(&rules_label(&network)), Some(network));
        }
        assert!(rules_from_label("off;proxy=http://user:password@127.0.0.1:3128").is_none());
    }

    #[tokio::test]
    async fn proxy_hostname_resolution_prefers_ipv4_and_preserves_protocol() {
        let proxy: ProxySpec = "socks5://proxy.internal:1080".parse().unwrap();
        let resolved = resolve_proxy_with(
            &proxy,
            std::future::ready(Ok(vec![
                "[::1]:1080".parse().unwrap(),
                "127.0.0.1:1080".parse().unwrap(),
            ])),
        )
        .await
        .unwrap();
        assert_eq!(resolved.to_string(), "socks5://127.0.0.1:1080");
        assert_eq!(proxy.to_string(), "socks5://proxy.internal:1080");
    }

    #[tokio::test]
    async fn proxy_dns_failure_returns_an_error_instead_of_a_direct_transport() {
        let proxy: ProxySpec = "http://proxy.invalid:3128".parse().unwrap();
        let error = resolve_proxy_with(
            &proxy,
            std::future::ready(Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "DNS unavailable",
            ))),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("proxy.invalid:3128"));
        assert!(error.to_string().contains("DNS unavailable"));
    }

    #[tokio::test]
    async fn socks4_proxy_hostname_requires_an_ipv4_answer() {
        let proxy: ProxySpec = "socks4://proxy.internal:1080".parse().unwrap();
        let error = resolve_proxy_with(
            &proxy,
            std::future::ready(Ok(vec!["[::1]:1080".parse().unwrap()])),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("SOCKS4 requires IPv4"));
    }

    #[tokio::test]
    async fn literal_proxy_address_does_not_need_dns() {
        let proxy: ProxySpec = "http://127.0.0.1:3128".parse().unwrap();
        let resolved = resolve_proxy_with(&proxy, std::future::pending())
            .await
            .unwrap();
        assert_eq!(resolved, proxy);
    }

    #[tokio::test]
    async fn proxy_dns_timeout_returns_an_error() {
        let proxy: ProxySpec = "http://proxy.internal:3128".parse().unwrap();
        let error = resolve_proxy_with(&proxy, std::future::pending())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("DNS lookup timed out"));
    }

    #[tokio::test]
    async fn proxy_hostname_can_resolve_to_ipv6_only_for_http_connect() {
        let proxy: ProxySpec = "http://proxy.internal:3128".parse().unwrap();
        let lookup = std::future::ready(Ok(vec!["[::1]:3128".parse().unwrap()]));
        let resolved = resolve_proxy_with(&proxy, lookup).await.unwrap();
        assert_eq!(resolved.to_string(), "http://[::1]:3128");
    }

    #[tokio::test]
    async fn localhost_proxy_is_resolved_before_building_the_sandbox_config() {
        let network = NetworkSpec {
            proxy: Some("http://localhost:3128".parse().unwrap()),
            ..NetworkSpec::default()
        };
        let config = config_with(&network, &[]).await;
        let json = serde_json::to_value(&config.spec.network).unwrap();
        let address: SocketAddr = json["outbound_proxy"]["address"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        assert!(address.ip().is_loopback());
        assert_eq!(address.port(), 3128);
        assert_eq!(rules_from_label(&rules_label(&network)), Some(network));
    }

    #[test]
    fn disabled_network_remembers_its_proxy_for_image_upgrades() {
        let network = NetworkSpec {
            enabled: Some(false),
            proxy: Some("http://127.0.0.1:3128".parse().unwrap()),
            ..NetworkSpec::default()
        };
        let restored = rules_from_label(&rules_label(&network)).unwrap();
        assert!(!restored.is_enabled());
        assert_eq!(restored.proxy, network.proxy);
    }

    #[tokio::test]
    async fn proxy_is_passed_to_microsandbox_independently_of_enforcement() {
        assert_proxy_config(false).await;
        assert_proxy_config(true).await;
    }

    async fn assert_proxy_config(enforce: bool) {
        for (url, protocol) in [
            ("http://127.0.0.1:3128", "http_connect"),
            ("socks4://127.0.0.1:1080", "socks4"),
            ("socks5://127.0.0.1:1080", "socks5"),
        ] {
            let network = NetworkSpec {
                enforce,
                proxy: Some(url.parse().unwrap()),
                ..NetworkSpec::default()
            };
            let config = config_with(&network, &[]).await;
            let json = serde_json::to_value(&config.spec.network).unwrap();
            assert_eq!(json["outbound_proxy"]["protocol"], protocol);
            assert_eq!(
                json["outbound_proxy"]["address"],
                url.split_once("://").unwrap().1
            );
        }
    }

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
        let builder = add_to_builder(builder, network).await.unwrap();

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
