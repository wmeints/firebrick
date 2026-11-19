//! Runs `fbk cert` plans made of shell commands, standing in for the trust store tools, to test
//! how `execute` removes copies, tells a missing certificate from a failure and keeps going after
//! a store fails.
#![cfg(unix)]

use firebrick_cli::cert::{Action, Command, Plan, Step, StorePlan, execute};
use std::path::Path;

/// Returns a command that runs the script with `sh`.
fn sh(script: &str) -> Command {
    Command {
        program: "sh".to_string(),
        args: vec!["-c".into(), script.into()],
    }
}

/// Returns a plan with one store per step.
fn plan(steps: Vec<(&str, Step)>) -> Plan {
    Plan {
        stores: steps
            .into_iter()
            .map(|(name, step)| StorePlan {
                name: name.to_string(),
                steps: vec![step],
            })
            .collect(),
        notes: vec![],
    }
}

/// Returns a step that removes `copy` while it exists, the way a store tool reports a missing
/// certificate when it doesn't.
fn remove_while_present(copy: &Path) -> Step {
    let copy = copy.display();

    Step::RemoveAll {
        present: sh(&format!(
            "test -e {copy} || {{ echo 'not found' >&2; exit 1; }}"
        )),
        remove: sh(&format!("rm {copy}")),
        absent: "not found",
    }
}

#[test]
fn remove_all_removes_copies_until_the_store_reports_none() {
    let dir = tempfile::tempdir().unwrap();
    let copy = dir.path().join("copy");
    std::fs::write(&copy, "").unwrap();

    execute(
        &plan(vec![("store", remove_while_present(&copy))]),
        &Action::Clean,
    )
    .unwrap();

    assert!(!copy.exists());
}

#[test]
fn remove_all_fails_when_the_store_cant_be_read() {
    let step = Step::RemoveAll {
        present: sh("echo 'bad database' >&2; exit 255"),
        remove: sh("exit 0"),
        absent: "not found",
    };

    let err = execute(&plan(vec![("store", step)]), &Action::Clean).unwrap_err();

    assert!(err.to_string().contains("bad database"), "{err:#}");
}

#[test]
fn remove_all_gives_up_when_removing_leaves_the_certificate() {
    let step = Step::RemoveAll {
        present: sh("exit 0"),
        remove: sh("exit 0"),
        absent: "not found",
    };

    let err = execute(&plan(vec![("store", step)]), &Action::Clean).unwrap_err();

    assert!(
        err.to_string().contains("didn't remove the certificate"),
        "{err:#}"
    );
}

#[test]
fn failing_store_doesnt_stop_the_others_and_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let trusted = dir.path().join("trusted");
    let steps = vec![
        (
            "the system store",
            Step::Run(sh("echo 'sudo refused' >&2; exit 1")),
        ),
        (
            "the NSS database",
            Step::Run(sh(&format!("touch {}", trusted.display()))),
        ),
    ];

    let err = execute(&plan(steps), &Action::Trust(dir.path().join("ca.crt"))).unwrap_err();

    assert!(trusted.exists());
    let message = err.to_string();
    assert!(message.contains("the system store"), "{message}");
    assert!(message.contains("sudo refused"), "{message}");
    assert!(!message.contains("the NSS database"), "{message}");
}
