//! Lets the JVMs in a sandbox with TLS interception trust microsandbox's interception CA. A JDK
//! only reads its own `lib/security/cacerts`, which doesn't hold the CA, so fbkd builds one
//! PKCS12 trust store from the guest's system bundle, which does, and points every JVM at it
//! with `JAVA_TOOL_OPTIONS`. The store doesn't belong to a JDK, so it also covers JDKs installed
//! after the sandbox started.

use super::{ExitError, check_exit};
use crate::secrets::Secret;
use firebrick_spec::NetworkSpec;
use microsandbox::sandbox::SandboxSpec;
use microsandbox::{MicrosandboxError, Sandbox};
use std::collections::BTreeMap;
use std::time::Duration;
use thiserror::Error;

/// The variable every JVM reads extra options from.
pub const TOOL_OPTIONS_VAR: &str = "JAVA_TOOL_OPTIONS";

/// Guest path of the trust store.
pub const STORE_PATH: &str = "/etc/ssl/certs/java/firebrick.p12";

/// Guest path of the system bundle, which holds the interception CA.
const SYSTEM_BUNDLE_PATH: &str = "/etc/ssl/certs/ca-certificates.crt";

/// The options that point a JVM at the trust store. The store holds only public CA
/// certificates, so its password protects nothing secret.
pub const TRUST_STORE_OPTIONS: &str = "-Djavax.net.ssl.trustStore=/etc/ssl/certs/java/firebrick.p12 \
     -Djavax.net.ssl.trustStorePassword=changeit";

/// How long building the trust store gets in the sandbox.
const BUILD_TIMEOUT: Duration = Duration::from_secs(30);

/// Builds the trust store at `$2` from the bundle at `$1`, as a separate file that replaces the
/// old store at once. `openssl pkcs12` writes mode `0600`, which only root could read. Names
/// the reason when the guest lacks openssl, the bundle or `-jdktrust` (OpenSSL 3.2 or newer).
const BUILD_SCRIPT: &str = r#"set -e
bundle=$1
store=$2
command -v openssl >/dev/null 2>&1 || { echo "openssl is not installed" >&2; exit 1; }
[ -f "$bundle" ] || { echo "$bundle doesn't exist" >&2; exit 1; }
openssl pkcs12 -help 2>&1 | grep -q -- -jdktrust || {
  echo "$(openssl version) can't write Java trust stores; it needs OpenSSL 3.2 or newer" >&2
  exit 1
}
install -d -m 755 "${store%/*}"
openssl pkcs12 -export -nokeys -jdktrust anyExtendedKeyUsage -in "$bundle" \
  -out "$store.tmp" -passout pass:changeit
chmod 644 "$store.tmp"
mv -f "$store.tmp" "$store"
"#;

/// The property a JVM reads the path of its trust store from.
const TRUST_STORE_PROPERTY: &str = "-Djavax.net.ssl.trustStore=";

/// Errors of building the trust store in a sandbox.
#[derive(Error, Debug)]
pub enum BuildError {
    /// Running the build in the guest failed, for example because the image has no shell.
    #[error(transparent)]
    Exec(#[from] MicrosandboxError),
    /// The build exited non-zero, with the reason on stderr.
    #[error("the build {0}")]
    Exited(#[from] ExitError),
}

/// Whether a sandbox created with the network settings and secrets gets TLS interception: an
/// enforced network policy or at least one secret turns it on.
pub fn will_intercept(network: &NetworkSpec, secrets: &[Secret]) -> bool {
    (network.is_enabled() && network.enforce) || !secrets.is_empty()
}

/// Whether the sandbox with the spec has TLS interception.
pub fn intercepts(spec: &SandboxSpec) -> bool {
    spec.network.tls.as_ref().is_some_and(|tls| tls.enabled)
}

/// Returns the variables with [`TRUST_STORE_OPTIONS`] at the end of `JAVA_TOOL_OPTIONS` when
/// `intercepts`, after the options the variables already have, and without them otherwise. A
/// value that already sets its own trust store is left alone, so the user's store wins. The
/// options are removed first, so this can be applied again to the variables it returned.
pub fn with_tool_options(
    mut env: BTreeMap<String, String>,
    intercepts: bool,
) -> BTreeMap<String, String> {
    let own = env
        .remove(TOOL_OPTIONS_VAR)
        .and_then(|value| without_trust_store(&value));
    let value = if intercepts {
        Some(with_trust_store(own))
    } else {
        own
    };

    if let Some(value) = value {
        env.insert(TOOL_OPTIONS_VAR.to_string(), value);
    }

    env
}

/// Returns the user's part of the value: the value without [`TRUST_STORE_OPTIONS`] at its end,
/// or `None` when it holds nothing else.
fn without_trust_store(value: &str) -> Option<String> {
    if value == TRUST_STORE_OPTIONS {
        return None;
    }

    let own = value
        .strip_suffix(TRUST_STORE_OPTIONS)
        .and_then(|own| own.strip_suffix(' '))
        .unwrap_or(value);

    Some(own.to_string())
}

/// Returns the user's options followed by [`TRUST_STORE_OPTIONS`], or the user's options alone
/// when they set a trust store of their own.
fn with_trust_store(own: Option<String>) -> String {
    match own {
        Some(own) if own.contains(TRUST_STORE_PROPERTY) => own,
        Some(own) if !own.trim().is_empty() => format!("{} {TRUST_STORE_OPTIONS}", own.trim()),
        _ => TRUST_STORE_OPTIONS.to_string(),
    }
}

/// (Re)builds the trust store as root from the guest's system bundle when the running sandbox
/// has TLS interception, so the store follows changes to the bundle. A failed build logs a
/// warning and returns the reason, so the caller can show it: the sandbox is still usable
/// without the store, but Java doesn't trust the interception CA.
pub async fn apply(sb: &Sandbox) -> Result<(), BuildError> {
    if !intercepts(&sb.config().spec) {
        return Ok(());
    }

    let result = build(sb).await;

    if let Err(err) = &result {
        tracing::warn!(
            "failed to build the Java trust store {STORE_PATH} in sandbox {}, so Java doesn't \
             trust the TLS interception CA: {err}",
            sb.name()
        );
    }

    result
}

/// Runs the build of the trust store as root.
async fn build(sb: &Sandbox) -> Result<(), BuildError> {
    let output = sb
        .exec_with("sh", |exec| {
            exec.args(build_args())
                .user("root")
                .cwd("/")
                .timeout(BUILD_TIMEOUT)
        })
        .await?;

    Ok(check_exit(&output)?)
}

/// Returns the arguments of `sh` that build the trust store.
fn build_args() -> [&'static str; 5] {
    [
        "-c",
        BUILD_SCRIPT,
        "build-java-trust-store",
        SYSTEM_BUNDLE_PATH,
        STORE_PATH,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandboxes::provision::network;
    use firebrick_spec::NetworkRule;

    fn vars(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect()
    }

    fn secret() -> Secret {
        Secret::new("GH_TOKEN".to_string(), "token".to_string(), vec![]).unwrap()
    }

    fn enforced() -> NetworkSpec {
        NetworkSpec {
            enforce: true,
            allow: vec![NetworkRule::Domain("example.com".to_string())],
            ..NetworkSpec::default()
        }
    }

    #[test]
    fn trust_store_options_point_at_the_store() {
        assert_eq!(
            TRUST_STORE_OPTIONS,
            "-Djavax.net.ssl.trustStore=/etc/ssl/certs/java/firebrick.p12 \
             -Djavax.net.ssl.trustStorePassword=changeit"
        );
        assert!(TRUST_STORE_OPTIONS.contains(STORE_PATH));
    }

    #[test]
    fn with_tool_options_sets_the_options_when_intercepting() {
        let env = with_tool_options(vars(&[("PORT", "3000")]), true);

        assert_eq!(
            env,
            vars(&[("PORT", "3000"), (TOOL_OPTIONS_VAR, TRUST_STORE_OPTIONS)])
        );
    }

    #[test]
    fn with_tool_options_appends_the_options_to_the_users_value() {
        let env = with_tool_options(vars(&[(TOOL_OPTIONS_VAR, "-Xmx1g")]), true);

        assert_eq!(
            env[TOOL_OPTIONS_VAR],
            format!("-Xmx1g {TRUST_STORE_OPTIONS}")
        );
    }

    #[test]
    fn with_tool_options_appends_the_options_once() {
        let once = with_tool_options(vars(&[(TOOL_OPTIONS_VAR, "-Xmx1g")]), true);

        assert_eq!(with_tool_options(once.clone(), true), once);
    }

    #[test]
    fn with_tool_options_is_stable_for_a_value_with_surrounding_whitespace() {
        let once = with_tool_options(vars(&[(TOOL_OPTIONS_VAR, " -Xmx1g ")]), true);

        assert_eq!(
            once[TOOL_OPTIONS_VAR],
            format!("-Xmx1g {TRUST_STORE_OPTIONS}")
        );
        assert_eq!(with_tool_options(once.clone(), true), once);
    }

    #[test]
    fn with_tool_options_keeps_a_trust_store_of_the_users_own() {
        let own = vars(&[(TOOL_OPTIONS_VAR, "-Djavax.net.ssl.trustStore=/opt/my.p12")]);

        assert_eq!(with_tool_options(own.clone(), true), own);
    }

    #[test]
    fn with_tool_options_keeps_an_empty_value_without_interception() {
        let empty = vars(&[(TOOL_OPTIONS_VAR, "")]);

        assert_eq!(with_tool_options(empty.clone(), false), empty);
        assert_eq!(
            with_tool_options(empty, true),
            vars(&[(TOOL_OPTIONS_VAR, TRUST_STORE_OPTIONS)])
        );
    }

    #[test]
    fn with_tool_options_removes_the_options_without_interception() {
        let added = with_tool_options(vars(&[(TOOL_OPTIONS_VAR, "-Xmx1g")]), true);

        assert_eq!(
            with_tool_options(added, false),
            vars(&[(TOOL_OPTIONS_VAR, "-Xmx1g")])
        );
        assert_eq!(
            with_tool_options(with_tool_options(vars(&[]), true), false),
            vars(&[])
        );
    }

    #[test]
    fn with_tool_options_keeps_the_users_value_without_interception() {
        let env = vars(&[(TOOL_OPTIONS_VAR, "-Xmx1g"), ("PORT", "3000")]);

        assert_eq!(with_tool_options(env.clone(), false), env);
        assert_eq!(with_tool_options(vars(&[]), false), vars(&[]));
    }

    #[tokio::test]
    async fn will_intercept_matches_the_tls_setting_microsandbox_creates() {
        let disabled = NetworkSpec {
            enabled: Some(false),
            ..enforced()
        };
        let cases = [
            (NetworkSpec::default(), vec![]),
            (NetworkSpec::default(), vec![secret()]),
            (enforced(), vec![]),
            (enforced(), vec![secret()]),
            (disabled.clone(), vec![]),
            (disabled, vec![secret()]),
        ];

        let wanted: Vec<_> = cases
            .iter()
            .map(|(network, secrets)| will_intercept(network, secrets))
            .collect();

        assert_eq!(wanted, created_intercepts(&cases).await);
        assert_eq!(wanted, [false, true, true, true, false, true]);
    }

    /// Returns whether microsandbox creates each sandbox with the network and secrets with TLS
    /// interception.
    async fn created_intercepts(cases: &[(NetworkSpec, Vec<Secret>)]) -> Vec<bool> {
        let mut created = vec![];
        for (network, secrets) in cases {
            created.push(intercepts(&built_spec(network, secrets).await));
        }
        created
    }

    /// Returns the spec microsandbox would create a sandbox with the network and secrets with.
    async fn built_spec(network: &NetworkSpec, secrets: &[Secret]) -> SandboxSpec {
        let builder = Sandbox::builder("java-test").image("ubuntu:26.04");
        let builder = network::add_to_builder(builder, network).await.unwrap();

        crate::secrets::add_to_builder(builder, secrets)
            .build()
            .await
            .unwrap()
            .spec
    }

    #[test]
    fn build_args_build_the_store_from_the_system_bundle() {
        let args = build_args();

        assert_eq!(&args[3..], [SYSTEM_BUNDLE_PATH, STORE_PATH]);
    }

    #[test]
    fn a_failed_build_names_the_reason() {
        let err = BuildError::from(ExitError {
            code: 1,
            stderr: "openssl is not installed".to_string(),
        });

        assert_eq!(
            err.to_string(),
            "the build exited with code 1: openssl is not installed"
        );
    }
}
