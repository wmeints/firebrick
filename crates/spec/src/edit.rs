//! Edits the text of a spec file line by line, so the rest of the file, including its
//! comments, stays as it is: adds and removes port mappings, and rewrites a spec written for an
//! older `fbk`, such as a boolean `mise`, to its current form.

use super::error::SandboxSpecError;
use super::from_str;
use super::ports::PortMapping;

use std::result::Result;

/// Returns the ports with `port` in place of the one with the same host port, or added at the
/// end when there is none.
pub fn with_port(ports: &[PortMapping], port: PortMapping) -> Vec<PortMapping> {
    let mut ports = ports.to_vec();

    match ports.iter_mut().find(|p| p.host == port.host) {
        Some(existing) => *existing = port,
        None => ports.push(port),
    }

    ports
}

/// Returns the text of a spec file with `port` in its `ports` list, in place of the entry with
/// the same host port or added at the end. Adds a `ports` key when the spec has none. Only the
/// `ports` block changes, so comments and formatting elsewhere are kept. Fails when the text
/// isn't a valid spec.
pub fn add_port(text: &str, port: PortMapping) -> Result<String, SandboxSpecError> {
    let ports = from_str(text)?.ports;
    let mut lines = Lines::of(text);

    match (
        lines.ports_block(ports.len()),
        ports_index(&ports, port.host),
    ) {
        (None, _) => lines.write_block(&with_port(&ports, port)),
        (Some(block), Some(index)) => lines.set_entry(block.entries[index], port),
        (Some(block), None) => lines.insert_entry(&block, port),
    }

    lines.into_spec()
}

/// Returns the text of a spec file without the entry of the host port in its `ports` list, or
/// `None` when the spec doesn't list the host port. Writes `ports: []` when the last entry is
/// removed. Only the `ports` block changes, so comments and formatting elsewhere are kept.
/// Fails when the text isn't a valid spec.
pub fn remove_port(text: &str, host: u16) -> Result<Option<String>, SandboxSpecError> {
    let ports = from_str(text)?.ports;
    let Some(index) = ports_index(&ports, host) else {
        return Ok(None);
    };
    let remaining: Vec<PortMapping> = ports.iter().filter(|p| p.host != host).copied().collect();
    let mut lines = Lines::of(text);

    match lines.ports_block(ports.len()) {
        Some(block) if !remaining.is_empty() => lines.remove_line(block.entries[index]),
        _ => lines.write_block(&remaining),
    }

    lines.into_spec().map(Some)
}

/// Returns the position of the host port in the ports.
fn ports_index(ports: &[PortMapping], host: u16) -> Option<usize> {
    ports.iter().position(|p| p.host == host)
}

/// Writes a mapping the way Docker Compose does: `3000` when the host and guest port match,
/// and `"8080:5173"` otherwise.
fn compose_entry(port: PortMapping) -> String {
    if port.host == port.guest {
        port.host.to_string()
    } else {
        format!("\"{port}\"")
    }
}

/// The top-level `ports` key of a spec file written as a block list, one entry per line.
struct PortsBlock {
    /// Line of the `ports:` key.
    key: usize,
    /// Line of each entry, in the order of the list.
    entries: Vec<usize>,
}

/// The lines of a spec file, each with its line ending.
struct Lines(Vec<String>);

impl Lines {
    fn of(text: &str) -> Self {
        Self(text.split_inclusive('\n').map(str::to_string).collect())
    }

    /// Finds the `ports` key when it holds a block list with one line per entry, `count` entries
    /// in all.
    fn ports_block(&self, count: usize) -> Option<PortsBlock> {
        let key = self.key("ports")?;
        let after_key = self.0[key]["ports:".len()..].trim();
        let values: Vec<usize> = (key + 1..key + 1 + self.block_len(key))
            .filter(|line| is_value(&self.0[*line]))
            .collect();
        let one_entry_per_line = values.len() == count
            && values
                .iter()
                .all(|line| self.0[*line].trim_start().starts_with('-'));

        let block_list = after_key.is_empty() || after_key.starts_with('#');
        (block_list && one_entry_per_line).then_some(PortsBlock {
            key,
            entries: values,
        })
    }

    /// Returns the line of the top-level key with the name.
    fn key(&self, name: &str) -> Option<usize> {
        self.0.iter().position(|line| {
            line.strip_prefix(name)
                .is_some_and(|rest| rest.starts_with(':'))
        })
    }

    /// Replaces the `ports` key and its entries with a block list of the ports, keeping the
    /// comments between them, or appends one when the spec has no `ports` key.
    fn write_block(&mut self, ports: &[PortMapping]) {
        let block = ports_block_text(ports);

        let Some(key) = self.key("ports") else {
            self.ensure_trailing_newline();
            self.0.push(block);
            return;
        };

        let end = key + 1 + self.block_len(key);
        let comments: Vec<String> = self.0[key + 1..end]
            .iter()
            .filter(|line| !is_value(line))
            .cloned()
            .collect();

        self.0
            .splice(key..end, std::iter::once(block).chain(comments))
            .for_each(drop);
    }

    /// Returns the number of lines after the key that belong to its value.
    fn block_len(&self, key: usize) -> usize {
        self.0[key + 1..]
            .iter()
            .take_while(|line| in_block(line))
            .count()
    }

    /// Replaces the entry on the line with the port, keeping its indentation.
    fn set_entry(&mut self, line: usize, port: PortMapping) {
        self.0[line] = entry_line(indentation(&self.0[line]), port);
    }

    /// Adds the port after the last entry of the block, with the same indentation.
    fn insert_entry(&mut self, block: &PortsBlock, port: PortMapping) {
        let last = block.entries.last().copied().unwrap_or(block.key);
        let indent = block
            .entries
            .last()
            .map_or("  ", |line| indentation(&self.0[*line]))
            .to_string();

        self.ensure_newline_at(last);
        self.0.insert(last + 1, entry_line(&indent, port));
    }

    fn remove_line(&mut self, line: usize) {
        self.0.remove(line);
    }

    fn ensure_trailing_newline(&mut self) {
        if let Some(last) = self.0.len().checked_sub(1) {
            self.ensure_newline_at(last);
        }
    }

    fn ensure_newline_at(&mut self, line: usize) {
        if !self.0[line].ends_with('\n') {
            self.0[line].push('\n');
        }
    }

    /// Joins the lines and checks that they are still a valid spec.
    fn into_spec(self) -> Result<String, SandboxSpecError> {
        let text = self.0.concat();
        from_str(&text)?;

        Ok(text)
    }
}

/// Whether the line belongs to the value of the top-level key above it: it's indented, an
/// entry of a list at the key's own indentation, a comment or blank.
fn in_block(line: &str) -> bool {
    line.starts_with([' ', '\t', '-', '#']) || line.trim().is_empty()
}

/// Whether the line holds a value, rather than only a comment or whitespace.
fn is_value(line: &str) -> bool {
    let trimmed = line.trim_start();

    !trimmed.is_empty() && !trimmed.starts_with('#')
}

/// Returns the leading whitespace of the line.
fn indentation(line: &str) -> &str {
    &line[..line.len() - line.trim_start().len()]
}

fn entry_line(indent: &str, port: PortMapping) -> String {
    format!("{indent}- {}\n", compose_entry(port))
}

/// Returns a `ports` key with a block list of the ports, or `ports: []` without ports.
fn ports_block_text(ports: &[PortMapping]) -> String {
    if ports.is_empty() {
        return "ports: []\n".to_string();
    }

    let entries: String = ports.iter().map(|port| entry_line("  ", *port)).collect();

    format!("ports:\n{entries}")
}

/// Rewrites a top-level `mise: true` or `mise: false` in the text of a spec file to the map
/// with `enabled`, keeping the rest of the text as written. Returns the new text and the value
/// of the boolean, or `None` when the spec has no boolean `mise`.
pub fn migrate_mise(text: &str) -> Option<(String, bool)> {
    let mut lines = Lines::of(text);
    let line = lines.key("mise")?;
    let (enabled, comment) = boolean_value(&lines.0[line]["mise:".len()..])?;
    let ending = if lines.0[line].ends_with("\r\n") {
        "\r\n"
    } else {
        "\n"
    };

    lines.0[line] = format!("mise:{comment}{ending}  enabled: {enabled}{ending}");

    Some((lines.0.concat(), enabled))
}

/// Reads a YAML boolean from the value after a key, the way the parser reads it, returning it
/// with the trailing comment, which keeps its leading space.
fn boolean_value(value: &str) -> Option<(bool, &str)> {
    let value = value.trim_end_matches(['\r', '\n']);
    let (scalar, comment) = match value.find(" #") {
        Some(start) => value.split_at(start),
        None => (value, ""),
    };

    let enabled = serde_yaml::from_str::<bool>(scalar.trim()).ok()?;

    Some((enabled, comment))
}
