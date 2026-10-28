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
        sanitize_label(Path::new(path))
    }

    #[test]
    fn label_uses_leaf_directory() {
        assert_eq!(label("/home/user/project"), "project");
    }

    #[test]
    fn label_is_lowercase_with_runs_replaced_by_one_dash() {
        assert_eq!(label("/home/user/My.App"), "my-app");
        assert_eq!(label("/home/user/My_Project.v2--"), "my-project-v2");
    }

    #[test]
    fn label_drops_leading_and_trailing_dashes() {
        assert_eq!(label("/home/user/my project_"), "my-project");
        assert_eq!(label("/home/user/.config"), "config");
    }

    #[test]
    fn empty_label_falls_back_to_sandbox() {
        assert_eq!(label("/home/user/__"), "sandbox");
        assert_eq!(label("/"), "sandbox");
        assert_eq!(label(""), "sandbox");
    }

    #[test]
    fn strip_control_keeps_plain_multi_line_text() {
        let text = "error: no such version\n\tat mise.toml line 2\n";

        assert_eq!(strip_control(text), text);
    }

    #[test]
    fn strip_control_removes_c0_controls() {
        assert_eq!(strip_control("a\rb\x00c\x08"), "abc");
    }

    #[test]
    fn strip_control_removes_del() {
        assert_eq!(strip_control("a\x7fb"), "ab");
    }

    #[test]
    fn strip_control_removes_csi_sequences() {
        assert_eq!(strip_control("\x1b[1;32mok\x1b[0m"), "ok");
        assert_eq!(strip_control("a\x1b[?25lb"), "ab");
    }

    #[test]
    fn strip_control_removes_osc_sequences() {
        assert_eq!(strip_control("a\x1b]52;c;eA==\x07b"), "ab");
        assert_eq!(strip_control("a\x1b]0;title\x1b\\b"), "ab");
    }

    #[test]
    fn strip_control_removes_two_character_escapes() {
        assert_eq!(strip_control("a\x1bcb\x1b7\x1b(Bc"), "abc");
    }

    #[test]
    fn strip_control_removes_c1_sequences() {
        assert_eq!(
            strip_control("a\u{9b}31mb\u{9d}52;c;eA==\u{9c}c\u{85}d"),
            "abcd"
        );
    }

    #[test]
    fn strip_control_drops_unterminated_sequences_up_to_the_end() {
        let cases = [
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
