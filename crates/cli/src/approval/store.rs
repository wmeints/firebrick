//! The approvals file: the settings the user approved per workspace, kept outside every
//! directory a sandbox mounts, with the workspace's canonical path as key.

use super::Settings;
use super::prompt::Prompt;
use anyhow::{Context, Result};
use firebrick_spec::NetworkSpec;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

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
    /// Returns the approvals of the workspace stored in the file at `path`. They're stored under
    /// the canonical workspace path, so a symlink to the workspace shares them.
    pub fn at(path: PathBuf, workspace: &Path) -> Result<Self> {
        let workspace = dunce::canonicalize(workspace)
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
