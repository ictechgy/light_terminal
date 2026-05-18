pub fn strip_controls(value: &str) -> String {
    value.chars().filter(|ch| !is_c0_or_c1(*ch)).collect()
}

pub fn osc_field(value: &str) -> String {
    value
        .chars()
        .filter_map(|ch| match ch {
            '\n' | '\r' | '\t' | ';' => Some(' '),
            ch if is_c0_or_c1(ch) => None,
            _ => Some(ch),
        })
        .collect()
}

pub fn terminal_text(value: &str) -> String {
    strip_controls(value)
}

pub fn terminal_capture(bytes: &[u8]) -> String {
    let mut out = Vec::with_capacity(bytes.len());
    let mut state = EscapeState::Ground;

    for byte in bytes {
        match state {
            EscapeState::Ground => match *byte {
                0x1b => state = EscapeState::Esc,
                0x9b => state = EscapeState::Csi,
                0x90 | 0x98 | 0x9d | 0x9e | 0x9f => state = EscapeState::String,
                0x80..=0x9f => {}
                b'\t' | b'\n' | b'\r' => out.push(*byte),
                0x00..=0x1f | 0x7f => {}
                _ => out.push(*byte),
            },
            EscapeState::Esc => match *byte {
                0x18 | 0x1a => state = EscapeState::Ground,
                0x1b => state = EscapeState::Esc,
                b'[' => state = EscapeState::Csi,
                b']' | b'P' | b'_' | b'^' | b'X' => state = EscapeState::String,
                b'(' | b')' | b'*' | b'+' | b'-' | b'.' | b'/' => state = EscapeState::Charset,
                0x9b => state = EscapeState::Csi,
                0x90 | 0x98 | 0x9d | 0x9e | 0x9f => state = EscapeState::String,
                0x20..=0x2f => {}
                _ => state = EscapeState::Ground,
            },
            EscapeState::Csi => match *byte {
                0x18 | 0x1a | 0x9c => state = EscapeState::Ground,
                0x1b => state = EscapeState::Esc,
                byte if (0x40..=0x7e).contains(&byte) => state = EscapeState::Ground,
                _ => {}
            },
            EscapeState::String => match *byte {
                0x18 | 0x1a => state = EscapeState::Ground,
                0x07 | 0x9c => state = EscapeState::Ground,
                0x1b => state = EscapeState::StringEsc,
                _ => {}
            },
            EscapeState::StringEsc => {
                state = if *byte == b'\\' {
                    EscapeState::Ground
                } else {
                    EscapeState::String
                };
            }
            EscapeState::Charset => state = EscapeState::Ground,
        }
    }

    String::from_utf8_lossy(&out).to_string()
}

fn is_c0_or_c1(ch: char) -> bool {
    matches!(ch, '\u{0000}'..='\u{001f}' | '\u{007f}'..='\u{009f}')
}

#[derive(Clone, Copy)]
enum EscapeState {
    Ground,
    Esc,
    Csi,
    String,
    StringEsc,
    Charset,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc_field_drops_c0_and_c1_controls() {
        assert_eq!(osc_field("a\u{001b};\u{009c}b"), "a b");
        assert_eq!(osc_field("a\u{007f}b"), "ab");
    }

    #[test]
    fn osc_field_replaces_field_separators_and_spacing_controls_with_spaces() {
        assert_eq!(
            osc_field("sub\nbody\rnext\ttail;semi"),
            "sub body next tail semi"
        );
    }

    #[test]
    fn osc_field_preserves_non_control_unicode_text() {
        assert_eq!(osc_field("완료;테스트 ✅"), "완료 테스트 ✅");
    }

    #[test]
    fn terminal_capture_strips_escape_sequences() {
        let text = terminal_capture(b"ok \x1b[31mred\x1b[0m \x1b]52;c;secret\x07done\n");
        assert_eq!(text, "ok red done\n");
    }

    #[test]
    fn terminal_capture_strips_raw_c1_sequences() {
        let text = terminal_capture(b"ok \x9b31mred\x9b0m \x9d52;c;secret\x9cdone\n");
        assert_eq!(text, "ok red done\n");
    }

    #[test]
    fn terminal_capture_resynchronizes_escape_sequences() {
        let text = terminal_capture(b"ok \x1b[\x1b]52;c;secret\x07done\n");
        assert_eq!(text, "ok done\n");
    }

    #[test]
    fn terminal_capture_fuzz_lite_seed_corpus_has_no_escape_controls() {
        let corpus: &[(&str, &[u8])] = &[
            (
                "osc52_clipboard",
                include_bytes!("../tests/fixtures/sanitize_fuzz_lite/osc52_clipboard.bin"),
            ),
            (
                "split_csi_osc",
                include_bytes!("../tests/fixtures/sanitize_fuzz_lite/split_csi_osc.bin"),
            ),
            (
                "raw_c1_controls",
                include_bytes!("../tests/fixtures/sanitize_fuzz_lite/raw_c1_controls.bin"),
            ),
            (
                "unterminated_string",
                include_bytes!("../tests/fixtures/sanitize_fuzz_lite/unterminated_string.bin"),
            ),
            (
                "invalid_utf8_and_controls",
                include_bytes!(
                    "../tests/fixtures/sanitize_fuzz_lite/invalid_utf8_and_controls.bin"
                ),
            ),
        ];

        for (name, seed) in corpus {
            let sanitized = terminal_capture(seed);
            assert!(
                !sanitized
                    .chars()
                    .any(|ch| { is_c0_or_c1(ch) && !matches!(ch, '\t' | '\n' | '\r') }),
                "{name} left terminal controls in sanitized output: {sanitized:?}"
            );
        }
    }
}
