//! Reads the git identity of a workspace with the host's `git`, in a process of its own because
//! it spawns host processes.

use firebrick_cli::api::GitIdentity;
use firebrick_cli::git_identity::host_identity;
use std::path::Path;
use std::process::Command;

/// Runs git with the arguments in the directory and fails the test when it fails.
fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .status()
        .unwrap();

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
