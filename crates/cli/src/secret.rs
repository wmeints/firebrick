//! Secrets that sandboxes use without seeing their values.

use anyhow::{Context, Result, anyhow, bail};
use clap::ValueEnum;
use serde::Serialize;
use std::io::Read;
use std::path::Path;
use tonic::transport::Channel;

use crate::api::sandbox_management_service_client::SandboxManagementServiceClient;
use crate::api::{ListSecretsRequest, RemoveSecretRequest, SecretSummary, SetSecretRequest};
use crate::manage::{self, OutputFormat};
use crate::table;

/// Which sandboxes a secret belongs to.
#[derive(ValueEnum, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Scope {
    /// Every sandbox, except the ones with a sandbox-scoped secret with the same name.
    #[default]
    Global,
    /// The sandbox for the working directory only. It overrides a global secret with the same
    /// name.
    Sandbox,
}

/// Name `fbk secret ls` shows for the global scope.
const GLOBAL_SCOPE: &str = "global";

/// A secret to set. It has no `Debug`, so its value can't end up in logs.
pub struct NewSecret {
    /// Environment variable that exposes the secret in sandboxes.
    pub name: String,
    /// Value of the secret.
    pub value: String,
    /// Hosts that may receive the value. Empty uses the defaults for well-known names.
    pub allowed_hosts: Vec<String>,
    /// Which sandboxes get the secret.
    pub scope: Scope,
}

/// Stores a secret in the daemon, which adds it to the sandboxes in its scope.
pub async fn set(
    secret: NewSecret,
    working_dir: &Path,
    client: &mut SandboxManagementServiceClient<Channel>,
) -> Result<()> {
    let name = secret.name;
    let sandbox = scoped_sandbox(secret.scope, working_dir, client).await?;
    let response = client
        .set_secret(SetSecretRequest {
            name: name.clone(),
            value: secret.value,
            allowed_hosts: secret.allowed_hosts,
            sandbox: sandbox.clone(),
        })
        .await
        .map_err(|status| anyhow!("{}", status.message()))?
        .into_inner();

    match sandbox {
        Some(sandbox) => println!(
            "Secret {name} set for sandbox {sandbox}. The sandbox sees the placeholder $MSB_{name}; it gets it after a restart when it's running."
        ),
        None => println!(
            "Secret {name} set. Sandboxes see the placeholder $MSB_{name}; running sandboxes get it after a restart."
        ),
    }

    for sandbox in response.failed_sandboxes {
        eprintln!(
            "warning: couldn't add secret {name} to sandbox {sandbox}, see the fbkd log for details"
        );
    }

    Ok(())
}

/// Prints the secrets' names and allowed hosts in the given format. Values never leave the
/// daemon.
pub async fn list(
    format: OutputFormat,
    client: &mut SandboxManagementServiceClient<Channel>,
) -> Result<()> {
    let secrets = client
        .list_secrets(ListSecretsRequest {})
        .await
        .map_err(|status| anyhow!("{}", status.message()))?
        .into_inner()
        .secrets;

    let output = match format {
        OutputFormat::Table => render_table(&secrets)?,
        OutputFormat::Json => render_json(&secrets)?,
    };

    print!("{output}");

    Ok(())
}

/// Removes a secret from the sandboxes in its scope and from the daemon. When a sandbox
/// fails, the daemon keeps the secret so the removal can be retried.
pub async fn remove(
    name: String,
    scope: Scope,
    working_dir: &Path,
    client: &mut SandboxManagementServiceClient<Channel>,
) -> Result<()> {
    let sandbox = scoped_sandbox(scope, working_dir, client).await?;
    let response = client
        .remove_secret(RemoveSecretRequest {
            name: name.clone(),
            sandbox: sandbox.clone(),
        })
        .await
        .map_err(|status| anyhow!("{}", status.message()))?
        .into_inner();

    ensure_removed(&name, scope, &response.failed_sandboxes)?;

    match sandbox {
        Some(sandbox) => println!(
            "Secret {name} removed from sandbox {sandbox}. A running sandbox keeps using it until it restarts."
        ),
        None => {
            println!("Secret {name} removed. Running sandboxes keep using it until they restart.")
        }
    }

    Ok(())
}

/// Fails with an error that explains how to retry when the secret couldn't be removed from
/// some sandboxes.
fn ensure_removed(name: &str, scope: Scope, failed_sandboxes: &[String]) -> Result<()> {
    if failed_sandboxes.is_empty() {
        return Ok(());
    }

    let retry = match scope {
        Scope::Global => format!("fbk secret rm {name}"),
        Scope::Sandbox => format!("fbk secret rm {name} --scope sandbox"),
    };
    bail!(
        "couldn't remove secret {name} from sandboxes {}, see the fbkd log for details. \
         The secret is kept, so run `{retry}` again to retry",
        failed_sandboxes.join(", ")
    );
}

/// Returns the sandbox for the working directory with the sandbox scope, or `None` with the
/// global scope. Fails when the sandbox belongs to another directory.
async fn scoped_sandbox(
    scope: Scope,
    working_dir: &Path,
    client: &mut SandboxManagementServiceClient<Channel>,
) -> Result<Option<String>> {
    match scope {
        Scope::Global => Ok(None),
        Scope::Sandbox => Ok(Some(
            manage::resolve_owned_spec(working_dir, client).await?.name,
        )),
    }
}

/// Returns the scope `fbk secret ls` shows: the sandbox's name, or `global`.
fn scope_of(secret: &SecretSummary) -> &str {
    secret.sandbox.as_deref().unwrap_or(GLOBAL_SCOPE)
}

/// Renders the secrets as a table with their name, scope and allowed hosts.
fn render_table(secrets: &[SecretSummary]) -> Result<String> {
    let rows: Vec<[String; 3]> = secrets
        .iter()
        .map(|secret| {
            [
                secret.name.clone(),
                scope_of(secret).to_string(),
                secret.allowed_hosts.join(", "),
            ]
        })
        .collect();

    table::render(["NAME", "SCOPE", "ALLOWED HOSTS"], &rows)
}

/// A secret as printed in the JSON output.
#[derive(Serialize)]
struct SecretListing<'a> {
    name: &'a str,
    scope: &'a str,
    allowed_hosts: &'a [String],
}

/// Renders the secrets as a pretty-printed JSON array.
fn render_json(secrets: &[SecretSummary]) -> Result<String> {
    let listings: Vec<SecretListing> = secrets
        .iter()
        .map(|secret| SecretListing {
            name: &secret.name,
            scope: scope_of(secret),
            allowed_hosts: &secret.allowed_hosts,
        })
        .collect();

    Ok(format!("{}\n", serde_json::to_string_pretty(&listings)?))
}

/// Reads a secret value from `reader`, without the line ending that `echo` and most password
/// managers add.
pub fn read_value(mut reader: impl Read) -> Result<String> {
    let mut value = String::new();
    reader
        .read_to_string(&mut value)
        .context("failed to read the secret value from stdin")?;

    let trimmed = value
        .strip_suffix("\r\n")
        .or_else(|| value.strip_suffix('\n'))
        .unwrap_or(&value);

    Ok(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake_daemon::{self, OTHER_WORKSPACE};
    use std::fs;
    use tempfile::TempDir;

    fn summary(name: &str, hosts: &[&str]) -> SecretSummary {
        SecretSummary {
            name: name.to_string(),
            allowed_hosts: hosts.iter().map(|host| host.to_string()).collect(),
            sandbox: None,
        }
    }

    fn scoped_summary(name: &str, sandbox: &str, hosts: &[&str]) -> SecretSummary {
        SecretSummary {
            sandbox: Some(sandbox.to_string()),
            ..summary(name, hosts)
        }
    }

    fn sandbox_secret() -> NewSecret {
        NewSecret {
            name: "TOKEN".to_string(),
            value: "value".to_string(),
            allowed_hosts: Vec::new(),
            scope: Scope::Sandbox,
        }
    }

    #[tokio::test]
    async fn sandbox_scope_refuses_sandbox_of_other_workspace() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join(manage::SPEC_FILE_NAME), "name: dev\n").unwrap();
        let mut client = fake_daemon::serve("dev", OTHER_WORKSPACE).await;

        let set_result = set(sandbox_secret(), dir.path(), &mut client).await;
        let remove_result =
            remove("TOKEN".to_string(), Scope::Sandbox, dir.path(), &mut client).await;

        fake_daemon::assert_dev_belongs_to_other(set_result);
        fake_daemon::assert_dev_belongs_to_other(remove_result);
    }

    #[tokio::test]
    async fn sandbox_scope_accepts_sandbox_that_does_not_exist_yet() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join(manage::SPEC_FILE_NAME), "name: dev\n").unwrap();
        let mut client = fake_daemon::serve("other", OTHER_WORKSPACE).await;

        let error = set(sandbox_secret(), dir.path(), &mut client)
            .await
            .unwrap_err();

        assert_eq!(error.to_string(), fake_daemon::refusal("set_secret", "dev"));
    }

    #[test]
    fn table_lists_names_scopes_and_hosts() {
        let secrets = [
            summary("GH_TOKEN", &["github.com", "api.github.com"]),
            scoped_summary("MY_SECRET", "dev", &["example.com"]),
        ];

        let table = render_table(&secrets).unwrap();

        assert_eq!(
            table,
            "┌───────────┬────────┬────────────────────────────┐\n\
             │ NAME      │ SCOPE  │ ALLOWED HOSTS              │\n\
             ├───────────┼────────┼────────────────────────────┤\n\
             │ GH_TOKEN  │ global │ github.com, api.github.com │\n\
             │ MY_SECRET │ dev    │ example.com                │\n\
             └───────────┴────────┴────────────────────────────┘\n"
        );
    }

    #[test]
    fn empty_table_shows_header() {
        let table = render_table(&[]).unwrap();

        assert!(table.contains("NAME"), "{table}");
        assert!(table.contains("SCOPE"), "{table}");
        assert!(table.contains("ALLOWED HOSTS"), "{table}");
    }

    #[test]
    fn json_lists_names_scopes_and_hosts() {
        let secrets = [
            summary("A", &["example.com"]),
            scoped_summary("A", "dev", &["example.org"]),
        ];

        let json: serde_json::Value =
            serde_json::from_str(&render_json(&secrets).unwrap()).unwrap();

        assert_eq!(
            json,
            serde_json::json!([
                { "name": "A", "scope": "global", "allowed_hosts": ["example.com"] },
                { "name": "A", "scope": "dev", "allowed_hosts": ["example.org"] },
            ])
        );
    }

    #[test]
    fn empty_json_is_empty_array() {
        assert_eq!(render_json(&[]).unwrap(), "[]\n");
    }

    #[test]
    fn read_value_removes_one_line_ending() {
        assert_eq!(read_value("token\n".as_bytes()).unwrap(), "token");
        assert_eq!(read_value("token\r\n".as_bytes()).unwrap(), "token");
        assert_eq!(read_value("token\n\n".as_bytes()).unwrap(), "token\n");
    }

    #[test]
    fn read_value_keeps_other_whitespace() {
        assert_eq!(read_value(" to ken ".as_bytes()).unwrap(), " to ken ");
        assert_eq!(read_value("".as_bytes()).unwrap(), "");
    }

    #[test]
    fn read_value_rejects_invalid_utf8() {
        assert!(read_value(&[0xff, 0xfe][..]).is_err());
    }
}
