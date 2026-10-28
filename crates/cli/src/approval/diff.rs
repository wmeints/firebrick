//! The lines that show the user what changed between the approved and the requested
//! settings, or what the settings are when none were approved yet.

use super::Settings;
use firebrick_spec::{MountSpec, NetworkRule, NetworkSpec};

/// Returns a line per added or removed mount, per changed network field and for a changed
/// clipboard setting between the approved and requested settings. Mounts and rules are compared
/// as sets, so their order doesn't count as a change.
pub fn difference(approved: &Settings, requested: &Settings) -> Vec<String> {
    let mut lines = list_difference(&approved.mounts, &requested.mounts, mount_line);
    lines.extend(network_difference(&approved.network, &requested.network));
    lines.extend(clipboard_difference(approved, requested));

    lines
}

/// Returns, when the clipboard modes differ, a `-` line for the approved mode and a `+` line
/// for the requested one, leaving out a mode that is off.
fn clipboard_difference(approved: &Settings, requested: &Settings) -> Vec<String> {
    if approved.clipboard == requested.clipboard {
        return Vec::new();
    }

    let removed = approved.clipboard.line().map(|line| format!("- {line}"));
    let added = requested.clipboard.line().map(|line| format!("+ {line}"));

    removed.into_iter().chain(added).collect()
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

/// Returns a line per mount and network field of settings that have nothing to compare with,
/// and one for the clipboard when it's shared.
pub(super) fn listing(settings: &Settings) -> Vec<String> {
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
    lines.extend(clipboard_difference(&Settings::default(), settings));

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
