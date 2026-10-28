//! The `ports` of a spec: `host:guest` mappings, each host port at most once.

use std::collections::HashSet;
use std::fmt;
use std::str::FromStr;

use serde::de::{self, Visitor};
use serde::{Deserializer, Serialize};
use std::result::Result;
use thiserror::Error;

/// A host port on the host's loopback that forwards to a port on the sandbox's loopback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PortMapping {
    /// Port fbkd listens on at `localhost` on the host.
    pub host: u16,
    /// Port in the sandbox that connections to the host port reach.
    pub guest: u16,
}

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
pub(super) fn deserialize_ports<'de, D: Deserializer<'de>>(
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

/// A port mapping that isn't `<port>` or `<host>:<guest>` with ports from 1 to 65535.
#[derive(Error, Debug, Clone, PartialEq, Eq)]
#[error(
    "invalid port mapping \"{0}\": use <port> or \"<host>:<guest>\" with ports from 1 to 65535"
)]
pub struct InvalidPortMapping(pub String);
