//! Reads the git identity of a workspace with the host's `git`, in a process of its own because
//! it spawns host processes.

use firebrick_cli::api::GitIdentity;
use firebrick_cli::git::{REPOSITORY_VARS, host_identity, read_identity};
use std::path::Path;
use std::process::Command;

/// Runs git with the arguments in the directory and fails the test when it fails. It ignores
/// the repository a git hook that runs the tests points at, so it only changes `dir`.
fn git(dir: &Path, args: &[&str]) {
    let mut command = Command::new("git");
    for var in REPOSITORY_VARS {
        command.env_remove(var);
    }
    let status = command.arg("-C").arg(dir).args(args).status().unwrap();

    assert!(status.success(), "git {args:?} failed");
}

#[tokio::test]
async fn reads_the_repository_local_identity() {
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "--quiet"]);
    git(repo.path(), &["config", "user.name", "O'Brien; $(id)"]);
    git(repo.path(), &["config", "user.email", "obrien@example.com"]);

    let identity = host_identity(repo.path()).await;

    assert_eq!(
        identity,
        Some(GitIdentity {
            name: Some("O'Brien; $(id)".to_string()),
            email: Some("obrien@example.com".to_string()),
        })
    );
}

#[tokio::test]
async fn broken_repository_config_gives_no_identity() {
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "--quiet"]);
    git(repo.path(), &["config", "user.name", "Ada Lovelace"]);
    let config = repo.path().join(".git").join("config");
    let text = std::fs::read_to_string(&config).unwrap();
    std::fs::write(&config, format!("{text}[broken\n")).unwrap();

    assert_eq!(host_identity(repo.path()).await, None);
}

#[tokio::test]
async fn missing_git_gives_no_identity() {
    let dir = tempfile::tempdir().unwrap();
    let git = dir.path().join("no-such-git");

    assert_eq!(read_identity(git.as_os_str(), dir.path()).await, None);
}
