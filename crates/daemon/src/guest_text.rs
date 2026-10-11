//! Makes text that a sandbox printed safe to show on the user's terminal.

/// Returns `text` without control characters other than newline and tab.
///
/// Guest output that ends up in an error message or a log line goes through this function, so a
/// workspace can't make `fbk` write terminal escape sequences, such as OSC 52 to set the
/// clipboard. This removes C0 controls, DEL and C1 controls, and keeps the text around them.
pub(crate) fn strip_control(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_control_keeps_plain_multi_line_text() {
        let text = "error: no such version\n\tat mise.toml line 2\n";

        assert_eq!(strip_control(text), text);
    }

    #[test]
    fn strip_control_removes_c0_controls() {
        assert_eq!(
            strip_control("a\x1b]52;c;eA==\x07b\r\x00c\x08"),
            "a]52;c;eA==bc"
        );
    }

    #[test]
    fn strip_control_removes_del() {
        assert_eq!(strip_control("a\x7fb"), "ab");
    }

    #[test]
    fn strip_control_removes_c1_controls() {
        assert_eq!(
            strip_control("a\u{9b}31mb\u{9d}52;c;eA==\u{9c}c"),
            "a31mb52;c;eA==c"
        );
    }
}
