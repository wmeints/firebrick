//! `fbk network allow`, `deny`, `enforce`, `enable` and the like: changes the network section of
//! `.firebrick.yml`, asks the user to approve it, and applies it to the working directory's
//! sandbox.

use anyhow::{Context, Result, anyhow};
use firebrick_spec::{InvalidNetworkRule, NetworkRule, NetworkSpec, SandboxSpec};
use std::path::Path;
use tonic::{Code, Status};

use super::Client;
use crate::api::UpdateNetworkRequest;
use crate::approval::Approver;
use crate::client;
use crate::sandbox;
use crate::spec_file::{self, SPEC_FILE_NAME};

/// A change to the network section of a spec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkChange {
    /// Allow the destinations, and stop denying them.
    Allow(Vec<NetworkRule>),
    /// Deny the destinations, and stop allowing them.
    Deny(Vec<NetworkRule>),
    /// Turn enforcement of the rules on or off.
    Enforce(bool),
    /// Give the sandbox a network device, or remove it.
    Enable(bool),
}

impl NetworkChange {
    /// Returns the change that allows the rules, failing on the first invalid rule.
    pub fn allow(rules: &[String]) -> Result<Self, InvalidNetworkRule> {
        Ok(Self::Allow(parse_rules(rules)?))
    }

    /// Returns the change that denies the rules, failing on the first invalid rule.
    pub fn deny(rules: &[String]) -> Result<Self, InvalidNetworkRule> {
        Ok(Self::Deny(parse_rules(rules)?))
    }
}

/// Parses network rules, failing on the first invalid one.
fn parse_rules(rules: &[String]) -> Result<Vec<NetworkRule>, InvalidNetworkRule> {
    rules.iter().map(|rule| rule.parse()).collect()
}

/// Applies the change to `.firebrick.yml` in the working directory, then sends the network
/// section to the sandbox when it exists, also when the file didn't change, so a sandbox that
/// missed an earlier update or a hand edit catches up. Without a spec file, it creates one with
/// the default settings and the name `fbk start` would use. Fails before it writes the file
/// when the sandbox belongs to another directory. Before sending, asks the user to approve the
/// section when it differs from the approved one with the change applied, for example after a
/// hand edit; `assume_yes` approves it without asking. Warns when rules are added while
/// enforcement is off.
pub async fn update(change: NetworkChange, working_dir: &Path, assume_yes: bool) -> Result<()> {
    let approver = Approver::new(assume_yes);

    update_with(change, working_dir, client::connect().await, approver).await
}

/// Runs [`update`] with the result of connecting to the daemon, getting approval from
/// `approver`. Without a spec file, the default spec is named the way `fbk start` names its
/// sandbox, which needs the daemon to keep the name of an existing legacy sandbox.
async fn update_with(
    change: NetworkChange,
    working_dir: &Path,
    mut client: Result<Client>,
    approver: Approver,
) -> Result<()> {
    let spec_path = working_dir.join(SPEC_FILE_NAME);
    let (spec, created) = match spec_file::load_spec(&spec_path)? {
        Some(spec) => (spec, false),
        None => {
            let mut connected = client.with_context(|| {
                format!(
                    "failed to connect to the daemon to name the sandbox in a new {SPEC_FILE_NAME}"
                )
            })?;
            let spec = sandbox::resolve_spec(working_dir, &mut connected).await?;
            client = Ok(connected);
            (spec, true)
        }
    };

    check_owner(&spec, working_dir, &mut client).await?;

    let (spec, changed) = edit_spec(&spec_path, spec, &change, created)?;
    approve(&change, &spec, working_dir, approver).await?;
    let result = apply_to_sandbox(&change, &spec, changed, client).await;

    if let Some(warning) = enforcement_warning(&change, &spec) {
        eprintln!("{warning}");
    }

    result
}

/// Fails when the spec's sandbox belongs to another directory. Without a connection to the
/// daemon, nothing can reach the sandbox, so the file may still change.
async fn check_owner(
    spec: &SandboxSpec,
    working_dir: &Path,
    client: &mut Result<Client>,
) -> Result<()> {
    match client {
        Ok(client) => sandbox::check_workspace_owner(&spec.name, working_dir, client).await,
        Err(_) => Ok(()),
    }
}

/// Applies the change to the spec and writes the spec file when the network section changed or
/// the file is `new`. Returns the spec and whether the file was written.
fn edit_spec(
    spec_path: &Path,
    mut spec: SandboxSpec,
    change: &NetworkChange,
    new: bool,
) -> Result<(SandboxSpec, bool)> {
    let changed = apply(change, &mut spec.network) || new;

    if changed {
        firebrick_spec::to_file(&spec, spec_path)?;
    }

    Ok((spec, changed))
}

/// Makes sure the user approved the network section of the spec before it's sent, asking on the
/// terminal when it differs from the approved section with the change applied.
async fn approve(
    change: &NetworkChange,
    spec: &SandboxSpec,
    working_dir: &Path,
    approver: Approver,
) -> Result<()> {
    let sent = spec.network.clone().unwrap_or_default();
    let change = change.clone();

    approver
        .run(working_dir, move |approvals, prompt| {
            approvals.approve_network(sent, |approved| expected_network(approved, &change), prompt)
        })
        .await
}

/// Returns the network section the user expects to send: the approved one with the change
/// applied.
fn expected_network(approved: &NetworkSpec, change: &NetworkChange) -> NetworkSpec {
    let mut network = Some(approved.clone());
    apply(change, &mut network);

    network.unwrap_or_default()
}

/// Applies the change to a spec's network section, adding the section when it's missing.
/// Returns whether the network section changed.
fn apply(change: &NetworkChange, network: &mut Option<NetworkSpec>) -> bool {
    let added = network.is_none();
    let network = network.get_or_insert_default();

    let changed = match change {
        NetworkChange::Allow(rules) => network.allow(rules),
        NetworkChange::Deny(rules) => network.deny(rules),
        NetworkChange::Enforce(enforce) => {
            std::mem::replace(&mut network.enforce, *enforce) != *enforce
        }
        NetworkChange::Enable(enable) => {
            let changed = network.is_enabled() != *enable;
            network.enabled = Some(*enable);
            changed
        }
    };

    added || changed
}

/// Sends the network section of the spec to the daemon, which applies it to the sandbox when
/// the sandbox doesn't have it yet, and prints the outcome. `file_changed` tells whether the
/// spec file was just written, and `client` is the connection to the daemon, if any.
async fn apply_to_sandbox(
    change: &NetworkChange,
    spec: &SandboxSpec,
    file_changed: bool,
    client: Result<Client>,
) -> Result<()> {
    let name = &spec.name;
    let mut client = client.with_context(|| {
        if file_changed {
            format!(
                "updated {SPEC_FILE_NAME}, but failed to connect to the daemon; run the command \
                 again to apply the change to {name}"
            )
        } else {
            "failed to connect to the daemon".to_string()
        }
    })?;

    let request = UpdateNetworkRequest {
        name: name.clone(),
        network: Some(sandbox::network_policy(
            spec.network.clone().unwrap_or_default(),
        )),
    };
    let result = client
        .update_network(request)
        .await
        .map(|response| response.into_inner().updated);

    println!("{}", outcome(change, name, file_changed, result)?);
    sandbox::warn_if_outdated(&mut client, name).await;

    Ok(())
}

/// Returns the message for the daemon's answer, which tells whether the sandbox was recreated.
/// A sandbox that doesn't exist gets the network settings from the spec file when it starts.
fn outcome(
    change: &NetworkChange,
    name: &str,
    file_changed: bool,
    result: Result<bool, Status>,
) -> Result<String> {
    let sandbox_updated = match result {
        Ok(updated) => Some(updated),
        Err(status) if status.code() == Code::NotFound => None,
        Err(status) => return Err(anyhow!("{}", status.message())),
    };

    Ok(match (file_changed, sandbox_updated) {
        (_, Some(true)) => updated_message(change, name),
        (false, _) => unchanged_message(change),
        (true, Some(false)) => format!("updated {SPEC_FILE_NAME}; {name} is already up to date"),
        (true, None) => format!(
            "updated {SPEC_FILE_NAME}; the {} when {name} starts",
            match change {
                NetworkChange::Enable(_) => "change applies",
                _ => "rules apply",
            }
        ),
    })
}

/// Returns the message for a sandbox that was recreated with the change.
fn updated_message(change: &NetworkChange, name: &str) -> String {
    match change {
        NetworkChange::Enable(true) => format!("enabled the network of {name}"),
        NetworkChange::Enable(false) => format!("disabled the network of {name}"),
        _ => format!("updated the network rules of {name}"),
    }
}

/// Returns the message for a change that neither the file nor the sandbox needed.
fn unchanged_message(change: &NetworkChange) -> String {
    match change {
        NetworkChange::Enable(true) => "the network is already enabled".to_string(),
        NetworkChange::Enable(false) => "the network is already disabled".to_string(),
        _ => "network rules are already up to date".to_string(),
    }
}

/// Returns a warning when the change adds rules to a spec that doesn't enforce them, so the
/// user doesn't think the rules already protect them.
fn enforcement_warning(change: &NetworkChange, spec: &SandboxSpec) -> Option<String> {
    let adds_rules = matches!(change, NetworkChange::Allow(_) | NetworkChange::Deny(_));
    let enforced = spec.network.as_ref().is_some_and(|network| network.enforce);

    (adds_rules && !enforced).then(|| {
        format!(
            "warning: the network policy of {} isn't enabled, so its rules aren't enforced. Run \
             `fbk network policy enable` to enforce them.",
            spec.name
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::tests::workspace;
    use crate::test_support::fake_daemon::{self, OTHER_WORKSPACE};
    use crate::test_support::symlinked_spec;
    use std::fs;
    use tempfile::TempDir;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(ToString::to_string).collect()
    }

    fn rules(values: &[&str]) -> Vec<NetworkRule> {
        values.iter().map(|value| value.parse().unwrap()).collect()
    }

    fn spec_with(network: Option<NetworkSpec>) -> SandboxSpec {
        SandboxSpec {
            network,
            ..firebrick_spec::default_spec("my-project".to_string())
        }
    }

    fn enforced(enforce: bool) -> Option<NetworkSpec> {
        Some(NetworkSpec {
            enforce,
            ..NetworkSpec::default()
        })
    }

    /// Returns a working directory whose spec file names the sandbox `dev`, and the path of
    /// the spec file.
    /// Returns an approver that approves without asking and keeps its approvals in `dir`, so
    /// the tests never touch the user's own approvals.
    fn approve_all(dir: &TempDir) -> Approver {
        Approver::at(dir.path().join("approved.yml"), true)
    }

    #[tokio::test]
    async fn update_refuses_sandbox_of_other_workspace_before_writing() {
        let (dir, spec_path) = workspace();
        let client = fake_daemon::serve("dev", OTHER_WORKSPACE).await;

        let result = update_with(rules_change(), dir.path(), Ok(client), approve_all(&dir)).await;

        fake_daemon::assert_dev_belongs_to_other(result);
        assert_eq!(fs::read_to_string(&spec_path).unwrap(), "name: dev\n");
    }

    #[tokio::test]
    async fn update_sends_the_network_of_the_working_directory_sandbox() {
        let (dir, _) = workspace();
        let canonical = dunce::canonicalize(dir.path()).unwrap();
        let client = fake_daemon::serve("dev", canonical.to_str().unwrap()).await;

        let error = update_with(rules_change(), dir.path(), Ok(client), approve_all(&dir))
            .await
            .unwrap_err();

        assert_eq!(
            error.to_string(),
            fake_daemon::refusal("update_network", "dev")
        );
    }

    #[tokio::test]
    async fn update_without_spec_file_names_and_sends_a_new_sandbox() {
        let dir = TempDir::new().unwrap();
        let client = fake_daemon::serve("other", OTHER_WORKSPACE).await;

        let error = update_with(rules_change(), dir.path(), Ok(client), approve_all(&dir))
            .await
            .unwrap_err();

        let spec = firebrick_spec::from_file(&dir.path().join(SPEC_FILE_NAME))
            .unwrap()
            .unwrap();
        assert_eq!(
            error.to_string(),
            fake_daemon::refusal("update_network", &spec.name)
        );
    }

    #[tokio::test]
    async fn update_without_daemon_still_writes_the_spec_file() {
        let (dir, spec_path) = workspace();

        let error = update_with(
            rules_change(),
            dir.path(),
            Err(anyhow!("no daemon")),
            approve_all(&dir),
        )
        .await
        .unwrap_err();

        assert!(
            error
                .to_string()
                .starts_with("updated .firebrick.yml, but failed to connect to the daemon"),
            "{error}"
        );
        assert!(
            fs::read_to_string(&spec_path)
                .unwrap()
                .contains("example.org")
        );
    }

    async fn assert_update_refuses_symlink(content: Option<&str>) {
        let Some((dir, target)) = symlinked_spec::workspace(content) else {
            return;
        };
        // The sandbox belongs to this directory, so only the symlink check stops the write.
        let workspace = dunce::canonicalize(dir.path()).unwrap();
        let client = fake_daemon::serve("dev", workspace.to_str().unwrap()).await;

        let error = update_with(rules_change(), dir.path(), Ok(client), approve_all(&dir))
            .await
            .unwrap_err();

        assert_eq!(error.to_string(), symlinked_spec::SYMLINK_ERROR);
        assert_eq!(symlinked_spec::target_content(&target).as_deref(), content);
    }

    #[tokio::test]
    async fn update_refuses_a_symlink_to_a_spec_and_leaves_it_alone() {
        assert_update_refuses_symlink(Some("name: dev\n")).await;
    }

    #[tokio::test]
    async fn update_refuses_a_dangling_symlink_and_creates_no_file() {
        assert_update_refuses_symlink(None).await;
    }

    #[test]
    fn allow_and_deny_parse_the_rules() {
        assert_eq!(
            NetworkChange::allow(&strings(&["example.org", "*.npmjs.org"])).unwrap(),
            NetworkChange::Allow(rules(&["example.org", "*.npmjs.org"]))
        );
        assert_eq!(
            NetworkChange::deny(&strings(&["10.0.0.0/8"])).unwrap(),
            NetworkChange::Deny(rules(&["10.0.0.0/8"]))
        );
    }

    #[test]
    fn invalid_rule_is_rejected_with_its_syntax() {
        let err = NetworkChange::allow(&strings(&["example.org", "github.com:443"])).unwrap_err();

        assert_eq!(
            err.to_string(),
            "invalid network rule \"github.com:443\": use a host name, *.domain, an IP address or \
             a CIDR range"
        );
    }

    #[test]
    fn apply_adds_a_network_section_when_missing() {
        let mut spec = spec_with(None);

        assert!(apply(&NetworkChange::Enforce(false), &mut spec.network));

        assert_eq!(spec.network, enforced(false));
    }

    #[test]
    fn apply_moves_rules_between_the_lists() {
        let mut spec = spec_with(Some(NetworkSpec {
            enforce: true,
            allow: rules(&["github.com"]),
            deny: rules(&["example.org"]),
            ..NetworkSpec::default()
        }));

        assert!(apply(
            &NetworkChange::Allow(rules(&["example.org"])),
            &mut spec.network
        ));

        assert_eq!(
            spec.network.unwrap(),
            NetworkSpec {
                enforce: true,
                allow: rules(&["github.com", "example.org"]),
                ..NetworkSpec::default()
            }
        );
    }

    #[test]
    fn apply_reports_unchanged_rules_and_enforcement() {
        let mut spec = spec_with(Some(NetworkSpec {
            enforce: true,
            allow: rules(&["github.com"]),
            deny: rules(&["example.org"]),
            ..NetworkSpec::default()
        }));

        assert!(!apply(
            &NetworkChange::Allow(rules(&["github.com"])),
            &mut spec.network
        ));
        assert!(!apply(
            &NetworkChange::Deny(rules(&["example.org"])),
            &mut spec.network
        ));
        assert!(!apply(&NetworkChange::Enforce(true), &mut spec.network));
        assert!(apply(&NetworkChange::Enforce(false), &mut spec.network));
    }

    #[test]
    fn edit_spec_writes_the_changed_spec() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(SPEC_FILE_NAME);
        fs::write(&path, "name: my-project\nimage: ubuntu:24.04\n").unwrap();
        let spec = firebrick_spec::from_file(&path).unwrap().unwrap();

        let (_, changed) = edit_spec(
            &path,
            spec,
            &NetworkChange::Allow(rules(&["example.org"])),
            false,
        )
        .unwrap();
        let written = firebrick_spec::from_file(&path).unwrap().unwrap();

        assert!(changed);
        assert_eq!(written.image.as_deref(), Some("ubuntu:24.04"));
        assert_eq!(
            written.network.unwrap(),
            NetworkSpec {
                allow: rules(&["example.org"]),
                ..NetworkSpec::default()
            }
        );
    }

    #[test]
    fn edit_spec_leaves_an_unchanged_file_alone() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(SPEC_FILE_NAME);
        let content = "# my rules\nname: my-project\nnetwork:\n  allow: [example.org]\n";
        fs::write(&path, content).unwrap();
        let spec = firebrick_spec::from_file(&path).unwrap().unwrap();

        let (_, changed) = edit_spec(
            &path,
            spec,
            &NetworkChange::Allow(rules(&["example.org"])),
            false,
        )
        .unwrap();

        assert!(!changed);
        assert_eq!(fs::read_to_string(&path).unwrap(), content);
    }

    #[test]
    fn edit_spec_creates_a_new_file_with_the_defaults() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(SPEC_FILE_NAME);
        let spec = firebrick_spec::default_spec("firebrick-abc123".to_string());

        let (_, changed) = edit_spec(
            &path,
            spec,
            &NetworkChange::Allow(rules(&["example.org"])),
            true,
        )
        .unwrap();
        let written = firebrick_spec::from_file(&path).unwrap().unwrap();

        assert!(changed);
        assert_eq!(written.name, "firebrick-abc123");
        assert_eq!(written.image, None);
        assert_eq!(written.init, Some(true));
        assert_eq!(written.mise.and_then(|mise| mise.enabled), Some(true));
        assert_eq!(
            written.network.unwrap(),
            NetworkSpec {
                allow: rules(&["example.org"]),
                ..NetworkSpec::default()
            }
        );
    }

    #[test]
    fn edit_spec_writes_a_new_file_even_without_a_change() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(SPEC_FILE_NAME);
        let spec = spec_with(enforced(false));

        let (_, changed) = edit_spec(&path, spec, &NetworkChange::Enforce(false), true).unwrap();

        assert!(changed);
        assert!(path.is_file());
    }

    #[test]
    fn read_spec_reports_an_invalid_file_like_validate() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(SPEC_FILE_NAME);
        fs::write(
            &path,
            "name: dev\nnetwork:\n  allow: [\"github.com:443\"]\n",
        )
        .unwrap();

        let err = spec_file::load_spec(&path).unwrap_err();

        assert!(err.to_string().starts_with(".firebrick.yml:3:"), "{err}");
        assert!(err.to_string().contains("github.com:443"), "{err}");
    }

    #[test]
    fn read_spec_returns_none_without_a_file() {
        let dir = TempDir::new().unwrap();

        assert!(
            spec_file::load_spec(&dir.path().join(SPEC_FILE_NAME))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn expected_network_applies_the_change_to_the_approved_section() {
        let approved = NetworkSpec {
            enforce: true,
            deny: rules(&["example.org"]),
            ..NetworkSpec::default()
        };

        assert_eq!(
            expected_network(&approved, &NetworkChange::Allow(rules(&["example.org"]))),
            NetworkSpec {
                enforce: true,
                allow: rules(&["example.org"]),
                ..NetworkSpec::default()
            }
        );
        assert_eq!(
            expected_network(&approved, &NetworkChange::Enable(false)),
            NetworkSpec {
                enabled: Some(false),
                ..approved.clone()
            }
        );
    }

    #[test]
    fn expected_network_ignores_hand_edits_to_the_spec() {
        // Whatever the spec says, the expected section starts from the approved one.
        let approved = NetworkSpec {
            enforce: true,
            ..NetworkSpec::default()
        };

        assert_eq!(
            expected_network(&approved, &NetworkChange::Allow(rules(&["example.org"]))),
            NetworkSpec {
                enforce: true,
                allow: rules(&["example.org"]),
                ..NetworkSpec::default()
            }
        );
    }

    fn rules_change() -> NetworkChange {
        NetworkChange::Allow(rules(&["example.org"]))
    }

    #[test]
    fn apply_sets_the_network_switch() {
        let mut spec = spec_with(None);

        assert!(apply(&NetworkChange::Enable(false), &mut spec.network));
        assert_eq!(spec.network.as_ref().unwrap().enabled, Some(false));
        assert!(!apply(&NetworkChange::Enable(false), &mut spec.network));
        assert!(apply(&NetworkChange::Enable(true), &mut spec.network));
        assert!(spec.network.as_ref().unwrap().is_enabled());
    }

    #[test]
    fn apply_treats_a_missing_switch_as_enabled() {
        let mut spec = spec_with(enforced(true));

        assert!(!apply(&NetworkChange::Enable(true), &mut spec.network));
        assert!(apply(&NetworkChange::Enable(false), &mut spec.network));
    }

    #[test]
    fn apply_keeps_the_rules_when_disabling_the_network() {
        let rules_spec = NetworkSpec {
            enforce: true,
            allow: rules(&["github.com"]),
            ..NetworkSpec::default()
        };
        let mut spec = spec_with(Some(rules_spec.clone()));

        apply(&NetworkChange::Enable(false), &mut spec.network);

        assert_eq!(
            spec.network.unwrap(),
            NetworkSpec {
                enabled: Some(false),
                ..rules_spec
            }
        );
    }

    #[test]
    fn edit_spec_writes_the_network_switch() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(SPEC_FILE_NAME);
        fs::write(&path, "name: my-project\n").unwrap();
        let spec = firebrick_spec::from_file(&path).unwrap().unwrap();

        let (_, changed) = edit_spec(&path, spec, &NetworkChange::Enable(false), false).unwrap();
        let written = firebrick_spec::from_file(&path).unwrap().unwrap();

        assert!(changed);
        assert_eq!(written.network.unwrap().enabled, Some(false));
    }

    #[test]
    fn outcome_reports_the_network_switch() {
        let cases = [
            (true, "enabled the network of my-project"),
            (false, "disabled the network of my-project"),
        ];

        for (enable, expected) in cases {
            assert_eq!(
                outcome(&NetworkChange::Enable(enable), "my-project", true, Ok(true)).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn outcome_without_a_switch_change_says_the_network_already_has_it() {
        let cases = [
            (true, Ok(false), "the network is already enabled"),
            (true, not_found(), "the network is already enabled"),
            (false, Ok(false), "the network is already disabled"),
            (false, not_found(), "the network is already disabled"),
        ];

        for (enable, result, expected) in cases {
            assert_eq!(
                outcome(&NetworkChange::Enable(enable), "my-project", false, result).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn outcome_of_a_switch_for_a_missing_sandbox_says_when_it_applies() {
        assert_eq!(
            outcome(
                &NetworkChange::Enable(false),
                "my-project",
                true,
                not_found()
            )
            .unwrap(),
            "updated .firebrick.yml; the change applies when my-project starts"
        );
    }

    #[test]
    fn warning_is_not_shown_for_the_network_switch() {
        assert_eq!(
            enforcement_warning(&NetworkChange::Enable(false), &spec_with(enforced(false))),
            None
        );
    }

    fn not_found() -> Result<bool, Status> {
        Err(Status::not_found("couldn't find specified sandbox"))
    }

    #[test]
    fn outcome_reports_the_updated_sandbox() {
        for file_changed in [true, false] {
            assert_eq!(
                outcome(&rules_change(), "my-project", file_changed, Ok(true)).unwrap(),
                "updated the network rules of my-project"
            );
        }
    }

    #[test]
    fn outcome_for_a_missing_sandbox_says_when_the_rules_apply() {
        assert_eq!(
            outcome(&rules_change(), "my-project", true, not_found()).unwrap(),
            "updated .firebrick.yml; the rules apply when my-project starts"
        );
    }

    #[test]
    fn outcome_reports_a_sandbox_that_already_has_the_rules() {
        assert_eq!(
            outcome(&rules_change(), "my-project", true, Ok(false)).unwrap(),
            "updated .firebrick.yml; my-project is already up to date"
        );
    }

    #[test]
    fn outcome_without_any_change_is_up_to_date() {
        for result in [Ok(false), not_found()] {
            assert_eq!(
                outcome(&rules_change(), "my-project", false, result).unwrap(),
                "network rules are already up to date"
            );
        }
    }

    #[test]
    fn outcome_passes_on_other_errors() {
        let result = Err(Status::internal(
            "failed to update the network rules of my-project",
        ));

        assert_eq!(
            outcome(&rules_change(), "my-project", true, result)
                .unwrap_err()
                .to_string(),
            "failed to update the network rules of my-project"
        );
    }

    #[test]
    fn warning_is_shown_when_rules_are_added_without_enforcement() {
        let allow = NetworkChange::Allow(rules(&["example.org"]));
        let deny = NetworkChange::Deny(rules(&["example.org"]));
        let cases = [
            (&allow, spec_with(enforced(false))),
            (&deny, spec_with(enforced(false))),
            (&allow, spec_with(None)),
        ];

        for (change, spec) in cases {
            assert_eq!(
                enforcement_warning(change, &spec).as_deref(),
                Some(
                    "warning: the network policy of my-project isn't enabled, so its rules \
                     aren't enforced. Run `fbk network policy enable` to enforce them."
                )
            );
        }
    }

    #[test]
    fn warning_is_not_shown_with_enforcement_or_for_policy_changes() {
        let allow = NetworkChange::Allow(rules(&["example.org"]));

        assert_eq!(
            enforcement_warning(&allow, &spec_with(enforced(true))),
            None
        );
        assert_eq!(
            enforcement_warning(&NetworkChange::Enforce(false), &spec_with(enforced(false))),
            None
        );
        assert_eq!(
            enforcement_warning(&NetworkChange::Enforce(true), &spec_with(enforced(true))),
            None
        );
    }
}
