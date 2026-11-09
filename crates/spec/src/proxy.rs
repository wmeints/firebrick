//! Validated, credential-free addresses of host-side outbound proxies.

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::net::SocketAddr;
use std::str::FromStr;
use thiserror::Error;

/// An outbound proxy reached by the host, rather than by the guest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProxySpec {
    /// TCP tunnels opened with HTTP CONNECT.
    HttpConnect(ProxyEndpoint),
    /// SOCKS4 TCP proxy, without a user ID.
    Socks4(ProxyEndpoint),
    /// SOCKS5 proxy, without authentication.
    Socks5(ProxyEndpoint),
}

/// A numeric endpoint or a DNS name resolved by fbkd when it creates the sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProxyEndpoint {
    /// An IPv4 address, or bracketed IPv6 address, with its port.
    Ip(SocketAddr),
    /// An ASCII DNS name and an explicit, nonzero port.
    Hostname { host: String, port: u16 },
}

impl FromStr for ProxyEndpoint {
    type Err = InvalidProxy;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if let Ok(address) = value.parse::<SocketAddr>() {
            return (address.port() != 0)
                .then_some(Self::Ip(address))
                .ok_or(InvalidProxy);
        }
        let (host, port) = value.rsplit_once(':').ok_or(InvalidProxy)?;
        let name = host.strip_suffix('.').unwrap_or(host);
        if !crate::network::is_host_name(name)
            || name.chars().all(|c| c.is_ascii_digit() || c == '.')
            || port.is_empty()
            || !port.chars().all(|c| c.is_ascii_digit())
        {
            return Err(InvalidProxy);
        }
        let port: u16 = port.parse().map_err(|_| InvalidProxy)?;
        if port == 0 {
            return Err(InvalidProxy);
        }
        Ok(Self::Hostname {
            host: host.to_ascii_lowercase(),
            port,
        })
    }
}

impl fmt::Display for ProxyEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ip(address) => write!(f, "{address}"),
            Self::Hostname { host, port } => write!(f, "{host}:{port}"),
        }
    }
}

impl ProxySpec {
    /// The endpoint, before host-side name resolution.
    pub fn endpoint(&self) -> &ProxyEndpoint {
        match self {
            Self::HttpConnect(endpoint) | Self::Socks4(endpoint) | Self::Socks5(endpoint) => {
                endpoint
            }
        }
    }

    /// Whether this proxy transport requires an IPv4 endpoint.
    pub fn requires_ipv4(&self) -> bool {
        matches!(self, Self::Socks4(_))
    }

    /// Keeps the protocol and replaces its endpoint with a resolved socket address.
    pub fn with_address(&self, address: SocketAddr) -> Self {
        let endpoint = ProxyEndpoint::Ip(address);
        match self {
            Self::HttpConnect(_) => Self::HttpConnect(endpoint),
            Self::Socks4(_) => Self::Socks4(endpoint),
            Self::Socks5(_) => Self::Socks5(endpoint),
        }
    }
}

/// Invalid proxy input. Deliberately excludes the input, which might contain credentials.
#[derive(Debug, Clone, Copy, Error)]
#[error(
    "invalid network.proxy: use http://host:port, socks4://host:port or socks5://host:port; host must be an ASCII DNS name or IP address (bracket IPv6); use a nonzero port and no credentials, path, query or fragment; SOCKS4 requires IPv4"
)]
pub struct InvalidProxy;

impl FromStr for ProxySpec {
    type Err = InvalidProxy;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (scheme, address) = value.split_once("://").ok_or(InvalidProxy)?;
        let address: ProxyEndpoint = address.parse()?;
        match scheme {
            "http" => Ok(Self::HttpConnect(address)),
            "socks4" if !matches!(&address, ProxyEndpoint::Ip(ip) if ip.is_ipv6()) => {
                Ok(Self::Socks4(address))
            }
            "socks5" => Ok(Self::Socks5(address)),
            _ => Err(InvalidProxy),
        }
    }
}

impl fmt::Display for ProxySpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HttpConnect(address) => write!(f, "http://{address}"),
            Self::Socks4(address) => write!(f, "socks4://{address}"),
            Self::Socks5(address) => write!(f, "socks5://{address}"),
        }
    }
}

impl Serialize for ProxySpec {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ProxySpec {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_str(ProxyVisitor)
    }
}

/// Parses at the scalar's position so YAML diagnostics identify the proxy setting.
struct ProxyVisitor;

impl Visitor<'_> for ProxyVisitor {
    type Value = ProxySpec;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a proxy URL with a DNS name or IP address and an explicit port")
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        value.parse().map_err(E::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NetworkSpec;

    #[test]
    fn proxy_urls_round_trip_through_the_network_section() {
        for url in [
            "http://127.0.0.1:3128",
            "http://[::1]:3128",
            "socks4://192.168.1.10:1080",
            "socks5://[::1]:1080",
            "http://proxy.example:3128",
            "socks4://proxy.internal:1080",
            "socks5://localhost:1080",
            "http://proxy.example.:3128",
        ] {
            let spec: NetworkSpec = serde_yaml::from_str(&format!("proxy: {url}\n")).unwrap();
            assert_eq!(spec.proxy.as_ref().unwrap().to_string(), url);
            let restored: NetworkSpec =
                serde_yaml::from_str(&serde_yaml::to_string(&spec).unwrap()).unwrap();
            assert_eq!(restored, spec);
        }
    }

    #[test]
    fn rejects_unsupported_or_ambiguous_proxy_urls_without_echoing_them() {
        for url in [
            "",
            "127.0.0.1:3128",
            "https://127.0.0.1:3128",
            "http://proxy.example",
            "http://proxy.example:0",
            "http://-proxy.example:3128",
            "http://proxy..example:3128",
            "http://proxy.example:3128#fragment",
            "http://999.999.999.999:3128",
            "http://user:secret@proxy.example:3128",
            "http://127.0.0.1:0",
            "http://127.0.0.1",
            "http://127.0.0.1:3128/",
            "http://user:secret@127.0.0.1:3128",
            "http://127.0.0.1:3128?secret",
            "socks4://[::1]:1080",
            "http://127.0.0.1:70000",
        ] {
            assert!(url.parse::<ProxySpec>().is_err(), "{url}");
        }
        let error = "http://user:secret@127.0.0.1:3128"
            .parse::<ProxySpec>()
            .unwrap_err()
            .to_string();
        assert!(!error.contains("secret"));
    }

    #[test]
    fn hostname_case_is_normalized_without_resolving_it() {
        let proxy: ProxySpec = "http://Proxy.Example:3128".parse().unwrap();
        assert_eq!(proxy.to_string(), "http://proxy.example:3128");
        assert!(
            matches!(proxy.endpoint(), ProxyEndpoint::Hostname { host, port }
            if host == "proxy.example" && *port == 3128)
        );
    }

    #[test]
    fn missing_proxy_keeps_the_existing_spec_format() {
        let spec: NetworkSpec = serde_yaml::from_str("enforce: true\n").unwrap();
        assert!(spec.proxy.is_none());
        assert!(!serde_yaml::to_string(&spec).unwrap().contains("proxy"));
    }
}
