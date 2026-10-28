//! Zed remote projects for sandboxes.
//!
//! Zed's remote development connects with the system `ssh`, so the sandbox hosts in the SSH
//! config already work. The daemon keeps one `ssh_connections` entry per sandbox in Zed's user
//! settings, with the sandbox's workspace as its project, so the user can pick the sandbox in
//! Remote Projects. The daemon owns the entries whose `host` ends in `.fbk`, and leaves the
//! rest of the file, comments and formatting included, as the user wrote it.

use super::settings_file::{self, PARSE_OPTIONS};
use jsonc_parser::cst::{CstArray, CstInputValue, CstObject, CstRootNode};
use jsonc_parser::parse_to_value;
use std::io;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Errors of syncing Zed's settings file. The file is left unchanged.
#[derive(Error, Debug)]
pub enum SettingsError {
    /// The settings file couldn't be read, or is a symlink to a file that doesn't exist.
    #[error("failed to read {}", path.display())]
    Read {
        /// The settings file.
        path: PathBuf,
        /// The I/O error.
        source: io::Error,
    },
    /// The settings file isn't valid JSONC, or its root isn't an object.
    #[error("failed to parse {}: {message}", path.display())]
    Parse {
        /// The settings file.
        path: PathBuf,
        /// What's wrong with the file.
        message: String,
    },
    /// `ssh_connections` exists, but isn't an array.
    #[error("{CONNECTIONS_KEY} in {} isn't an array", path.display())]
    ConnectionsNotAnArray {
        /// The settings file.
        path: PathBuf,
    },
    /// An entry of `ssh_connections` isn't an object.
    #[error("{CONNECTIONS_KEY} in {} has an entry that isn't an object", path.display())]
    ConnectionNotAnObject {
        /// The settings file.
        path: PathBuf,
    },
    /// `ssh_connections` appears more than once, so editing one could leave the one Zed reads
    /// unchanged.
    #[error("{CONNECTIONS_KEY} appears more than once in {}", path.display())]
    DuplicateConnectionsKey {
        /// The settings file.
        path: PathBuf,
    },
    /// The settings file couldn't be written.
    #[error("failed to write {}", path.display())]
    Write {
        /// The settings file.
        path: PathBuf,
        /// The I/O error.
        source: io::Error,
    },
}

/// A sandbox as a Zed remote project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteProject {
    /// SSH host name of the sandbox, e.g. `project.fbk`.
    pub host: String,
    /// Name Zed shows for the connection: the sandbox name.
    pub nickname: String,
    /// Guest path of the sandbox's workspace, e.g. `/workspaces/project`, if it has one.
    pub workspace_path: Option<String>,
}

const CONNECTIONS_KEY: &str = "ssh_connections";
const HOST_KEY: &str = "host";
const HOST_SUFFIX: &str = ".fbk";
const ZED_DIR: &str = "zed";
const SETTINGS_FILE: &str = "settings.json";

/// Returns the directory that holds Zed's `zed` config directory: `$FIREBRICK_EDITOR_CONFIG_ROOT`
/// when it's set, or the directory Zed uses on the platform otherwise.
pub fn config_root() -> Option<PathBuf> {
    settings_file::config_root_or(platform_config_root)
}

/// Returns `$XDG_CONFIG_HOME`, or `~/.config` when it isn't set.
#[cfg(all(unix, not(target_os = "macos")))]
fn platform_config_root() -> Option<PathBuf> {
    settings_file::xdg_config_home()
}

/// Returns `%APPDATA%`, which holds Zed's `Zed` directory on Windows. Paths there aren't case
/// sensitive, so [`ZED_DIR`] finds it.
#[cfg(windows)]
fn platform_config_root() -> Option<PathBuf> {
    settings_file::app_data()
}

/// Returns `~/.config`, which Zed uses on macOS regardless of `$XDG_CONFIG_HOME`.
#[cfg(target_os = "macos")]
fn platform_config_root() -> Option<PathBuf> {
    settings_file::home_config()
}

/// Writes one `ssh_connections` entry per project into `<config_root>/zed/settings.json`,
/// rewrites the `*.fbk` entries that differ and removes the stale ones. Does nothing when Zed
/// isn't installed, that is when the `zed` directory doesn't exist.
pub fn sync_settings(config_root: &Path, projects: &[RemoteProject]) -> Result<(), SettingsError> {
    let zed_dir = config_root.join(ZED_DIR);

    if !zed_dir.is_dir() {
        return Ok(());
    }

    let _guard = settings_file::lock();
    let path = zed_dir.join(SETTINGS_FILE);
    let text = settings_file::read(&path).map_err(|source| SettingsError::Read {
        path: path.clone(),
        source,
    })?;

    match update_connections(&path, &text, projects)? {
        Some(updated) => {
            settings_file::write(&path, &updated).map_err(|source| SettingsError::Write {
                path: path.clone(),
                source,
            })
        }
        None => Ok(()),
    }
}

/// Returns the settings text with the projects synced into `ssh_connections`, or `None` when
/// nothing changes.
fn update_connections(
    path: &Path,
    text: &str,
    projects: &[RemoteProject],
) -> Result<Option<String>, SettingsError> {
    let root = CstRootNode::parse(text, &PARSE_OPTIONS).map_err(|err| SettingsError::Parse {
        path: path.to_path_buf(),
        message: err.to_string(),
    })?;

    let Some(connections) = connections_array(&root, path, projects)? else {
        return Ok(None);
    };
    let entries = connection_objects(&connections, path)?;

    let mut missing: Vec<&RemoteProject> = projects.iter().collect();
    missing.sort_by(|a, b| a.host.cmp(&b.host));

    for entry in entries {
        sync_entry(entry, &mut missing);
    }

    for project in missing {
        connections.append(connection_value(project));
    }

    let updated = root.to_string();
    Ok((updated != text).then_some(updated))
}

/// Returns the `ssh_connections` array, creating it when there are projects to add, or `None`
/// when it doesn't exist and there's nothing to add.
fn connections_array(
    root: &CstRootNode,
    path: &Path,
    projects: &[RemoteProject],
) -> Result<Option<CstArray>, SettingsError> {
    let settings = match root.value() {
        None if projects.is_empty() => return Ok(None),
        None => root.object_value_or_set(),
        Some(value) => value.as_object().ok_or_else(|| SettingsError::Parse {
            path: path.to_path_buf(),
            message: "the settings aren't an object".to_string(),
        })?,
    };

    if settings_file::key_count(&settings, CONNECTIONS_KEY) > 1 {
        return Err(SettingsError::DuplicateConnectionsKey {
            path: path.to_path_buf(),
        });
    }

    match settings.get(CONNECTIONS_KEY) {
        None if projects.is_empty() => Ok(None),
        None => Ok(Some(settings.array_value_or_set(CONNECTIONS_KEY))),
        Some(prop) => {
            prop.array_value()
                .map(Some)
                .ok_or_else(|| SettingsError::ConnectionsNotAnArray {
                    path: path.to_path_buf(),
                })
        }
    }
}

/// Returns the entries of `ssh_connections`, or an error when one of them isn't an object.
fn connection_objects(
    connections: &CstArray,
    path: &Path,
) -> Result<Vec<CstObject>, SettingsError> {
    connections
        .elements()
        .iter()
        .map(|element| {
            element
                .as_object()
                .ok_or_else(|| SettingsError::ConnectionNotAnObject {
                    path: path.to_path_buf(),
                })
        })
        .collect()
}

/// Syncs one entry: keeps other hosts, rewrites a firebrick host that is still `missing` when it
/// differs, and removes the stale and duplicate firebrick hosts.
fn sync_entry(entry: CstObject, missing: &mut Vec<&RemoteProject>) {
    let Some(host) = entry_host(&entry).filter(|host| host.ends_with(HOST_SUFFIX)) else {
        return;
    };

    let Some(index) = missing.iter().position(|project| project.host == host) else {
        entry.remove();
        return;
    };

    let project = missing.remove(index);

    if !same_json(&entry.to_string(), &connection_text(project)) {
        entry.replace_with(connection_value(project));
    }
}

/// Returns the entry's `host`, when it's a string.
fn entry_host(entry: &CstObject) -> Option<String> {
    entry
        .get(HOST_KEY)?
        .value()?
        .as_string_lit()?
        .decoded_value()
        .ok()
}

/// Returns the `ssh_connections` entry of a project.
fn connection_value(project: &RemoteProject) -> CstInputValue {
    let projects = match &project.workspace_path {
        Some(workspace_path) => vec![CstInputValue::Object(vec![(
            "paths".to_string(),
            vec![workspace_path.as_str()].into(),
        )])],
        None => Vec::new(),
    };

    CstInputValue::Object(vec![
        (HOST_KEY.to_string(), project.host.as_str().into()),
        ("nickname".to_string(), project.nickname.as_str().into()),
        ("projects".to_string(), CstInputValue::Array(projects)),
    ])
}

/// Returns the `ssh_connections` entry of a project as JSON text.
fn connection_text(project: &RemoteProject) -> String {
    let root = CstRootNode::parse("", &PARSE_OPTIONS);

    root.map(|root| {
        root.set_value(connection_value(project));
        root.to_string()
    })
    .unwrap_or_default()
}

/// Returns whether two JSONC texts hold the same value, ignoring formatting and comments.
fn same_json(a: &str, b: &str) -> bool {
    match (
        parse_to_value(a, &PARSE_OPTIONS),
        parse_to_value(b, &PARSE_OPTIONS),
    ) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn project(leaf: &str) -> RemoteProject {
        RemoteProject {
            host: format!("{leaf}.fbk"),
            nickname: leaf.to_string(),
            workspace_path: Some(format!("/workspaces/{leaf}")),
        }
    }

    /// Returns a config root with a `zed` directory, and the path of its settings.
    fn zed_root() -> (TempDir, PathBuf) {
        let root = TempDir::new().unwrap();
        fs::create_dir_all(root.path().join("zed")).unwrap();
        let settings = root.path().join("zed/settings.json");

        (root, settings)
    }

    #[test]
    fn adds_projects_and_keeps_comments_other_hosts_and_keys() {
        let (root, settings) = zed_root();
        fs::write(
            &settings,
            "{\n  // my own comment stays\n  \"ui_font_size\": 16,\n  \"ssh_connections\": [\n    \
             { \"host\": \"myserver\", \"projects\": [{ \"paths\": [\"~/code\"] }] },\n  ],\n}\n",
        )
        .unwrap();

        sync_settings(root.path(), &[project("project")]).unwrap();

        let content = fs::read_to_string(&settings).unwrap();
        assert!(
            content.starts_with(
                "{\n  // my own comment stays\n  \"ui_font_size\": 16,\n  \
                 \"ssh_connections\": [\n    \
                 { \"host\": \"myserver\", \"projects\": [{ \"paths\": [\"~/code\"] }] },\n"
            ),
            "{content}"
        );
        assert!(content.ends_with("  ],\n}\n"), "{content}");
        assert!(same_json(
            &content,
            "{\"ui_font_size\": 16, \"ssh_connections\": [\
             {\"host\": \"myserver\", \"projects\": [{\"paths\": [\"~/code\"]}]},\
             {\"host\": \"project.fbk\", \"nickname\": \"project\", \
             \"projects\": [{\"paths\": [\"/workspaces/project\"]}]}]}"
        ));
    }

    #[test]
    fn rewrites_changed_projects_and_removes_stale_ones() {
        let (root, settings) = zed_root();
        fs::write(
            &settings,
            "{\n  \"ssh_connections\": [\n    { \"host\": \"old.fbk\", \"projects\": [] },\n    \
             { \"host\": \"myserver\", \"projects\": [] },\n    \
             { \"host\": \"web.fbk\", \"nickname\": \"stale\", \"projects\": [] }\n  ]\n}\n",
        )
        .unwrap();

        sync_settings(root.path(), &[project("web")]).unwrap();

        let content = fs::read_to_string(&settings).unwrap();
        assert!(same_json(
            &content,
            "{\"ssh_connections\": [{\"host\": \"myserver\", \"projects\": []},\
             {\"host\": \"web.fbk\", \"nickname\": \"web\", \
             \"projects\": [{\"paths\": [\"/workspaces/web\"]}]}]}"
        ));
    }

    #[test]
    fn removes_duplicate_firebrick_entries() {
        let (root, settings) = zed_root();
        let entry = "{\"host\": \"web.fbk\", \"nickname\": \"web\", \
                     \"projects\": [{\"paths\": [\"/workspaces/web\"]}]}";
        fs::write(
            &settings,
            format!("{{\"ssh_connections\": [{entry}, {entry}]}}"),
        )
        .unwrap();

        sync_settings(root.path(), &[project("web")]).unwrap();

        let content = fs::read_to_string(&settings).unwrap();
        assert!(same_json(
            &content,
            &format!("{{\"ssh_connections\": [{entry}]}}")
        ));
    }

    #[test]
    fn writes_empty_projects_without_workspace_path() {
        let (root, settings) = zed_root();
        let project = RemoteProject {
            workspace_path: None,
            ..project("named")
        };

        sync_settings(root.path(), &[project]).unwrap();

        let content = fs::read_to_string(&settings).unwrap();
        assert!(same_json(
            &content,
            "{\"ssh_connections\": [{\"host\": \"named.fbk\", \"nickname\": \"named\", \
             \"projects\": []}]}"
        ));
    }

    #[test]
    fn leaves_up_to_date_file_untouched() {
        let (root, settings) = zed_root();
        let content = "{\"ssh_connections\": [{\n  // mine\n  \"nickname\": \"project\",\n  \
                       \"host\": \"project.fbk\", \
                       \"projects\": [{\"paths\": [\"/workspaces/project\"]}]}]}";
        fs::write(&settings, content).unwrap();

        sync_settings(root.path(), &[project("project")]).unwrap();

        assert_eq!(fs::read_to_string(&settings).unwrap(), content);
    }

    #[test]
    fn creates_missing_settings_file() {
        let (root, settings) = zed_root();

        sync_settings(root.path(), &[project("project")]).unwrap();

        let content = fs::read_to_string(&settings).unwrap();
        assert!(same_json(
            &content,
            "{\"ssh_connections\": [{\"host\": \"project.fbk\", \"nickname\": \"project\", \
             \"projects\": [{\"paths\": [\"/workspaces/project\"]}]}]}"
        ));
    }

    #[test]
    fn leaves_missing_settings_file_alone_without_projects() {
        let (root, settings) = zed_root();

        sync_settings(root.path(), &[]).unwrap();

        assert!(!settings.exists());
    }

    #[test]
    fn skips_zed_when_it_is_not_installed() {
        let root = TempDir::new().unwrap();

        sync_settings(root.path(), &[project("project")]).unwrap();

        assert!(!root.path().join("zed").exists());
    }

    #[test]
    fn leaves_invalid_file_unchanged() {
        let (root, settings) = zed_root();
        let invalid = "{ \"ui_font_size\": 16, oops";
        fs::write(&settings, invalid).unwrap();

        let result = sync_settings(root.path(), &[project("project")]);

        assert!(
            matches!(result, Err(SettingsError::Parse { ref path, .. }) if *path == settings),
            "{result:?}"
        );
        assert_eq!(fs::read_to_string(&settings).unwrap(), invalid);
    }

    #[test]
    fn leaves_file_with_malformed_connections_unchanged() {
        let (root, settings) = zed_root();
        for content in [
            "{ \"ssh_connections\": {} }\n",
            "{ \"ssh_connections\": [{ \"host\": \"old.fbk\" }, \"myserver\"] }\n",
            "{ \"ssh_connections\": [], \"ssh_connections\": [] }\n",
        ] {
            fs::write(&settings, content).unwrap();

            let result = sync_settings(root.path(), &[project("project")]);

            assert!(
                matches!(
                    result,
                    Err(SettingsError::ConnectionsNotAnArray { .. }
                        | SettingsError::ConnectionNotAnObject { .. }
                        | SettingsError::DuplicateConnectionsKey { .. })
                ),
                "{content}: {result:?}"
            );
            assert_eq!(fs::read_to_string(&settings).unwrap(), content);
        }
    }
}
