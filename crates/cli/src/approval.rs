//! Approval of the `mounts` and `network` settings in `.firebrick.yml`. The spec file lives in
//! the workspace, which the sandbox mounts read/write, so an agent can edit it. `fbk` records
//! the settings the user approved per workspace outside every mounted directory, and asks before
//! it applies other ones.

use anyhow::{Context, Result, anyhow, bail};
use firebrick_spec::{MountSpec, NetworkRule, NetworkSpec};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

/// Error for an answer other than yes.
const ABORTED: &str =
    "aborted: the changes to mounts and network in .firebrick.yml weren't approved";

/// Error when there is no terminal to ask on and `--yes` isn't passed.
const NOT_INTERACTIVE: &str = ".firebrick.yml changes mounts or network since you last approved \
                               them; run the command in a terminal to review the changes, or \
                               pass --yes";

/// Header above the changes to approve.
const CHANGES_HEADER: &str =
    ".firebrick.yml changes settings you haven't approved for this workspace:";

/// Header above the settings of a workspace without approved settings.
const FIRST_HEADER: &str = ".firebrick.yml sets mounts or network settings you haven't approved \
                            for this workspace yet:";

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
}

/// Layout of the approvals file.
#[derive(Serialize, Deserialize, Debug, Default)]
#[serde(deny_unknown_fields)]
struct ApprovalsFile {
    /// Approved settings by canonical workspace path.
    #[serde(default)]
    workspaces: BTreeMap<PathBuf, Settings>,
}

/// The approved settings of one workspace, stored in a file shared by all workspaces.
#[derive(Debug, Clone)]
pub struct Approvals {
    path: PathBuf,
    workspace: PathBuf,
}

impl Approvals {
    /// Returns the approvals of the workspace in the user's config directory.
    pub fn open(workspace: &Path) -> Result<Self> {
        let path = firebrick_utils::approvals_path().ok_or_else(|| {
            anyhow!(
                "can't find the firebrick config directory: set HOME or XDG_CONFIG_HOME to an \
                 absolute path"
            )
        })?;

        Self::at(path, workspace)
    }

    /// Returns the approvals of the workspace stored in the file at `path`. They're stored under
    /// the canonical workspace path, so a symlink to the workspace shares them.
    pub fn at(path: PathBuf, workspace: &Path) -> Result<Self> {
        let workspace = fs::canonicalize(workspace)
            .with_context(|| format!("failed to resolve workspace {}", workspace.display()))?;

        Ok(Self { path, workspace })
    }

    /// Returns the approved settings, or `None` when the workspace has none yet. Fails when the
    /// file can't be read or isn't valid.
    pub fn approved(&self) -> Result<Option<Settings>> {
        Ok(self.load()?.workspaces.remove(&self.workspace))
    }

    /// Records the settings as approved.
    pub fn record(&self, settings: Settings) -> Result<()> {
        let mut file = self.load()?;
        file.workspaces.insert(self.workspace.clone(), settings);

        self.save(&file)
            .with_context(|| format!("failed to write {}", self.path.display()))
    }

    /// Makes sure the user approved the settings a new sandbox gets. Asks when they differ from
    /// the approved ones, and records them once approved. A workspace without approved settings
    /// gets no mounts and the default network section without asking; other settings are shown
    /// in full, because the sandbox they replace may have had stricter ones.
    pub fn approve_sandbox<R: BufRead, W: Write>(
        &self,
        requested: Settings,
        prompt: &mut Prompt<R, W>,
    ) -> Result<()> {
        let approved = self.approved()?;
        prompt.review(approved.as_ref(), approved.as_ref(), &requested)?;

        self.record_changed(approved.as_ref(), requested)
    }

    /// Makes sure the user approved the network section sent to a sandbox. `expected` turns the
    /// approved section into the one the user asked for; when the sent section differs from it,
    /// asks about the difference with the approved one. Records the sent section once approved,
    /// keeping the approved mounts. A workspace without approved settings is asked about any
    /// section but the default, like [`Approvals::approve_sandbox`].
    pub fn approve_network<R: BufRead, W: Write>(
        &self,
        sent: NetworkSpec,
        expected: impl FnOnce(&NetworkSpec) -> NetworkSpec,
        prompt: &mut Prompt<R, W>,
    ) -> Result<()> {
        let approved = self.approved()?;
        let base = approved.clone().unwrap_or_default();
        let expected = Settings {
            network: expected(&base.network),
            ..base.clone()
        };
        let requested = Settings {
            network: sent,
            ..base
        };
        prompt.review(approved.as_ref(), Some(&expected), &requested)?;

        self.record_changed(approved.as_ref(), requested)
    }

    /// Records the requested settings when they differ from the approved ones. Default settings
    /// need no entry.
    fn record_changed(&self, approved: Option<&Settings>, requested: Settings) -> Result<()> {
        if approved.cloned().unwrap_or_default() == requested {
            return Ok(());
        }

        self.record(requested)
    }

    /// Reads the file, or returns no approvals when it doesn't exist.
    fn load(&self) -> Result<ApprovalsFile> {
        let content = match fs::read_to_string(&self.path) {
            Ok(content) => content,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Ok(ApprovalsFile::default());
            }
            Err(err) => {
                return Err(err).with_context(|| format!("failed to read {}", self.path.display()));
            }
        };

        serde_yaml::from_str(&content)
            .with_context(|| format!("{} isn't a valid approvals file", self.path.display()))
    }

    /// Writes the file with mode `0600` through a temp file and a rename, so a failed write
    /// leaves the old approvals in place.
    fn save(&self, file: &ApprovalsFile) -> Result<()> {
        firebrick_utils::write_private(&self.path, &serde_yaml::to_string(file)?)?;

        Ok(())
    }
}

/// Runs `approve` with the approvals of the workspace and a prompt on the terminal, off the
/// async runtime because reading the answer blocks. `assume_yes` approves without asking.
pub async fn on_terminal<F>(workspace: &Path, assume_yes: bool, approve: F) -> Result<()>
where
    F: FnOnce(&Approvals, &mut Prompt<io::StdinLock<'static>, io::Stderr>) -> Result<()>
        + Send
        + 'static,
{
    let workspace = workspace.to_path_buf();

    tokio::task::spawn_blocking(move || {
        let approvals = Approvals::open(&workspace)?;
        approve(&approvals, &mut Prompt::terminal(assume_yes))
    })
    .await
    .context("failed to ask for approval of the mounts and network settings")?
}

/// Returns a line per added or removed mount and per changed network field between the
/// approved and requested settings. Mounts and rules are compared as sets, so their order
/// doesn't count as a change.
pub fn difference(approved: &Settings, requested: &Settings) -> Vec<String> {
    let mut lines = list_difference(&approved.mounts, &requested.mounts, mount_line);
    lines.extend(network_difference(&approved.network, &requested.network));

    lines
}

/// Returns the lines for the network fields that differ.
fn network_difference(approved: &NetworkSpec, requested: &NetworkSpec) -> Vec<String> {
    let switches = [
        switch_difference("enabled", approved, requested, NetworkSpec::is_enabled),
        switch_difference("enforce", approved, requested, |network| network.enforce),
    ];
    let mut lines: Vec<String> = switches.into_iter().flatten().collect();

    lines.extend(list_difference(
        &approved.allow,
        &requested.allow,
        |sign, rule| rule_line(sign, "allow", rule),
    ));
    lines.extend(list_difference(
        &approved.deny,
        &requested.deny,
        |sign, rule| rule_line(sign, "deny", rule),
    ));

    lines
}

/// Returns a `-` line per item only in `approved`, then a `+` line per item only in
/// `requested`.
fn list_difference<T: PartialEq>(
    approved: &[T],
    requested: &[T],
    line: impl Fn(char, &T) -> String,
) -> Vec<String> {
    let removed = approved.iter().filter(|item| !requested.contains(item));
    let added = requested.iter().filter(|item| !approved.contains(item));

    removed
        .map(|item| line('-', item))
        .chain(added.map(|item| line('+', item)))
        .collect()
}

/// Describes an added or removed mount.
fn mount_line(sign: char, mount: &MountSpec) -> String {
    let access = if mount.readonly {
        "read-only"
    } else {
        "read/write"
    };

    format!("{sign} mount {} -> {} ({access})", mount.host, mount.guest)
}

/// Returns a line per mount and network field of settings that have nothing to compare with.
fn listing(settings: &Settings) -> Vec<String> {
    let network = &settings.network;
    let mut lines = list_difference(&[], &settings.mounts, mount_line);
    lines.push(format!("network.enabled: {}", network.is_enabled()));
    lines.push(format!("network.enforce: {}", network.enforce));
    lines.extend(list_difference(&[], &network.allow, |sign, rule| {
        rule_line(sign, "allow", rule)
    }));
    lines.extend(list_difference(&[], &network.deny, |sign, rule| {
        rule_line(sign, "deny", rule)
    }));

    lines
}

/// Describes an added or removed network rule.
fn rule_line(sign: char, list: &str, rule: &NetworkRule) -> String {
    format!("{sign} network.{list}: {rule}")
}

/// Describes a network switch whose value differs, or returns `None` when it's the same.
fn switch_difference(
    field: &str,
    approved: &NetworkSpec,
    requested: &NetworkSpec,
    value: impl Fn(&NetworkSpec) -> bool,
) -> Option<String> {
    let (from, to) = (value(approved), value(requested));

    (from != to).then(|| format!("~ network.{field}: {from} -> {to}"))
}

/// How a prompt gets its answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// Approve the changes without asking, for `--yes`.
    Yes,
    /// Ask the user.
    Ask,
    /// Nobody can answer, because stdin isn't a terminal.
    NoTerminal,
}

/// Asks the user to approve changes.
pub struct Prompt<R, W> {
    answer: Answer,
    input: R,
    output: W,
}

impl Prompt<io::StdinLock<'static>, io::Stderr> {
    /// Returns a prompt that asks on the terminal, or approves without asking with
    /// `assume_yes`.
    pub fn terminal(assume_yes: bool) -> Self {
        let stdin = io::stdin();
        let answer = match (assume_yes, stdin.is_terminal()) {
            (true, _) => Answer::Yes,
            (false, true) => Answer::Ask,
            (false, false) => Answer::NoTerminal,
        };

        Self::new(answer, stdin.lock(), io::stderr())
    }
}

impl<R: BufRead, W: Write> Prompt<R, W> {
    /// Returns a prompt that reads the answer from `input` and writes to `output`.
    pub fn new(answer: Answer, input: R, output: W) -> Self {
        Self {
            answer,
            input,
            output,
        }
    }

    /// Asks about the difference between the approved and requested settings when the
    /// requested ones differ from the expected ones. Settings equal to the approved ones never
    /// need asking. Without approved settings, asks about all requested settings unless they're
    /// the defaults.
    fn review(
        &mut self,
        approved: Option<&Settings>,
        expected: Option<&Settings>,
        requested: &Settings,
    ) -> Result<()> {
        let Some(approved) = approved else {
            return self.review_first(requested);
        };

        let changes = difference(approved, requested);
        let expected = expected.unwrap_or(approved);

        if changes.is_empty() || difference(expected, requested).is_empty() {
            return Ok(());
        }

        self.confirm(CHANGES_HEADER, &changes)
    }

    /// Asks about all requested settings of a workspace without approved ones, unless they're
    /// the defaults.
    fn review_first(&mut self, requested: &Settings) -> Result<()> {
        if difference(&Settings::default(), requested).is_empty() {
            return Ok(());
        }

        self.confirm(FIRST_HEADER, &listing(requested))
    }

    /// Prints the lines under the header and asks to apply them, unless the answer is `Yes`.
    /// Fails when the user doesn't answer yes, or nobody can answer.
    fn confirm(&mut self, header: &str, lines: &[String]) -> Result<()> {
        self.show(header, lines)
            .context("failed to show the settings to approve")?;

        match self.answer {
            Answer::Yes => return Ok(()),
            Answer::NoTerminal => bail!(NOT_INTERACTIVE),
            Answer::Ask => {}
        }

        let answer = self
            .ask()
            .context("failed to read the answer to the approval prompt")?;

        match answer.trim().to_lowercase().as_str() {
            "y" | "yes" => Ok(()),
            _ => bail!(ABORTED),
        }
    }

    /// Writes the header and an indented line per setting.
    fn show(&mut self, header: &str, lines: &[String]) -> io::Result<()> {
        writeln!(self.output, "{header}")?;
        for line in lines {
            writeln!(self.output, "  {line}")?;
        }

        Ok(())
    }

    /// Asks to apply the changes and returns the answer.
    fn ask(&mut self) -> io::Result<String> {
        write!(self.output, "Apply these changes? [y/N] ")?;
        self.output.flush()?;

        let mut answer = String::new();
        self.input.read_line(&mut answer)?;

        Ok(answer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        std::os::unix::fs::symlink(fixture.workspace.path(), &link).unwrap();

        Approvals::at(fixture.path(), &link)
            .unwrap()
            .record(home_mount())
            .unwrap();

        assert_eq!(fixture.approvals().approved().unwrap(), Some(home_mount()));
    }

    #[test]
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
            ".firebrick.yml sets mounts or network settings you haven't approved for this \
             workspace yet:\n  + mount /home/me -> /mnt/home (read/write)\n  \
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
            ".firebrick.yml sets mounts or network settings you haven't approved for this \
             workspace yet:\n  network.enabled: true\n  network.enforce: false\n  \
             + network.allow: example.org\nApply these changes? [y/N] "
        );
        assert_eq!(approvals.approved().unwrap(), None);
    }
}
