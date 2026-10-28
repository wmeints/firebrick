//! Network presets: groups of `allow` rules for well-known coding agents, toolchains and package
//! registries, compiled into `fbk` so nobody can change them by editing a file (see ADR 0040).
//! `fbk network presets apply` copies the rules of the named presets into the spec as ordinary
//! `allow` rules.

use anyhow::{Result, bail};
use firebrick_spec::NetworkRule;

use crate::ui::table;

/// The name that selects every preset.
pub const ALL: &str = "all";

/// A named group of network rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preset {
    /// Lowercase name of the preset, e.g. `rust`.
    pub name: &'static str,
    /// The rules the preset allows, written as in the spec.
    pub rules: &'static [&'static str],
}

/// Every preset, in the order `fbk network presets show` lists them.
pub const PRESETS: &[Preset] = &[
    Preset {
        name: "claude",
        rules: &["*.anthropic.com", "*.claude.ai", "*.claude.com"],
    },
    Preset {
        name: "copilot",
        rules: &[
            "*.githubcopilot.com",
            "copilot-proxy.githubusercontent.com",
            "origin-tracker.githubusercontent.com",
        ],
    },
    Preset {
        name: "github",
        rules: &[
            "*.github.com",
            "*.githubusercontent.com",
            "*.githubassets.com",
            "ghcr.io",
        ],
    },
    Preset {
        name: "codex",
        rules: &["*.openai.com", "*.chatgpt.com"],
    },
    Preset {
        name: "node",
        rules: &[
            "*.nodejs.org",
            "*.npmjs.org",
            "*.npmjs.com",
            "*.yarnpkg.com",
            "*.pnpm.io",
            "*.bun.sh",
        ],
    },
    Preset {
        name: "dotnet",
        rules: &[
            "*.nuget.org",
            "*.dot.net",
            "builds.dotnet.microsoft.com",
            "dotnetcli.azureedge.net",
            "dotnetcli.blob.core.windows.net",
        ],
    },
    Preset {
        name: "java",
        rules: &[
            "*.maven.org",
            "repo.maven.apache.org",
            "*.gradle.org",
            "*.adoptium.net",
            "central.sonatype.com",
            "oss.sonatype.org",
            "s01.oss.sonatype.org",
        ],
    },
    Preset {
        name: "rust",
        rules: &["*.crates.io", "*.rust-lang.org", "*.rustup.rs", "docs.rs"],
    },
    Preset {
        name: "go",
        rules: &["*.golang.org", "*.go.dev", "dl.google.com"],
    },
    Preset {
        name: "python",
        rules: &[
            "*.pypi.org",
            "*.pythonhosted.org",
            "*.python.org",
            "*.astral.sh",
            "*.anaconda.org",
            "repo.anaconda.com",
        ],
    },
    Preset {
        name: "mise",
        rules: &["*.jdx.dev", "mise.run"],
    },
    Preset {
        name: "ruby",
        rules: &["*.rubygems.org", "*.ruby-lang.org"],
    },
];

/// Returns the names `fbk network presets apply` accepts: `all`, then every preset.
pub fn names() -> impl Iterator<Item = &'static str> {
    std::iter::once(ALL).chain(PRESETS.iter().map(|preset| preset.name))
}

/// Returns the rules of the named presets, or of every preset when a name is `all`, each rule
/// once and in the order of [`PRESETS`]. Fails on an unknown name or an invalid rule.
pub fn rules<S: AsRef<str>>(names: &[S]) -> Result<Vec<NetworkRule>> {
    let names: Vec<&str> = names.iter().map(AsRef::as_ref).collect();
    if let Some(unknown) = names
        .iter()
        .find(|name| !self::names().any(|n| n == **name))
    {
        bail!("unknown network preset \"{unknown}\"");
    }

    select(PRESETS, &names)
}

/// Returns the rules of the presets with one of the names, or of every preset when a name is
/// `all`, each rule once.
fn select(presets: &[Preset], names: &[&str]) -> Result<Vec<NetworkRule>> {
    let all = names.contains(&ALL);
    let selected = presets
        .iter()
        .filter(|preset| all || names.contains(&preset.name))
        .flat_map(|preset| preset.rules);
    let mut rules = Vec::new();

    for rule in selected {
        let rule: NetworkRule = rule.parse()?;
        if !rules.contains(&rule) {
            rules.push(rule);
        }
    }

    Ok(rules)
}

/// Renders every preset and its rules as a table.
pub fn show() -> Result<String> {
    let rows: Vec<[String; 2]> = PRESETS
        .iter()
        .map(|preset| [preset.name.to_string(), preset.rules.join(", ")])
        .collect();

    table::render(["PRESET", "RULES"], &rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule_strings(names: &[&str]) -> Vec<String> {
        rules(names)
            .unwrap()
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    /// Returns every preset paired with each of its rules and the rules after it.
    fn preset_rules()
    -> impl Iterator<Item = (&'static Preset, &'static str, &'static [&'static str])> {
        PRESETS.iter().flat_map(|preset| {
            (0..preset.rules.len()).map(move |i| (preset, preset.rules[i], &preset.rules[i + 1..]))
        })
    }

    #[test]
    fn every_preset_rule_is_a_valid_network_rule() {
        for (preset, rule, _) in preset_rules() {
            let parsed: NetworkRule = rule.parse().unwrap();
            assert_eq!(parsed.to_string(), rule, "{}", preset.name);
        }
    }

    #[test]
    fn no_rule_appears_twice_within_a_preset() {
        for (preset, rule, later) in preset_rules() {
            assert!(!later.contains(&rule), "{} lists {rule} twice", preset.name);
        }
    }

    #[test]
    fn defines_the_expected_presets() {
        let expected = [
            "claude", "copilot", "github", "codex", "node", "dotnet", "java", "rust", "go",
            "python", "mise", "ruby",
        ];
        let actual: Vec<&str> = PRESETS.iter().map(|preset| preset.name).collect();

        assert_eq!(actual, expected);
    }

    #[test]
    fn preset_names_are_unique_lowercase_and_not_all() {
        for (index, preset) in PRESETS.iter().enumerate() {
            assert_ne!(preset.name, ALL);
            assert_eq!(preset.name, preset.name.to_lowercase());
            assert!(
                PRESETS[index + 1..].iter().all(|p| p.name != preset.name),
                "{}",
                preset.name
            );
        }
    }

    #[test]
    fn names_lists_all_then_every_preset_in_order() {
        let names: Vec<&str> = names().collect();

        assert_eq!(
            names,
            [
                "all", "claude", "copilot", "github", "codex", "node", "dotnet", "java", "rust",
                "go", "python", "mise", "ruby",
            ]
        );
    }

    #[test]
    fn rules_returns_the_rules_of_the_named_presets_once_in_preset_order() {
        let rust = ["*.crates.io", "*.rust-lang.org", "*.rustup.rs", "docs.rs"];
        let claude_and_rust: Vec<&str> = ["*.anthropic.com", "*.claude.ai", "*.claude.com"]
            .into_iter()
            .chain(rust)
            .collect();

        for (names, expected) in [
            (["rust", "claude"].as_slice(), claude_and_rust.as_slice()),
            (&["rust", "rust"], &rust),
        ] {
            assert_eq!(rule_strings(names), expected, "{names:?}");
        }
    }

    #[test]
    fn all_selects_every_rule_of_every_preset_once() {
        let all = rule_strings(&["all"]);

        for (preset, rule, _) in preset_rules() {
            assert!(all.iter().any(|r| r == rule), "{rule} of {}", preset.name);
        }
        for (index, rule) in all.iter().enumerate() {
            assert!(!all[index + 1..].contains(rule), "{rule} twice");
        }
        assert_eq!(rule_strings(&["rust", "all"]), all);
    }

    // No two real presets share a rule, so this calls the private `select` with presets that do.
    #[test]
    fn a_rule_shared_by_two_presets_is_selected_once() {
        let presets = [
            Preset {
                name: "a",
                rules: &["*.example.org", "a.example.com"],
            },
            Preset {
                name: "b",
                rules: &["b.example.com", "*.example.org"],
            },
        ];

        let rules: Vec<String> = select(&presets, &["a", "b"])
            .unwrap()
            .iter()
            .map(ToString::to_string)
            .collect();

        assert_eq!(rules, ["*.example.org", "a.example.com", "b.example.com"]);
    }

    #[test]
    fn rules_rejects_unknown_names() {
        assert_eq!(
            rules(&["rustt"]).unwrap_err().to_string(),
            "unknown network preset \"rustt\""
        );
    }

    #[test]
    fn show_lists_every_preset_with_its_rules() {
        let table = show().unwrap();

        assert!(table.contains("PRESET"), "{table}");
        for preset in PRESETS {
            let row = table
                .lines()
                .find(|line| line.starts_with(&format!("│ {} ", preset.name)))
                .unwrap_or_else(|| panic!("no row for {}:\n{table}", preset.name));
            assert!(row.contains(&preset.rules.join(", ")), "{row}");
        }
    }
}
