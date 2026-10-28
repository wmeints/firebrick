//! Approval of the `mounts`, `network` and `clipboard` settings in `.firebrick.yml`. The spec
//! file lives in the workspace, which the sandbox mounts read/write, so an agent can edit it.
//! `fbk` records the settings the user approved per workspace outside every mounted directory,
//! and asks before it applies other ones.

mod diff;
mod prompt;
mod store;

use anyhow::{Context, Result, anyhow};
use firebrick_spec::{MountSpec, NetworkSpec};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};
use std::io;
use std::path::{Path, PathBuf};

use prompt::Prompt;
use store::Approvals;

/// How a sandbox shares the host clipboard.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Clipboard {
    /// Not at all.
    #[default]
    Off,
    /// The sandbox can read the host clipboard. Approvals files from before copying existed
    /// store it as `clipboard: true`; `fbk` no longer requests it.
    Paste,
    /// The sandbox can read and write the host clipboard.
    Both,
}

impl Clipboard {
    /// Returns the mode of the `clipboard` setting in `.firebrick.yml`, which shares the
    /// clipboard both ways when it's on.
    pub fn shared(on: bool) -> Self {
        if on { Clipboard::Both } else { Clipboard::Off }
    }

    /// Whether the clipboard isn't shared.
    fn is_off(&self) -> bool {
        *self == Clipboard::Off
    }

    /// Returns the line that describes the mode, or `None` when it's off.
    fn line(self) -> Option<&'static str> {
        match self {
            Clipboard::Off => None,
            Clipboard::Paste => Some("clipboard: shared with the host, paste only"),
            Clipboard::Both => Some("clipboard: shared with the host, both ways"),
        }
    }
}

/// Reads the approved clipboard mode, or the boolean of approvals files from before copying
/// existed, where `true` means paste only.
fn clipboard_mode<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Clipboard, D::Error> {
    /// The forms the mode is stored in.
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Stored {
        Shared(bool),
        Mode(Clipboard),
    }

    let stored = Stored::deserialize(deserializer).map_err(|_| {
        D::Error::custom("invalid clipboard mode; expected off, paste, both, true or false")
    })?;

    Ok(match stored {
        Stored::Shared(true) => Clipboard::Paste,
        Stored::Shared(false) => Clipboard::Off,
        Stored::Mode(mode) => mode,
    })
}

/// The settings of a workspace that need approval.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// Extra mounts, with the canonical host path they resolve to.
    #[serde(default)]
    pub mounts: Vec<MountSpec>,
    /// The network section, or its default when the spec has none.
    #[serde(default)]
    pub network: NetworkSpec,
    /// How the sandbox shares the host clipboard. Left out of the file when it's off, so
    /// approvals files stay readable by `fbk` versions without the setting.
    #[serde(
        default,
        deserialize_with = "clipboard_mode",
        skip_serializing_if = "Clipboard::is_off"
    )]
    pub clipboard: Clipboard,
}

/// Gets approval on the terminal, with the approvals stored in a file.
#[derive(Debug, Clone)]
pub struct Approver {
    path: Option<PathBuf>,
    assume_yes: bool,
}

impl Approver {
    /// Returns an approver for the approvals in the user's config directory. `assume_yes`
    /// approves without asking.
    pub fn new(assume_yes: bool) -> Self {
        Self {
            path: firebrick_utils::approvals_path(),
            assume_yes,
        }
    }

    /// Returns an approver for the approvals in the file at `path`.
    #[cfg(test)]
    pub fn at(path: PathBuf, assume_yes: bool) -> Self {
        Self {
            path: Some(path),
            assume_yes,
        }
    }

    /// Runs `approve` with the approvals of the workspace and a prompt on the terminal, off
    /// the async runtime because reading the answer blocks.
    pub async fn run<F>(self, workspace: &Path, approve: F) -> Result<()>
    where
        F: FnOnce(&Approvals, &mut Prompt<io::StdinLock<'static>, io::Stderr>) -> Result<()>
            + Send
            + 'static,
    {
        let path = self
            .path
            .ok_or_else(|| anyhow!(firebrick_utils::NO_CONFIG_DIR))?;
        let workspace = workspace.to_path_buf();
        let assume_yes = self.assume_yes;

        tokio::task::spawn_blocking(move || {
            let approvals = Approvals::at(path, &workspace)?;
            approve(&approvals, &mut Prompt::terminal(assume_yes))
        })
        .await
        .context("failed to ask for approval of the mounts, network and clipboard settings")?
    }
}

#[cfg(test)]
mod tests {
    use super::diff::difference;
    use super::prompt::{ABORTED, Answer, NOT_INTERACTIVE};
    use super::*;
    use firebrick_spec::NetworkRule;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    /// A config dir and a workspace in temp dirs.
    struct Fixture {
        config: TempDir,
        workspace: TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                config: TempDir::new().unwrap(),
                workspace: TempDir::new().unwrap(),
            }
        }

        fn path(&self) -> PathBuf {
            self.config.path().join("firebrick/approved.yml")
        }

        fn approvals(&self) -> Approvals {
            Approvals::at(self.path(), self.workspace.path()).unwrap()
        }

        /// Returns the approvals of a workspace whose default settings were approved before.
        fn approvals_with_defaults(&self) -> Approvals {
            let approvals = self.approvals();
            approvals.record(Settings::default()).unwrap();
            approvals
        }
    }

    /// A prompt that answers with `answer` and keeps what it printed.
    fn prompt(answer: &str) -> Prompt<&[u8], Vec<u8>> {
        Prompt::new(Answer::Ask, answer.as_bytes(), Vec::new())
    }

    fn printed(prompt: &Prompt<&[u8], Vec<u8>>) -> String {
        String::from_utf8(prompt.output.clone()).unwrap()
    }

    fn rules(values: &[&str]) -> Vec<NetworkRule> {
        values.iter().map(|value| value.parse().unwrap()).collect()
    }

    fn mount(host: &str, guest: &str, readonly: bool) -> MountSpec {
        MountSpec {
            host: host.to_string(),
            guest: guest.to_string(),
            readonly,
        }
    }

    fn home_mount() -> Settings {
        Settings {
            mounts: vec![mount("/home/me", "/mnt/home", false)],
            ..Settings::default()
        }
    }

    fn network(network: NetworkSpec) -> Settings {
        Settings {
            network,
            ..Settings::default()
        }
    }

    #[test]
    fn workspace_without_an_entry_has_no_approved_settings() {
        let fixture = Fixture::new();

        assert_eq!(fixture.approvals().approved().unwrap(), None);
    }

    #[test]
    fn recorded_settings_are_approved_for_that_workspace_only() {
        let fixture = Fixture::new();
        let other = TempDir::new().unwrap();
        let approvals = fixture.approvals();

        approvals.record(home_mount()).unwrap();

        assert_eq!(approvals.approved().unwrap(), Some(home_mount()));
        assert_eq!(
            Approvals::at(fixture.path(), other.path())
                .unwrap()
                .approved()
                .unwrap(),
            None
        );
    }

    #[test]
    fn workspace_is_stored_under_its_canonical_path() {
        let fixture = Fixture::new();
        let link = fixture.config.path().join("link");
        if !crate::test_support::symlinked_spec::symlink_dir(fixture.workspace.path(), &link) {
            return;
        }

        Approvals::at(fixture.path(), &link)
            .unwrap()
            .record(home_mount())
            .unwrap();

        assert_eq!(fixture.approvals().approved().unwrap(), Some(home_mount()));
    }

    #[test]
    #[cfg(unix)]
    fn approvals_file_is_private() {
        let fixture = Fixture::new();

        fixture.approvals().record(home_mount()).unwrap();
        let mode = fs::metadata(fixture.path()).unwrap().permissions().mode();

        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn invalid_approvals_file_is_an_error_naming_it() {
        let fixture = Fixture::new();
        fs::create_dir_all(fixture.path().parent().unwrap()).unwrap();
        fs::write(fixture.path(), "workspaces: [not, a, map]\n").unwrap();

        let error = fixture.approvals().approved().unwrap_err();

        assert!(
            error
                .to_string()
                .contains(&fixture.path().display().to_string()),
            "{error}"
        );
    }

    #[test]
    fn unreadable_approvals_file_is_an_error_naming_it() {
        let fixture = Fixture::new();
        // A directory in place of the file can't be read, even by root.
        fs::create_dir_all(fixture.path()).unwrap();

        let error = fixture.approvals().approved().unwrap_err();

        assert_eq!(
            error.to_string(),
            format!("failed to read {}", fixture.path().display())
        );
    }

    #[test]
    fn difference_lists_added_and_removed_mounts() {
        let approved = Settings {
            mounts: vec![mount("/data", "/data", true)],
            ..Settings::default()
        };

        assert_eq!(
            difference(&approved, &home_mount()),
            [
                "- mount /data -> /data (read-only)",
                "+ mount /home/me -> /mnt/home (read/write)",
            ]
        );
    }

    #[test]
    fn difference_lists_changed_network_fields() {
        let approved = network(NetworkSpec {
            enforce: true,
            allow: rules(&["github.com"]),
            deny: rules(&["gist.github.com"]),
            ..NetworkSpec::default()
        });
        let requested = network(NetworkSpec {
            enabled: Some(false),
            enforce: false,
            allow: rules(&["github.com", "*.attacker.example"]),
            deny: vec![],
        });

        assert_eq!(
            difference(&approved, &requested),
            [
                "~ network.enabled: true -> false",
                "~ network.enforce: true -> false",
                "+ network.allow: *.attacker.example",
                "- network.deny: gist.github.com",
            ]
        );
    }

    #[test]
    fn difference_ignores_order_and_an_explicit_default_switch() {
        let approved = network(NetworkSpec {
            allow: rules(&["a.example", "b.example"]),
            ..NetworkSpec::default()
        });
        let requested = network(NetworkSpec {
            enabled: Some(true),
            allow: rules(&["b.example", "a.example"]),
            ..NetworkSpec::default()
        });

        assert!(difference(&approved, &requested).is_empty());
    }

    fn clipboard(mode: Clipboard) -> Settings {
        Settings {
            clipboard: mode,
            ..Settings::default()
        }
    }

    /// Writes an approvals file whose entry for the fixture's workspace has the fields.
    fn write_entry(fixture: &Fixture, fields: &str) {
        let workspace = dunce::canonicalize(fixture.workspace.path()).unwrap();
        fs::create_dir_all(fixture.path().parent().unwrap()).unwrap();
        let content = format!("workspaces:\n  {}:\n{fields}", workspace.display());
        fs::write(fixture.path(), content).unwrap();
    }

    #[test]
    fn the_clipboard_setting_shares_both_ways() {
        assert_eq!(Clipboard::shared(true), Clipboard::Both);
        assert_eq!(Clipboard::shared(false), Clipboard::Off);
    }

    #[test]
    fn difference_lists_a_changed_clipboard() {
        let both = "clipboard: shared with the host, both ways";

        assert_eq!(
            difference(&clipboard(Clipboard::Off), &clipboard(Clipboard::Both)),
            [format!("+ {both}")]
        );
        assert_eq!(
            difference(&clipboard(Clipboard::Both), &clipboard(Clipboard::Off)),
            [format!("- {both}")]
        );
        assert_eq!(
            difference(&clipboard(Clipboard::Paste), &clipboard(Clipboard::Both)),
            [
                "- clipboard: shared with the host, paste only".to_string(),
                format!("+ {both}")
            ]
        );
        assert!(difference(&clipboard(Clipboard::Both), &clipboard(Clipboard::Both)).is_empty());
    }

    #[test]
    fn sharing_the_clipboard_is_asked_about_and_recorded() {
        let fixture = Fixture::new();
        let approvals = fixture.approvals_with_defaults();
        let mut prompt = prompt("y\n");

        approvals
            .approve_sandbox(clipboard(Clipboard::Both), &mut prompt)
            .unwrap();

        assert_eq!(
            printed(&prompt),
            ".firebrick.yml changes settings you haven't approved for this workspace:\n  \
             + clipboard: shared with the host, both ways\nApply these changes? [y/N] "
        );
        assert_eq!(
            approvals.approved().unwrap(),
            Some(clipboard(Clipboard::Both))
        );
    }

    #[test]
    fn first_settings_list_a_shared_clipboard() {
        let fixture = Fixture::new();
        let mut prompt = prompt("n\n");

        let error = fixture
            .approvals()
            .approve_sandbox(clipboard(Clipboard::Both), &mut prompt)
            .unwrap_err();

        assert_eq!(error.to_string(), ABORTED);
        assert_eq!(
            printed(&prompt),
            ".firebrick.yml sets mounts, network or clipboard settings you haven't approved \
             for this workspace yet:\n  network.enabled: true\n  network.enforce: false\n  \
             + clipboard: shared with the host, both ways\nApply these changes? [y/N] "
        );
        assert_eq!(fixture.approvals().approved().unwrap(), None);
    }

    #[test]
    fn a_clipboard_approved_before_copying_existed_is_asked_about_again() {
        let fixture = Fixture::new();
        write_entry(&fixture, "    clipboard: true\n");
        let approvals = fixture.approvals();
        let mut prompt = prompt("y\n");

        approvals
            .approve_sandbox(clipboard(Clipboard::Both), &mut prompt)
            .unwrap();

        assert_eq!(
            printed(&prompt),
            ".firebrick.yml changes settings you haven't approved for this workspace:\n  \
             - clipboard: shared with the host, paste only\n  \
             + clipboard: shared with the host, both ways\nApply these changes? [y/N] "
        );
        assert_eq!(
            approvals.approved().unwrap(),
            Some(clipboard(Clipboard::Both))
        );
        let file = fs::read_to_string(fixture.path()).unwrap();
        assert!(file.contains("clipboard: both"), "{file}");
    }

    #[test]
    fn approvals_file_with_an_unknown_clipboard_mode_is_invalid() {
        let fixture = Fixture::new();
        write_entry(&fixture, "    clipboard: always\n");

        let error = fixture.approvals().approved().unwrap_err();

        assert!(
            format!("{error:#}")
                .contains("invalid clipboard mode; expected off, paste, both, true or false"),
            "{error:#}"
        );
    }

    #[test]
    fn approvals_file_with_each_clipboard_mode_loads() {
        let modes = [
            ("off", Clipboard::Off),
            ("false", Clipboard::Off),
            ("paste", Clipboard::Paste),
            ("both", Clipboard::Both),
        ];

        for (stored, mode) in modes {
            let fixture = Fixture::new();
            write_entry(&fixture, &format!("    clipboard: {stored}\n"));

            let approved = fixture.approvals().approved().unwrap().unwrap();

            assert_eq!(approved.clipboard, mode, "{stored}");
        }
    }

    #[test]
    fn approvals_file_without_the_clipboard_loads_with_it_off() {
        let fixture = Fixture::new();
        let workspace = dunce::canonicalize(fixture.workspace.path()).unwrap();
        fs::create_dir_all(fixture.path().parent().unwrap()).unwrap();
        let content = format!(
            "workspaces:\n  {}:\n    mounts: []\n    network:\n      enforce: true\n",
            workspace.display()
        );
        fs::write(fixture.path(), content).unwrap();

        let approved = fixture.approvals().approved().unwrap().unwrap();

        assert_eq!(approved.clipboard, Clipboard::Off);
        assert!(approved.network.enforce);
    }

    #[test]
    fn approvals_file_leaves_out_a_clipboard_that_is_off() {
        let fixture = Fixture::new();

        fixture.approvals().record(home_mount()).unwrap();
        let without = fs::read_to_string(fixture.path()).unwrap();
        fixture
            .approvals()
            .record(clipboard(Clipboard::Both))
            .unwrap();
        let with = fs::read_to_string(fixture.path()).unwrap();

        assert!(!without.contains("clipboard"), "{without}");
        assert!(with.contains("clipboard: both"), "{with}");
    }

    #[test]
    fn approved_settings_create_the_sandbox_without_asking() {
        let fixture = Fixture::new();
        let approvals = fixture.approvals();
        approvals.record(home_mount()).unwrap();
        let mut prompt = prompt("");

        approvals
            .approve_sandbox(home_mount(), &mut prompt)
            .unwrap();

        assert_eq!(printed(&prompt), "");
    }

    #[test]
    fn default_settings_need_no_entry() {
        let fixture = Fixture::new();
        let mut prompt = prompt("");

        fixture
            .approvals()
            .approve_sandbox(Settings::default(), &mut prompt)
            .unwrap();

        assert_eq!(printed(&prompt), "");
        assert!(!fixture.path().exists());
    }

    #[test]
    fn changes_are_shown_and_recorded_when_the_user_answers_yes() {
        let fixture = Fixture::new();
        let approvals = fixture.approvals_with_defaults();
        let mut prompt = prompt("y\n");

        approvals
            .approve_sandbox(home_mount(), &mut prompt)
            .unwrap();

        assert_eq!(
            printed(&prompt),
            ".firebrick.yml changes settings you haven't approved for this workspace:\n  \
             + mount /home/me -> /mnt/home (read/write)\nApply these changes? [y/N] "
        );
        assert_eq!(approvals.approved().unwrap(), Some(home_mount()));
    }

    #[test]
    fn yes_is_accepted_in_any_case() {
        for answer in ["yes\n", "Y\n", " YES \n"] {
            let fixture = Fixture::new();
            let approvals = fixture.approvals_with_defaults();

            approvals
                .approve_sandbox(home_mount(), &mut prompt(answer))
                .unwrap();

            assert_eq!(
                approvals.approved().unwrap(),
                Some(home_mount()),
                "{answer:?}"
            );
        }
    }

    #[test]
    fn other_answers_abort_without_recording() {
        for answer in ["n\n", "\n", "", "sure\n"] {
            let fixture = Fixture::new();
            let approvals = fixture.approvals_with_defaults();

            let error = approvals
                .approve_sandbox(home_mount(), &mut prompt(answer))
                .unwrap_err();

            assert_eq!(error.to_string(), ABORTED, "{answer:?}");
            assert_eq!(approvals.approved().unwrap(), Some(Settings::default()));
        }
    }

    #[test]
    fn without_a_terminal_the_changes_are_shown_and_refused() {
        let fixture = Fixture::new();
        let approvals = fixture.approvals_with_defaults();
        let mut prompt = Prompt::new(Answer::NoTerminal, "y\n".as_bytes(), Vec::new());

        let error = approvals
            .approve_sandbox(home_mount(), &mut prompt)
            .unwrap_err();

        assert_eq!(error.to_string(), NOT_INTERACTIVE);
        assert_eq!(
            printed(&prompt),
            ".firebrick.yml changes settings you haven't approved for this workspace:\n  \
             + mount /home/me -> /mnt/home (read/write)\n"
        );
        assert_eq!(approvals.approved().unwrap(), Some(Settings::default()));
    }

    #[test]
    fn assume_yes_shows_and_records_the_changes_without_asking() {
        let fixture = Fixture::new();
        let approvals = fixture.approvals_with_defaults();
        let mut prompt = Prompt::new(Answer::Yes, "".as_bytes(), Vec::new());

        approvals
            .approve_sandbox(home_mount(), &mut prompt)
            .unwrap();

        assert_eq!(
            printed(&prompt),
            ".firebrick.yml changes settings you haven't approved for this workspace:\n  \
             + mount /home/me -> /mnt/home (read/write)\n"
        );
        assert_eq!(approvals.approved().unwrap(), Some(home_mount()));
    }

    /// Adds `example.org` to the allowed rules, like `fbk network allow example.org`.
    fn allow_example(approved: &NetworkSpec) -> NetworkSpec {
        let mut network = approved.clone();
        network.allow(&rules(&["example.org"]));
        network
    }

    #[test]
    fn expected_network_change_is_recorded_without_asking() {
        let fixture = Fixture::new();
        let approvals = fixture.approvals();
        approvals.record(home_mount()).unwrap();
        let sent = allow_example(&NetworkSpec::default());
        let mut prompt = prompt("");

        approvals
            .approve_network(sent.clone(), allow_example, &mut prompt)
            .unwrap();

        assert_eq!(printed(&prompt), "");
        assert_eq!(
            approvals.approved().unwrap(),
            Some(Settings {
                network: sent,
                ..home_mount()
            })
        );
    }

    #[test]
    fn hand_edits_to_the_network_are_asked_about() {
        let fixture = Fixture::new();
        let approvals = fixture.approvals();
        let sent = NetworkSpec {
            enforce: false,
            ..allow_example(&NetworkSpec::default())
        };
        let enforced = network(NetworkSpec {
            enforce: true,
            ..NetworkSpec::default()
        });
        approvals.record(enforced.clone()).unwrap();
        let mut prompt = prompt("n\n");

        let error = approvals
            .approve_network(sent, allow_example, &mut prompt)
            .unwrap_err();

        assert_eq!(error.to_string(), ABORTED);
        assert_eq!(
            printed(&prompt),
            ".firebrick.yml changes settings you haven't approved for this workspace:\n  \
             ~ network.enforce: true -> false\n  + network.allow: example.org\n\
             Apply these changes? [y/N] "
        );
        assert_eq!(approvals.approved().unwrap(), Some(enforced));
    }

    #[test]
    fn first_settings_are_shown_in_full() {
        let fixture = Fixture::new();
        let approvals = fixture.approvals();
        let mut prompt = prompt("y\n");

        approvals
            .approve_sandbox(home_mount(), &mut prompt)
            .unwrap();

        assert_eq!(
            printed(&prompt),
            ".firebrick.yml sets mounts, network or clipboard settings you haven't approved \
             for this workspace yet:\n  + mount /home/me -> /mnt/home (read/write)\n  \
             network.enabled: true\n  network.enforce: false\nApply these changes? [y/N] "
        );
        assert_eq!(approvals.approved().unwrap(), Some(home_mount()));
    }

    #[test]
    fn first_network_change_shows_that_enforcement_is_off() {
        // A sandbox created before approvals existed may enforce rules that the agent removed
        // from the spec, so the first change shows the whole section it would send.
        let fixture = Fixture::new();
        let approvals = fixture.approvals();
        let mut prompt = prompt("n\n");

        let error = approvals
            .approve_network(
                allow_example(&NetworkSpec::default()),
                allow_example,
                &mut prompt,
            )
            .unwrap_err();

        assert_eq!(error.to_string(), ABORTED);
        assert_eq!(
            printed(&prompt),
            ".firebrick.yml sets mounts, network or clipboard settings you haven't approved \
             for this workspace yet:\n  network.enabled: true\n  network.enforce: false\n  \
             + network.allow: example.org\nApply these changes? [y/N] "
        );
        assert_eq!(approvals.approved().unwrap(), None);
    }
}
