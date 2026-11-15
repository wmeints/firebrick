//! Text rules shared by the CLI and the daemon: labels made from paths, and text without
//! control characters or escape sequences for terminals and logs.

use std::path::Path;

/// Name used when a path's leaf has no ASCII letters or digits left after sanitizing.
const FALLBACK_LABEL: &str = "sandbox";

/// Turns the leaf directory of `path` into a DNS label of lowercase ASCII letters, digits and
/// dashes: every run of other characters becomes one `-`, and leading and trailing dashes are
/// dropped. Returns `sandbox` when nothing is left, for example for `/`.
pub fn sanitize_label(path: &Path) -> String {
    let leaf = path
        .file_name()
        .map(|leaf| leaf.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    let mut label = String::new();

    for c in leaf.chars() {
        if c.is_ascii_alphanumeric() {
            label.push(c);
        } else if !label.is_empty() && !label.ends_with('-') {
            label.push('-');
        }
    }

    match label.trim_end_matches('-') {
        "" => FALLBACK_LABEL.to_string(),
        label => label.to_string(),
    }
}

/// Returns `text` without terminal escape sequences and control characters other than newline
/// and tab.
///
/// Guest output that ends up in an error message or a log line goes through this function, so a
/// workspace can't make `fbk` write terminal escape sequences, such as OSC 52 to set the
/// clipboard. [`console::strip_ansi_codes`] removes whole escape sequences, with the C1 forms of
/// CSI and OSC rewritten to their `ESC` forms first, and then the remaining C0 controls, DEL and
/// C1 controls are removed, keeping the text around them.
pub fn strip_control(text: &str) -> String {
    let text = text.replace('\u{9b}', "\x1b[").replace('\u{9d}', "\x1b]");
    console::strip_ansi_codes(&text)
        .chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn label(path: &str) -> String {
        // Labels the test's constant paths; no file is opened.
        // foxguard: ignore[rs/no-path-traversal]
        sanitize_label(Path::new(path))
    }

    #[test]
    fn label_is_the_leaf_in_lowercase_with_runs_of_other_characters_as_one_dash() {
        let cases = [
            ("/home/user/project", "project"),
            ("/home/user/My.App", "my-app"),
            ("/home/user/My_Project.v2--", "my-project-v2"),
            ("/home/user/my project_", "my-project"),
            ("/home/user/.config", "config"),
            ("/home/user/__", "sandbox"),
            ("/", "sandbox"),
            ("", "sandbox"),
        ];
        for (path, expected) in cases {
            assert_eq!(label(path), expected, "{path:?}");
        }
    }

    #[test]
    fn strip_control_removes_escape_sequences_and_controls_and_keeps_the_text() {
        let cases = [
            (
                "error: no such version\n\tat mise.toml line 2\n",
                "error: no such version\n\tat mise.toml line 2\n",
            ),
            // C0 controls and DEL.
            ("a\rb\x00c\x08", "abc"),
            ("a\x7fb", "ab"),
            // CSI sequences.
            ("\x1b[1;32mok\x1b[0m", "ok"),
            ("a\x1b[?25lb", "ab"),
            // OSC sequences, ended by BEL or ST.
            ("a\x1b]52;c;eA==\x07b", "ab"),
            ("a\x1b]0;title\x1b\\b", "ab"),
            // Two-character escapes.
            ("a\x1bcb\x1b7\x1b(Bc", "abc"),
            // C1 forms of CSI, OSC and ST, and other C1 controls.
            ("a\u{9b}31mb\u{9d}52;c;eA==\u{9c}c\u{85}d", "abcd"),
            // Unterminated sequences are dropped up to the end.
            ("a\x1b]52;c;eA==", "a"),
            ("a\x1b]x\nb", "a"),
            ("a\x1b[31", "a"),
            ("a\x1b", "a"),
        ];
        for (text, expected) in cases {
            assert_eq!(strip_control(text), expected, "{text:?}");
        }
    }

    #[test]
    fn strip_control_leaves_no_control_characters() {
        let parts = [
            "a", "\x1b", "[", "]", "\x07", "\\", "1;", "(", " ", "\n", "\t", "\r", "\x00", "\x7f",
            "\u{85}", "\u{9b}", "\u{9c}", "\u{9d}",
        ];
        let mut texts = vec![String::new()];
        for _ in 0..3 {
            texts = texts
                .iter()
                .flat_map(|text| parts.map(|part| format!("{text}{part}")))
                .collect();
        }
        for text in texts {
            let stripped = strip_control(&text);
            assert!(
                !stripped
                    .chars()
                    .any(|c| c.is_control() && !matches!(c, '\n' | '\t')),
                "{text:?} -> {stripped:?}"
            );
        }
    }
}
