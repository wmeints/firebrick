//! The `env` of a spec: plain environment variables whose names are valid and not reserved
//! for microsandbox, with string, number or boolean values.

use std::collections::BTreeMap;
use std::fmt;

use serde::Deserializer;
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use std::result::Result;
use thiserror::Error;

/// Prefix of the environment variables that microsandbox reserves for secret placeholders.
pub const RESERVED_ENV_PREFIX: &str = "MSB_";

/// An environment variable name that a sandbox can't get.
#[derive(Error, Debug, PartialEq, Eq)]
pub enum InvalidEnvName {
    /// The name doesn't match `[A-Za-z_][A-Za-z0-9_]*`.
    #[error(
        "invalid variable name {0:?}: use ASCII letters, digits and '_', not starting with a digit"
    )]
    Invalid(String),
    /// The name has the prefix microsandbox reserves for secret placeholders.
    #[error("{0}: the MSB_ prefix is reserved")]
    Reserved(String),
}

/// Checks an environment variable name: an ASCII letter or `_` first, then only ASCII letters,
/// digits and `_`, and not the [`RESERVED_ENV_PREFIX`]. A valid name is safe to show in a
/// message.
pub fn validate_env_name(name: &str) -> Result<(), InvalidEnvName> {
    let starts_ok = name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_');
    let charset_ok = name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');

    if !(starts_ok && charset_ok) {
        return Err(InvalidEnvName::Invalid(name.to_string()));
    }

    if name.starts_with(RESERVED_ENV_PREFIX) {
        return Err(InvalidEnvName::Reserved(name.to_string()));
    }

    Ok(())
}

/// Deserializes the `env` map, checking each name and turning each scalar value into its text.
pub(super) fn deserialize_env<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<String, String>, D::Error> {
    deserializer.deserialize_option(EnvVisitor)
}

/// Reads the variables of an `env` map. An empty `env:` has no variables.
struct EnvVisitor;

impl<'de> Visitor<'de> for EnvVisitor {
    type Value = BTreeMap<String, String>;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a map of environment variables")
    }

    fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(BTreeMap::new())
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_map(self)
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut env = BTreeMap::new();

        while let Some(name) = map.next_key_seed(EnvName(&env))? {
            let value = map.next_value_seed(EnvValue)?;
            env.insert(name, value);
        }

        Ok(env)
    }
}

/// Reads a variable name and checks it, failing when the map already has it. The check runs
/// while the parser still points at the name, so a problem is reported at its own line.
struct EnvName<'a>(&'a BTreeMap<String, String>);

impl<'de> de::DeserializeSeed<'de> for EnvName<'_> {
    type Value = String;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<String, D::Error> {
        deserializer.deserialize_str(self)
    }
}

impl Visitor<'_> for EnvName<'_> {
    type Value = String;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("an environment variable name")
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<String, E> {
        // An invalid name may hold escape sequences, which the diagnostic strips.
        validate_env_name(value).map_err(E::custom)?;

        if self.0.contains_key(value) {
            return Err(E::custom(format!("{value} is listed more than once")));
        }

        Ok(value.to_string())
    }
}

/// Reads a variable value: a string as it is, and a number or boolean as its text.
struct EnvValue;

/// What a variable value has to be.
const ENV_VALUE_EXPECTED: &str = "expected a string, number or boolean";

impl<'de> de::DeserializeSeed<'de> for EnvValue {
    type Value = String;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<String, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for EnvValue {
    type Value = String;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a string, number or boolean")
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<String, E> {
        Ok(value.to_string())
    }

    fn visit_bool<E: de::Error>(self, value: bool) -> Result<String, E> {
        Ok(value.to_string())
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<String, E> {
        Ok(value.to_string())
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<String, E> {
        Ok(value.to_string())
    }

    fn visit_f64<E: de::Error>(self, value: f64) -> Result<String, E> {
        Ok(value.to_string())
    }

    fn visit_unit<E: de::Error>(self) -> Result<String, E> {
        Err(E::custom(ENV_VALUE_EXPECTED))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, _seq: A) -> Result<String, A::Error> {
        Err(de::Error::custom(ENV_VALUE_EXPECTED))
    }

    fn visit_map<A: MapAccess<'de>>(self, _map: A) -> Result<String, A::Error> {
        Err(de::Error::custom(ENV_VALUE_EXPECTED))
    }
}
