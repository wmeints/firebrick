//! `fbk network log`: shows the network activity fbkd recorded for the working directory's
//! sandbox, with whether its current rules allow each destination.

use anyhow::{Result, anyhow, bail};
use chrono::{DateTime, Local, TimeZone};
use std::fmt::Display;
use std::path::Path;
use tonic::Code;

use super::Client;
use crate::api::{ListNetworkActivityRequest, NetworkActivity, NetworkActivityKind};
use crate::sandbox;
use crate::ui::table;

/// The first fbk release whose sandboxes get a network log.
const NETWORK_LOG_VERSION: &str = "0.7.0";

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
    let name = sandbox::resolve_owned_spec(working_dir, client).await?.name;
    let request = ListNetworkActivityRequest { name: name.clone() };

    let entries = match client.list_network_activity(request).await {
        Ok(response) => response.into_inner().entries,
        Err(status) if status.code() == Code::FailedPrecondition => bail!(
            "Network logging isn't available for {name}: it was created before fbk \
             {NETWORK_LOG_VERSION}. Remove and start it again to turn it on, or change its \
             network settings, for example with fbk network allow; a command that leaves them as \
             they are doesn't restart it."
        ),
        Err(status) => return Err(sandbox::describe_status(&name, status)),
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
    use crate::network::tests::workspace;
    use crate::spec_file::SPEC_FILE_NAME;
    use crate::test_support::fake_daemon::{self, OTHER_WORKSPACE};
    use std::fs;
    use tempfile::TempDir;

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
        let canonical = dunce::canonicalize(dir.path()).unwrap();
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
