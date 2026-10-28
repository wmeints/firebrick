//! Setting and removing the secrets of sandboxes: stores them and adds them to, or removes
//! them from, the sandboxes they're scoped to, without letting a secret and a plain
//! environment variable share a name.

use super::error::SandboxError;
use super::status::{get_sandbox, list_all_sandboxes};
use super::{NewSecret, SandboxManager, StartSandbox, sets_variable};
use crate::secrets::{self, Secret};
use crate::ssh;
use microsandbox::sandbox::SandboxHandle;
use std::collections::HashSet;
use std::fmt;

impl SandboxManager {
    /// Fails with `InvalidArgument` when a requested environment variable has the name of a
    /// secret the sandbox gets, which would hide the secret's placeholder.
    pub(super) fn check_env_against_secrets(
        &self,
        request: StartSandbox<'_>,
    ) -> Result<(), SandboxError> {
        let Some(env) = request.env.filter(|env| !env.is_empty()) else {
            return Ok(());
        };
        let secrets = self.load_secrets_for(request.name)?;

        match secrets::conflicting_name(&secrets, env.keys()) {
            Some(name) => Err(SandboxError::invalid_argument(format!(
                "env variable {name} conflicts with secret {name}; remove one of them"
            ))),
            None => Ok(()),
        }
    }
    /// Removes the sandbox-scoped secrets of a removed sandbox from the store, so a later
    /// sandbox with the same name doesn't inherit them.
    pub(super) async fn remove_sandbox_secrets(&self, name: &str) -> Result<(), SandboxError> {
        let _secrets_guard = self.secrets_lock.lock().await;

        self.secrets.remove_sandbox(name).map_err(|err| {
            tracing::error!(error = ?err, "failed to remove the secrets of sandbox {name}");
            SandboxError::internal(format!(
                "removed sandbox {name}, but failed to remove its secrets"
            ))
        })?;

        Ok(())
    }
    /// Stores a secret and adds it to the sandboxes in its scope: the sandbox it belongs to, or,
    /// for a global secret, every existing sandbox without a sandbox-scoped secret with that
    /// name. Running sandboxes pick it up the next time they start. Returns the names of the
    /// sandboxes it couldn't be added to.
    pub async fn set_secret(&self, request: NewSecret) -> Result<Vec<String>, SandboxError> {
        let secret = Secret::new(request.name, request.value, request.allowed_hosts)
            .map_err(|err| SandboxError::invalid_argument(err.to_string()))?
            .in_scope(request.sandbox);

        let _secrets_guard = self.secrets_lock.lock().await;
        let scoped_handle = scoped_sandbox(secret.sandbox()).await?;
        self.check_secret_against_env(scoped_handle.as_ref(), secret.name())
            .await?;

        self.secrets.set(secret.clone()).map_err(|err| {
            tracing::error!(error = ?err, "failed to store secret {}", secret.name());
            SandboxError::internal("failed to store secret")
        })?;

        let change = SecretChange::Add(&secret);
        let failed_sandboxes = self
            .update_scope(scoped_handle.as_ref(), secret.name(), change)
            .await
            .map_err(|_| {
                SandboxError::internal(
                    "stored the secret, but failed to list the sandboxes to add it to",
                )
            })?;

        tracing::info!(
            "set secret {}",
            describe_secret(secret.name(), secret.sandbox())
        );

        Ok(failed_sandboxes)
    }
    /// Returns the stored secrets, sorted by name, then by scope with the global secret first.
    pub fn list_secrets(&self) -> Result<Vec<Secret>, SandboxError> {
        let mut secrets = self.load_secrets()?;
        secrets.sort_by(|a, b| (a.name(), a.sandbox()).cmp(&(b.name(), b.sandbox())));
        Ok(secrets)
    }
    /// Removes a stored secret and removes it from the sandboxes in its scope, like
    /// [`SandboxManager::set_secret`]. A sandbox that loses its sandbox-scoped secret gets the
    /// global secret with the same name back, if there is one. Running sandboxes keep the old
    /// secret until they restart. Returns the names of the sandboxes the change failed for, in
    /// which case the secret stays in the store so the removal can be retried.
    pub async fn remove_secret(
        &self,
        name: &str,
        sandbox: Option<&str>,
    ) -> Result<Vec<String>, SandboxError> {
        secrets::validate_name(name)
            .map_err(|err| SandboxError::invalid_argument(err.to_string()))?;

        let _secrets_guard = self.secrets_lock.lock().await;
        let scoped_handle = scoped_sandbox(sandbox).await?;
        let stored = self.load_secrets()?;
        ensure_secret_exists(&stored, name, sandbox)?;

        // Remove the secret from the store last, so it can be removed again when a sandbox
        // fails.
        let change = removal(&stored, name, sandbox);
        let failed_sandboxes = self
            .update_scope(scoped_handle.as_ref(), name, change)
            .await?;

        if !failed_sandboxes.is_empty() {
            return Ok(failed_sandboxes);
        }

        self.secrets.remove(name, sandbox).map_err(|err| {
            tracing::error!(error = ?err, "failed to remove secret {name}");
            SandboxError::internal("failed to remove secret")
        })?;

        tracing::info!("removed secret {}", describe_secret(name, sandbox));

        Ok(failed_sandboxes)
    }
    /// Applies a change to the secret with the name to the sandbox it belongs to, or, without
    /// one, to the sandboxes that get the global secret. Returns the names of the sandboxes the
    /// change failed for.
    async fn update_scope(
        &self,
        scoped_handle: Option<&SandboxHandle>,
        name: &str,
        change: SecretChange<'_>,
    ) -> Result<Vec<String>, SandboxError> {
        match scoped_handle {
            Some(handle) => Ok(apply_to_sandbox(handle, &change).await),
            None => self.update_unscoped_sandboxes(name, change).await,
        }
    }
    /// Fails with `FailedPrecondition` when a sandbox the secret goes to has a variable from its
    /// spec with the secret's name, which would hide the secret's placeholder: the scoped
    /// sandbox, or every firebrick sandbox without its own secret with the name.
    async fn check_secret_against_env(
        &self,
        scoped_handle: Option<&SandboxHandle>,
        name: &str,
    ) -> Result<(), SandboxError> {
        let conflicting = match scoped_handle {
            Some(handle) => sets_variable(handle, name).then(|| handle.name().to_string()),
            None => self
                .unscoped_sandboxes(name)
                .await?
                .iter()
                .find(|handle| sets_variable(handle, name))
                .map(|handle| handle.name().to_string()),
        };

        match conflicting {
            Some(sandbox) => Err(SandboxError::failed_precondition(format!(
                "secret {name} conflicts with env variable {name} of sandbox {sandbox}; remove \
                 one of them"
            ))),
            None => Ok(()),
        }
    }
    /// Returns the firebrick sandboxes a global secret with the name goes to: the ones without
    /// a sandbox-scoped secret with that name.
    async fn unscoped_sandboxes(&self, name: &str) -> Result<Vec<SandboxHandle>, SandboxError> {
        let overridden = self.overriding_sandboxes(name)?;

        Ok(list_all_sandboxes()
            .await?
            .into_iter()
            .filter(|handle| ssh::hostname_of(handle).is_some())
            .filter(|handle| !overridden.contains(handle.name()))
            .collect())
    }
    /// Returns the names of the sandboxes with a sandbox-scoped secret with the name.
    fn overriding_sandboxes(&self, name: &str) -> Result<HashSet<String>, SandboxError> {
        Ok(self
            .load_secrets()?
            .iter()
            .filter(|secret| secret.name() == name)
            .filter_map(|secret| secret.sandbox().map(str::to_string))
            .collect())
    }
    /// Applies a change to a global secret to every firebrick sandbox, except the ones with a
    /// sandbox-scoped secret with the same name. Returns the names of the sandboxes the change
    /// failed for.
    async fn update_unscoped_sandboxes(
        &self,
        name: &str,
        change: SecretChange<'_>,
    ) -> Result<Vec<String>, SandboxError> {
        let overridden = self.overriding_sandboxes(name)?;

        update_firebrick_sandboxes(change, &overridden).await
    }
    /// Returns the stored secrets the sandbox with the name gets when it's created: the global
    /// secrets, with its own sandbox-scoped secrets in place of the global ones with the same
    /// name. A new sandbox only has scoped secrets left over from an earlier sandbox with its
    /// name, for example one removed without fbkd, which global changes skip.
    pub(super) fn load_secrets_for(&self, sandbox: &str) -> Result<Vec<Secret>, SandboxError> {
        Ok(secrets::for_sandbox(self.load_secrets()?, sandbox))
    }
    /// Returns the stored secrets.
    fn load_secrets(&self) -> Result<Vec<Secret>, SandboxError> {
        self.secrets.load().map_err(|err| {
            tracing::error!(error = ?err, "failed to load secrets");
            SandboxError::internal("failed to load secrets")
        })
    }
}

/// Fails with `NotFound` when no secret with the name is stored in the scope.
fn ensure_secret_exists(
    secrets: &[Secret],
    name: &str,
    sandbox: Option<&str>,
) -> Result<(), SandboxError> {
    if secrets.iter().any(|secret| secret.matches(name, sandbox)) {
        return Ok(());
    }

    Err(SandboxError::not_found(match sandbox {
        Some(sandbox) => format!("secret {name} doesn't exist in sandbox {sandbox}"),
        None => format!("secret {name} doesn't exist"),
    }))
}

/// Names a secret and its scope for the log.
fn describe_secret(name: &str, sandbox: Option<&str>) -> String {
    match sandbox {
        Some(sandbox) => format!("{name} for sandbox {sandbox}"),
        None => name.to_string(),
    }
}

/// Returns the change that removes the secret from its scope. A sandbox that loses its
/// sandbox-scoped secret gets the global secret with the same name instead, if there is one.
fn removal<'a>(stored: &'a [Secret], name: &'a str, sandbox: Option<&str>) -> SecretChange<'a> {
    let global = stored.iter().find(|secret| secret.matches(name, None));

    match (sandbox, global) {
        (Some(_), Some(global)) => SecretChange::Add(global),
        _ => SecretChange::Remove(name),
    }
}

/// Returns the sandbox a sandbox-scoped secret belongs to, or `None` for a global secret.
/// Fails with an error that names the sandbox when it doesn't exist.
async fn scoped_sandbox(sandbox: Option<&str>) -> Result<Option<SandboxHandle>, SandboxError> {
    let Some(name) = sandbox else {
        return Ok(None);
    };

    get_sandbox(name).await.map(Some).map_err(|err| match err {
        SandboxError::NotFound(_) => {
            SandboxError::not_found(format!("sandbox {name} doesn't exist"))
        }
        err => err,
    })
}

/// A change to the secrets of existing sandboxes.
enum SecretChange<'a> {
    Add(&'a Secret),
    Remove(&'a str),
}

impl fmt::Display for SecretChange<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SecretChange::Add(secret) => write!(f, "add secret {} to", secret.name()),
            SecretChange::Remove(name) => write!(f, "remove secret {name} from"),
        }
    }
}

/// Applies a secret change to every sandbox firebrick created, which are the ones with a host
/// name, except the `skipped` ones. Returns the names of the sandboxes the change failed for.
async fn update_firebrick_sandboxes(
    change: SecretChange<'_>,
    skipped: &HashSet<String>,
) -> Result<Vec<String>, SandboxError> {
    let mut failed = vec![];

    for handle in list_all_sandboxes().await? {
        if ssh::hostname_of(&handle).is_none() || skipped.contains(handle.name()) {
            continue;
        }

        failed.extend(apply_to_sandbox(&handle, &change).await);
    }

    Ok(failed)
}

/// Applies a secret change to one sandbox. Returns its name when the change failed.
async fn apply_to_sandbox(handle: &SandboxHandle, change: &SecretChange<'_>) -> Vec<String> {
    let result = match change {
        SecretChange::Add(secret) => secrets::add_to_sandbox(handle, secret).await,
        SecretChange::Remove(name) => secrets::remove_from_sandbox(handle, name).await,
    };

    match result {
        Ok(()) => vec![],
        Err(err) => {
            tracing::warn!(error = ?err, "failed to {change} sandbox {}", handle.name());
            vec![handle.name().to_string()]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandboxes::tests::scoped_secret;

    #[test]
    fn removal_puts_the_global_secret_back_in_a_sandbox() {
        let stored = [scoped_secret("X", None), scoped_secret("X", Some("a"))];

        assert!(matches!(
            removal(&stored, "X", Some("a")),
            SecretChange::Add(secret) if secret == &stored[0]
        ));
        assert!(matches!(
            removal(&stored[1..], "X", Some("a")),
            SecretChange::Remove("X")
        ));
        assert!(matches!(
            removal(&stored, "X", None),
            SecretChange::Remove("X")
        ));
    }

    #[test]
    fn ensure_secret_exists_checks_the_scope() {
        let global = scoped_secret("X", None);
        let scoped = scoped_secret("X", Some("a"));

        assert_eq!(
            ensure_secret_exists(std::slice::from_ref(&scoped), "X", Some("a")),
            Ok(())
        );
        assert_eq!(
            ensure_secret_exists(std::slice::from_ref(&global), "X", None),
            Ok(())
        );
        assert_eq!(
            ensure_secret_exists(&[global], "X", Some("a")),
            Err(SandboxError::NotFound(
                "secret X doesn't exist in sandbox a".into()
            ))
        );
        assert_eq!(
            ensure_secret_exists(&[scoped], "X", None),
            Err(SandboxError::NotFound("secret X doesn't exist".into()))
        );
    }
}
