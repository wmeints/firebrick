//! `fbk ls`: lists the sandboxes as a table for people or as JSON for scripts.

use anyhow::Result;
use clap::ValueEnum;
use serde::Serialize;
use tonic::transport::Channel;

use crate::api::{
    ListSandboxesRequest, ListSandboxesResponse, SandboxStatus, SandboxSummary,
    sandbox_management_service_client::SandboxManagementServiceClient,
};
use crate::ui::table;

/// Output format for the list of sandboxes.
#[derive(ValueEnum, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OutputFormat {
    /// A human-readable table.
    #[default]
    Table,
    /// A JSON array of sandboxes.
    Json,
}

/// Prints all sandboxes with their status in the given format.
pub async fn list_sandboxes(
    client: &mut SandboxManagementServiceClient<Channel>,
    format: OutputFormat,
) -> Result<()> {
    let response = client.list_sandboxes(ListSandboxesRequest {}).await?;
    let response_data: ListSandboxesResponse = response.into_inner();

    let output = match format {
        OutputFormat::Table => render_sandbox_table(&response_data.sandboxes)?,
        OutputFormat::Json => render_sandbox_json(&response_data.sandboxes)?,
    };

    print!("{output}");

    Ok(())
}

const TABLE_HEADER: [&str; 3] = ["NAME", "STATUS", "HOSTNAME"];

/// Renders the sandboxes as a table with their name, status and host name.
fn render_sandbox_table(sandboxes: &[SandboxSummary]) -> Result<String> {
    let rows: Vec<[String; 3]> = sandboxes
        .iter()
        .map(|item| {
            [
                item.name.clone(),
                format_status(item.status()),
                item.hostname.clone(),
            ]
        })
        .collect();

    table::render(TABLE_HEADER, &rows)
}

/// A sandbox as printed in the JSON output.
#[derive(Serialize)]
struct SandboxListing<'a> {
    name: &'a str,
    status: String,
    hostname: &'a str,
}

/// Renders the sandboxes as a pretty-printed JSON array.
fn render_sandbox_json(sandboxes: &[SandboxSummary]) -> Result<String> {
    let listings: Vec<SandboxListing> = sandboxes
        .iter()
        .map(|item| SandboxListing {
            name: &item.name,
            status: format_status(item.status()).to_lowercase(),
            hostname: &item.hostname,
        })
        .collect();

    Ok(format!("{}\n", serde_json::to_string_pretty(&listings)?))
}

/// Returns a human-readable label for a sandbox status.
pub(super) fn format_status(status: SandboxStatus) -> String {
    let status_text = match status {
        SandboxStatus::Running => "Running",
        SandboxStatus::Stopped => "Stopped",
        SandboxStatus::Starting => "Starting",
        SandboxStatus::Paused => "Paused",
        SandboxStatus::Stopping => "Stopping",
        SandboxStatus::Crashed => "Crashed",
        SandboxStatus::Unknown => "Unknown",
    };

    status_text.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(name: &str, status: SandboxStatus, hostname: &str) -> SandboxSummary {
        SandboxSummary {
            name: name.to_string(),
            status: status.into(),
            hostname: hostname.to_string(),
        }
    }

    #[test]
    fn table_aligns_sandboxes_under_header() {
        let sandboxes = [
            summary("dev", SandboxStatus::Running, "dev.fbk"),
            summary("long_project_name", SandboxStatus::Stopped, ""),
        ];

        let table = render_sandbox_table(&sandboxes).unwrap();

        assert_eq!(
            table,
            "┌───────────────────┬─────────┬──────────┐\n\
             │ NAME              │ STATUS  │ HOSTNAME │\n\
             ├───────────────────┼─────────┼──────────┤\n\
             │ dev               │ Running │ dev.fbk  │\n\
             │ long_project_name │ Stopped │          │\n\
             └───────────────────┴─────────┴──────────┘\n"
        );
    }

    #[test]
    fn table_shows_a_sandbox_whose_pause_state_is_unknown() {
        let sandboxes = [summary("dev", SandboxStatus::Unknown, "dev.fbk")];

        let table = render_sandbox_table(&sandboxes).unwrap();

        assert!(table.contains("│ dev  │ Unknown │ dev.fbk  │"), "{table}");
    }

    #[test]
    fn table_aligns_wide_characters() {
        let sandboxes = [summary("名前", SandboxStatus::Running, "")];

        let table = render_sandbox_table(&sandboxes).unwrap();
        let widths: Vec<usize> = table
            .lines()
            .map(|line| ratatui::text::Line::from(line).width())
            .collect();

        assert!(widths.windows(2).all(|pair| pair[0] == pair[1]), "{table}");
    }

    #[test]
    fn empty_table_shows_header() {
        let table = render_sandbox_table(&[]).unwrap();

        assert_eq!(
            table,
            "┌──────┬────────┬──────────┐\n\
             │ NAME │ STATUS │ HOSTNAME │\n\
             ├──────┼────────┼──────────┤\n\
             └──────┴────────┴──────────┘\n"
        );
    }

    #[test]
    fn json_lists_sandboxes() {
        let sandboxes = [summary("dev", SandboxStatus::Running, "dev.fbk")];

        let json = render_sandbox_json(&sandboxes).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(
            value,
            serde_json::json!([{ "name": "dev", "status": "running", "hostname": "dev.fbk" }])
        );
    }

    #[test]
    fn empty_json_is_empty_array() {
        assert_eq!(render_sandbox_json(&[]).unwrap(), "[]\n");
    }
}
