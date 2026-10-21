//! Sets the plain environment variables of a sandbox's spec, and keeps them in sync with the
//! spec when the sandbox starts. A label records which variables came from the spec, so a sync
//! only changes those and leaves the image's variables alone.

use microsandbox::MicrosandboxError;
use microsandbox::sandbox::{SandboxBuilder, SandboxHandle, SandboxSpec};
use std::collections::BTreeMap;

/// Label that stores the names of the variables a sandbox got from its spec, comma-separated.
pub const NAMES_LABEL: &str = "firebrick.env";

/// Returns the builder with the variables set and their names in [`NAMES_LABEL`].
pub fn with_env(builder: SandboxBuilder, env: &BTreeMap<String, String>) -> SandboxBuilder {
    builder.envs(env).label(NAMES_LABEL, label_value(env))
}

/// Returns the variables the sandbox got from its spec: the names in [`NAMES_LABEL`] with the
/// values the sandbox has for them. A sandbox from before the label has none.
pub fn stored(spec: &SandboxSpec) -> BTreeMap<String, String> {
    let Some(names) = spec.labels.get(NAMES_LABEL) else {
        return BTreeMap::new();
    };

    names
        .split(',')
        .filter_map(|name| {
            spec.env
                .iter()
                .find(|var| var.key == name)
                .map(|var| (var.key.clone(), var.value.clone()))
        })
        .collect()
}

/// Gives the sandbox the variables in place of the ones it got from its spec before, from its
/// next start on: sets the new and changed ones and removes the ones that are gone. Does
/// nothing when the sandbox already has them.
pub async fn sync(
    sb: &SandboxHandle,
    env: &BTreeMap<String, String>,
) -> Result<(), MicrosandboxError> {
    let stored = stored(&sb.config()?.spec);

    if stored == *env {
        return Ok(());
    }

    let modify = env
        .iter()
        .filter(|(name, value)| stored.get(*name) != Some(*value))
        .fold(sb.modify(), |modify, (name, value)| modify.env(name, value));
    let modify = stored
        .keys()
        .filter(|name| !env.contains_key(*name))
        .fold(modify, |modify, name| modify.remove_env(name));

    modify
        .label(NAMES_LABEL, label_value(env))
        .next_start()
        .apply()
        .await?;

    Ok(())
}

/// Whether the variables differ from the ones the sandbox got from its spec.
pub fn differs(sb: &SandboxHandle, env: &BTreeMap<String, String>) -> bool {
    sb.config().is_ok_and(|config| stored(&config.spec) != *env)
}

/// Returns the value of [`NAMES_LABEL`] for the variables.
fn label_value(env: &BTreeMap<String, String>) -> String {
    env.keys().map(String::as_str).collect::<Vec<_>>().join(",")
}

#[cfg(test)]
mod tests {
    use super::*;
    use microsandbox::sandbox::{EnvVar, Sandbox};

    fn vars(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect()
    }

    fn spec_with(labels: &[(&str, &str)], env: &[(&str, &str)]) -> SandboxSpec {
        SandboxSpec {
            labels: vars(labels),
            env: env
                .iter()
                .map(|(name, value)| EnvVar::new(*name, *value))
                .collect(),
            ..SandboxSpec::default()
        }
    }

    #[test]
    fn stored_returns_only_the_variables_from_the_spec() {
        let spec = spec_with(
            &[(NAMES_LABEL, "NODE_ENV,PORT")],
            &[("PATH", "/usr/bin"), ("NODE_ENV", "dev"), ("PORT", "3000")],
        );

        assert_eq!(
            stored(&spec),
            vars(&[("NODE_ENV", "dev"), ("PORT", "3000")])
        );
    }

    #[test]
    fn stored_is_empty_without_the_label() {
        let spec = spec_with(&[], &[("PATH", "/usr/bin")]);

        assert!(stored(&spec).is_empty());
    }

    #[test]
    fn stored_is_empty_for_an_empty_label() {
        let spec = spec_with(&[(NAMES_LABEL, "")], &[("PATH", "/usr/bin")]);

        assert!(stored(&spec).is_empty());
    }

    #[tokio::test]
    async fn with_env_records_the_names_in_the_label() {
        let builder = with_env(
            Sandbox::builder("env-test").image("ubuntu:26.04"),
            &vars(&[("B", "2"), ("A", "")]),
        );

        let spec = builder.build().await.unwrap().spec;

        assert_eq!(stored(&spec), vars(&[("A", ""), ("B", "2")]));
        assert_eq!(spec.labels[NAMES_LABEL], "A,B");
    }
}
