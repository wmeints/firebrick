//! Network settings: `fbk network` changes the network section of `.firebrick.yml` and applies it
//! to the sandbox of the working directory, so the file and the sandbox keep the same network
//! switch and rules. `fbk network log` shows the network activity fbkd recorded for the sandbox.

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Local, TimeZone};
use firebrick_spec::{InvalidNetworkRule, NetworkRule, NetworkSpec, SandboxSpec};
use std::fmt::Display;
use std::path::Path;
use tonic::transport::Channel;
use tonic::{Code, Status};

use crate::api::sandbox_management_service_client::SandboxManagementServiceClient;
use crate::api::{
    ListNetworkActivityRequest, NetworkActivity, NetworkActivityKind, UpdateNetworkRequest,
};
use crate::approval::Approver;
use crate::manage::{self, SPEC_FILE_NAME};
use crate::{client, table, validate};

/// The first fbk release whose sandboxes get a network log.
const NETWORK_LOG_VERSION: &str = "0.7.0";

type Client = SandboxManagementServiceClient<Channel>;

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
    let (spec, created) = match read_spec(&spec_path)? {
        Some(spec) => (spec, false),
        None => {
            let mut connected = client.with_context(|| {
                format!(
                    "failed to connect to the daemon to name the sandbox in a new {SPEC_FILE_NAME}"
                )
            })?;
            let spec = manage::resolve_spec(working_dir, &mut connected).await?;
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

/// Loads the spec file, or returns `None` when it doesn't exist. An invalid file is reported
/// like `fbk validate` reports it, and a symlink is refused.
fn read_spec(spec_path: &Path) -> Result<Option<SandboxSpec>> {
    validate::load_spec(spec_path)
}

/// Fails when the spec's sandbox belongs to another directory. Without a connection to the
/// daemon, nothing can reach the sandbox, so the file may still change.
async fn check_owner(
    spec: &SandboxSpec,
    working_dir: &Path,
    client: &mut Result<Client>,
) -> Result<()> {
    match client {
        Ok(client) => manage::check_workspace_owner(&spec.name, working_dir, client).await,
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
        firebrick_spec::to_file(&spec, spec_path)
            .with_context(|| format!("failed to write {}", spec_path.display()))?;
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
        network: Some(manage::network_policy(
            spec.network.clone().unwrap_or_default(),
        )),
    };
    let result = client
        .update_network(request)
        .await
        .map(|response| response.into_inner().updated);

    println!("{}", outcome(change, name, file_changed, result)?);
    manage::warn_if_outdated(&mut client, name).await;

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

/// Prints the network activity recorded for the working directory's sandbox, newest first, in
/// the local time zone. Only reads: it changes neither the spec file nor the approvals.
pub async fn log(working_dir: &Path, client: &mut Client) -> Result<()> {
    print!("{}", activity_log(working_dir, client, &Local).await?);

    Ok(())
}

/// Returns the text [`log`] prints, with the times in the time zone `tz`.
async fn activity_log<Tz: TimeZone>(
    working_dir: &Path,
    client: &mut Client,
    tz: &Tz,
) -> Result<String>
where
    Tz::Offset: Display,
{
    let name = manage::resolve_owned_spec(working_dir, client).await?.name;
    let request = ListNetworkActivityRequest { name: name.clone() };

    let entries = match client.list_network_activity(request).await {
        Ok(response) => response.into_inner().entries,
        Err(status) if status.code() == Code::FailedPrecondition => bail!(
            "Network logging isn't available for {name}: it was created before fbk \
             {NETWORK_LOG_VERSION}. Remove and start it again to turn it on, or change its \
             network settings, for example with fbk network allow; a command that leaves them as \
             they are doesn't restart it."
        ),
        Err(status) => return Err(manage::describe_status(&name, status)),
    };

    if entries.is_empty() {
        return Ok(format!("No network activity recorded for {name} yet.\n"));
    }

    render_activity(&entries, tz)
}

const ACTIVITY_HEADER: [&str; 6] = [
    "DESTINATION",
    "KIND",
    "PORTS",
    "COUNT",
    "LAST SEEN",
    "POLICY",
];

/// Renders the entries as a table in their order, with the times in the time zone `tz`.
/// Destinations come from inside the sandbox, so their control characters are removed.
fn render_activity<Tz: TimeZone>(entries: &[NetworkActivity], tz: &Tz) -> Result<String>
where
    Tz::Offset: Display,
{
    let rows = entries
        .iter()
        .map(|entry| activity_row(entry, tz))
        .collect::<Result<Vec<_>>>()?;

    table::render(ACTIVITY_HEADER, &rows)
}

/// Returns the table row of an entry.
fn activity_row<Tz: TimeZone>(entry: &NetworkActivity, tz: &Tz) -> Result<[String; 6]>
where
    Tz::Offset: Display,
{
    let ports: Vec<String> = entry.ports.iter().map(ToString::to_string).collect();

    Ok([
        firebrick_utils::strip_control(&entry.destination),
        activity_kind(entry.kind).to_string(),
        ports.join(","),
        entry.count.to_string(),
        local_time(entry.last_seen, tz)?,
        if entry.allowed { "allowed" } else { "denied" }.to_string(),
    ])
}

/// Returns the name of the kind as the table shows it. A kind this version doesn't know, for
/// example one a newer daemon added, is `unknown`, so it can't pass for another kind.
fn activity_kind(kind: i32) -> &'static str {
    match NetworkActivityKind::try_from(kind) {
        Ok(NetworkActivityKind::Dns) => "dns",
        Ok(NetworkActivityKind::Tcp) => "tcp",
        Ok(NetworkActivityKind::Udp) => "udp",
        Ok(NetworkActivityKind::Unspecified) | Err(_) => "unknown",
    }
}

/// Formats seconds since the Unix epoch as a date and time in the time zone `tz`.
fn local_time<Tz: TimeZone>(seconds: i64, tz: &Tz) -> Result<String>
where
    Tz::Offset: Display,
{
    let time = DateTime::from_timestamp(seconds, 0)
        .ok_or_else(|| anyhow!("the daemon sent an invalid time: {seconds}"))?;

    Ok(time
        .with_timezone(tz)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake_daemon::{self, OTHER_WORKSPACE};
    use crate::symlinked_spec;
    use std::fs;
    use std::path::PathBuf;
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

    fn workspace() -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let spec_path = dir.path().join(SPEC_FILE_NAME);
        fs::write(&spec_path, "name: dev\n").unwrap();
        (dir, spec_path)
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
        let canonical = fs::canonicalize(dir.path()).unwrap();
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

        let spec = firebrick_spec::from_file(&dir.path().join(SPEC_FILE_NAME)).unwrap();
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
        let (dir, target) = symlinked_spec::workspace(content);
        let client = fake_daemon::serve("dev", OTHER_WORKSPACE).await;

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
        let spec = firebrick_spec::from_file(&path).unwrap();

        let (_, changed) = edit_spec(
            &path,
            spec,
            &NetworkChange::Allow(rules(&["example.org"])),
            false,
        )
        .unwrap();
        let written = firebrick_spec::from_file(&path).unwrap();

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
        let spec = firebrick_spec::from_file(&path).unwrap();

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
        let written = firebrick_spec::from_file(&path).unwrap();

        assert!(changed);
        assert_eq!(written.name, "firebrick-abc123");
        assert_eq!(written.image, None);
        assert_eq!(written.init, Some(true));
        assert_eq!(written.mise, Some(true));
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

        let err = read_spec(&path).unwrap_err();

        assert!(err.to_string().starts_with(".firebrick.yml:3:"), "{err}");
        assert!(err.to_string().contains("github.com:443"), "{err}");
    }

    #[test]
    fn read_spec_returns_none_without_a_file() {
        let dir = TempDir::new().unwrap();

        assert!(
            read_spec(&dir.path().join(SPEC_FILE_NAME))
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
        let spec = firebrick_spec::from_file(&path).unwrap();

        let (_, changed) = edit_spec(&path, spec, &NetworkChange::Enable(false), false).unwrap();
        let written = firebrick_spec::from_file(&path).unwrap();

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

    /// Returns a denied entry without ports, last seen `seconds` after 2026-10-11 14:00:00 UTC.
    fn activity(kind: NetworkActivityKind, destination: &str, seconds: i64) -> NetworkActivity {
        NetworkActivity {
            kind: kind.into(),
            destination: destination.to_string(),
            count: 12,
            first_seen: 1_791_727_200,
            last_seen: 1_791_727_200 + seconds,
            ..NetworkActivity::default()
        }
    }

    /// Returns `entry` with the ports, allowed by the rules.
    fn allowed(entry: NetworkActivity, ports: &[u32]) -> NetworkActivity {
        NetworkActivity {
            ports: ports.to_vec(),
            allowed: true,
            ..entry
        }
    }

    /// The table of [`github_activity`] in UTC.
    const GITHUB_TABLE: &str = "\
        ┌──────────────────┬──────┬────────┬───────┬─────────────────────┬─────────┐\n\
        │ DESTINATION      │ KIND │ PORTS  │ COUNT │ LAST SEEN           │ POLICY  │\n\
        ├──────────────────┼──────┼────────┼───────┼─────────────────────┼─────────┤\n\
        │ evil.example.com │ dns  │        │ 12    │ 2026-10-11 14:03:40 │ denied  │\n\
        │ api.github.com   │ dns  │        │ 12    │ 2026-10-11 14:02:11 │ allowed │\n\
        │ 140.82.112.6     │ tcp  │ 80,443 │ 12    │ 2026-10-11 14:02:11 │ allowed │\n\
        └──────────────────┴──────┴────────┴───────┴─────────────────────┴─────────┘\n";

    /// Returns a record with a denied DNS lookup, and an allowed lookup and connection.
    fn github_activity() -> Vec<NetworkActivity> {
        vec![
            activity(NetworkActivityKind::Dns, "evil.example.com", 220),
            allowed(
                activity(NetworkActivityKind::Dns, "api.github.com", 131),
                &[],
            ),
            allowed(
                activity(NetworkActivityKind::Tcp, "140.82.112.6", 131),
                &[80, 443],
            ),
        ]
    }

    /// Returns a working directory whose spec file names `dev`, the spec file's content and a
    /// fake daemon whose sandbox `dev` mounts it and recorded `activity`.
    async fn log_workspace(activity: Option<Vec<NetworkActivity>>) -> (TempDir, String, Client) {
        let (dir, spec_path) = workspace();
        let canonical = fs::canonicalize(dir.path()).unwrap();
        let client =
            fake_daemon::serve_with_activity("dev", canonical.to_str().unwrap(), activity).await;
        let content = fs::read_to_string(spec_path).unwrap();

        (dir, content, client)
    }

    #[tokio::test]
    async fn log_prints_the_activity_as_a_table_in_the_time_zone() {
        let (dir, _, mut client) = log_workspace(Some(github_activity())).await;

        let output = activity_log(dir.path(), &mut client, &chrono::Utc)
            .await
            .unwrap();

        assert_eq!(output, GITHUB_TABLE);
    }

    #[tokio::test]
    async fn log_shows_the_times_in_the_given_time_zone() {
        let entries = vec![allowed(
            activity(NetworkActivityKind::Udp, "10.0.0.1", 0),
            &[53],
        )];
        let (dir, _, mut client) = log_workspace(Some(entries)).await;
        let amsterdam = chrono::FixedOffset::east_opt(2 * 3600).unwrap();

        let output = activity_log(dir.path(), &mut client, &amsterdam)
            .await
            .unwrap();

        assert!(output.contains("│ udp  │ 53    │"), "{output}");
        assert!(output.contains("2026-10-11 16:00:00"), "{output}");
    }

    #[tokio::test]
    async fn log_of_an_empty_record_says_nothing_was_recorded() {
        let (dir, _, mut client) = log_workspace(Some(vec![])).await;

        let output = activity_log(dir.path(), &mut client, &chrono::Utc)
            .await
            .unwrap();

        assert_eq!(output, "No network activity recorded for dev yet.\n");
    }

    #[tokio::test]
    async fn log_of_a_sandbox_without_a_network_log_explains_how_to_turn_it_on() {
        let (dir, _, mut client) = log_workspace(None).await;

        let error = activity_log(dir.path(), &mut client, &chrono::Utc)
            .await
            .unwrap_err();

        assert_eq!(
            error.to_string(),
            "Network logging isn't available for dev: it was created before fbk 0.7.0. Remove \
             and start it again to turn it on, or change its network settings, for example with \
             fbk network allow; a command that leaves them as they are doesn't restart it."
        );
    }

    #[tokio::test]
    async fn log_of_a_missing_sandbox_says_it_does_not_exist() {
        let (dir, _) = workspace();
        let mut client = fake_daemon::serve("other", OTHER_WORKSPACE).await;

        let error = activity_log(dir.path(), &mut client, &chrono::Utc)
            .await
            .unwrap_err();

        assert_eq!(error.to_string(), "sandbox dev doesn't exist");
    }

    #[tokio::test]
    async fn log_refuses_sandbox_of_other_workspace() {
        let (dir, _) = workspace();
        let mut client = fake_daemon::serve("dev", OTHER_WORKSPACE).await;

        let result = activity_log(dir.path(), &mut client, &chrono::Utc).await;

        fake_daemon::assert_dev_belongs_to_other(result);
    }

    #[tokio::test]
    async fn log_shows_kinds_it_does_not_know_as_unknown() {
        let entries = vec![
            activity(NetworkActivityKind::Unspecified, "10.0.0.1", 1),
            NetworkActivity {
                kind: 99,
                ..activity(NetworkActivityKind::Dns, "10.0.0.2", 0)
            },
        ];
        let (dir, _, mut client) = log_workspace(Some(entries)).await;

        let output = activity_log(dir.path(), &mut client, &chrono::Utc)
            .await
            .unwrap();

        assert!(output.contains("│ 10.0.0.1    │ unknown │"), "{output}");
        assert!(output.contains("│ 10.0.0.2    │ unknown │"), "{output}");
        assert!(!output.contains("dns"), "{output}");
    }

    #[tokio::test]
    async fn log_removes_control_characters_from_destinations() {
        let entries = vec![activity(
            NetworkActivityKind::Dns,
            "evil\u{1b}[2J.example\u{7}.com",
            0,
        )];
        let (dir, _, mut client) = log_workspace(Some(entries)).await;

        let output = activity_log(dir.path(), &mut client, &chrono::Utc)
            .await
            .unwrap();

        assert!(
            !output.contains('\u{1b}') && !output.contains('\u{7}'),
            "{output:?}"
        );
        assert!(output.contains("evil.example.com"), "{output}");
    }

    #[tokio::test]
    async fn log_changes_neither_the_spec_file_nor_creates_one() {
        let entries = vec![activity(NetworkActivityKind::Dns, "example.com", 0)];
        let (dir, content, mut client) = log_workspace(Some(entries)).await;
        let empty = TempDir::new().unwrap();
        let mut other_client = fake_daemon::serve("other", OTHER_WORKSPACE).await;

        activity_log(dir.path(), &mut client, &chrono::Utc)
            .await
            .unwrap();
        let error = activity_log(empty.path(), &mut other_client, &chrono::Utc)
            .await
            .unwrap_err()
            .to_string();

        assert_eq!(
            fs::read_to_string(dir.path().join(SPEC_FILE_NAME)).unwrap(),
            content
        );
        // Without a spec file, it asks for the sandbox `fbk start` would create.
        assert!(
            error.starts_with("sandbox firebrick-") && error.ends_with(" doesn't exist"),
            "{error}"
        );
        assert_eq!(fs::read_dir(empty.path()).unwrap().count(), 0);
    }
}
