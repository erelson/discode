pub use crate::terminal_pane::build_styled_frame;

#[cfg(test)]
mod tests {
    use super::build_styled_frame;
    use serde_json::Value;

    fn line_text(frame: &Value, row: usize) -> String {
        frame["lines"][row]["segments"]
            .as_array()
            .map(|segments| {
                segments
                    .iter()
                    .filter_map(|seg| seg["text"].as_str())
                    .collect::<String>()
            })
            .unwrap_or_default()
    }

    #[test]
    fn renders_cursor_rewrites() {
        let frame = build_styled_frame("hello\rbye", 20, 6);
        let first = line_text(&frame, 0);
        assert!(first.starts_with("byelo"));
    }

    #[test]
    fn handles_clear_screen_and_home() {
        let frame = build_styled_frame("old\x1b[2J\x1b[Hnew", 20, 6);
        let first = line_text(&frame, 0);
        assert!(first.starts_with("new"));
        assert!(!first.contains("old"));
    }

    #[test]
    fn keeps_primary_buffer_after_alt_screen_leave() {
        let frame = build_styled_frame("primary\x1b[?1049halt\x1b[?1049l", 20, 6);
        let joined = (0..6)
            .map(|idx| line_text(&frame, idx))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("primary"));
        assert!(!joined.contains("alt"));
    }

    #[test]
    fn emits_sgr_color_segments() {
        let frame = build_styled_frame("\x1b[31mred\x1b[0m normal", 20, 6);
        let red = frame["lines"][0]["segments"]
            .as_array()
            .and_then(|segments| {
                segments
                    .iter()
                    .find(|seg| seg["text"].as_str().unwrap_or("").contains("red"))
            });
        assert_eq!(
            red.and_then(|seg| seg.get("fg")).and_then(|v| v.as_str()),
            Some("#cd3131")
        );
    }
}
