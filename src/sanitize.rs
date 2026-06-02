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
    let mut out = String::with_capacity(bytes.len());
    let mut state = EscapeState::Ground;
    let mut index = 0_usize;

    while index < bytes.len() {
        let byte = bytes[index];
        match state {
            EscapeState::Ground => match byte {
                0x1b => state = EscapeState::Esc,
                0x9b => state = EscapeState::Csi,
                0x90 | 0x98 | 0x9d | 0x9e | 0x9f => state = EscapeState::String,
                0x80..=0x9f => {}
                b'\t' | b'\n' | b'\r' => out.push(byte as char),
                0x00..=0x1f | 0x7f => {}
                0x00..=0x7f => out.push(byte as char),
                _ => {
                    if let Some((ch, len)) = decode_utf8_char(&bytes[index..]) {
                        match ch {
                            '\u{009b}' => state = EscapeState::Csi,
                            '\u{0090}' | '\u{0098}' | '\u{009d}' | '\u{009e}' | '\u{009f}' => {
                                state = EscapeState::String
                            }
                            ch if is_c0_or_c1(ch) => {}
                            _ => out.push(ch),
                        }
                        index += len;
                        continue;
                    }
                    out.push('\u{fffd}');
                }
            },
            EscapeState::Esc => match byte {
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
            EscapeState::Csi => match byte {
                0x18 | 0x1a | 0x9c => state = EscapeState::Ground,
                0x1b => state = EscapeState::Esc,
                byte if (0x40..=0x7e).contains(&byte) => state = EscapeState::Ground,
                _ => {}
            },
            EscapeState::String => match byte {
                0x18 | 0x1a => state = EscapeState::Ground,
                0x07 | 0x9c => state = EscapeState::Ground,
                0x1b => state = EscapeState::StringEsc,
                _ => {}
            },
            EscapeState::StringEsc => {
                state = if byte == b'\\' {
                    EscapeState::Ground
                } else {
                    EscapeState::String
                };
            }
            EscapeState::Charset => state = EscapeState::Ground,
        }
        index += 1;
    }

    out
}

fn is_c0_or_c1(ch: char) -> bool {
    matches!(ch, '\u{0000}'..='\u{001f}' | '\u{007f}'..='\u{009f}')
}

fn decode_utf8_char(bytes: &[u8]) -> Option<(char, usize)> {
    let first = *bytes.first()?;
    let width = utf8_char_width(first)?;
    if bytes.len() < width {
        return None;
    }
    let text = std::str::from_utf8(&bytes[..width]).ok()?;
    let ch = text.chars().next()?;
    Some((ch, width))
}

fn utf8_char_width(byte: u8) -> Option<usize> {
    match byte {
        0x00..=0x7f => Some(1),
        0xc2..=0xdf => Some(2),
        0xe0..=0xef => Some(3),
        0xf0..=0xf4 => Some(4),
        _ => None,
    }
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
    fn terminal_capture_preserves_utf8_text_while_stripping_controls() {
        let text = terminal_capture("ok \x1b[31m완료 테스트 ✅\x1b[0m done\n".as_bytes());
        assert_eq!(text, "ok 완료 테스트 ✅ done\n");
    }

    #[test]
    fn terminal_capture_strips_utf8_encoded_c1_controls() {
        let text = terminal_capture("ok \u{009b}31mred\u{009b}0m done\n".as_bytes());
        assert_eq!(text, "ok red done\n");
    }

    #[test]
    fn terminal_capture_resynchronizes_escape_sequences() {
        let text = terminal_capture(b"ok \x1b[\x1b]52;c;secret\x07done\n");
        assert_eq!(text, "ok done\n");
    }

    #[test]
    fn terminal_capture_handles_escape_edge_states_without_leaking_payloads() {
        assert_eq!(terminal_capture(b"A\x1b(B"), "A");
        assert_eq!(terminal_capture(b"A\x1b[31\x18B"), "AB");
        assert_eq!(terminal_capture(b"A\x1b]title\x1bXsecret\x07B"), "AB");
        assert_eq!(terminal_capture(b"A\x90secret\x1b\\B"), "AB");
    }

    #[test]
    fn terminal_capture_replaces_incomplete_or_invalid_utf8() {
        assert_eq!(terminal_capture(&[b'A', 0xe2]), "A�");
        assert_eq!(terminal_capture(&[b'A', 0xf5, b'B']), "A�B");
    }

    #[test]
    fn terminal_capture_fuzz_lite_seed_corpus_has_no_escape_controls() {
        let corpus: &[(&str, &[u8], &str, &[&str])] = &[
            (
                "osc52_clipboard",
                include_bytes!("../tests/fixtures/sanitize_fuzz_lite/osc52_clipboard.bin"),
                "ok done\n",
                &["secret", "52;c"],
            ),
            (
                "split_csi_osc",
                include_bytes!("../tests/fixtures/sanitize_fuzz_lite/split_csi_osc.bin"),
                "ok done\n",
                &["secret", "[31"],
            ),
            (
                "raw_c1_controls",
                include_bytes!("../tests/fixtures/sanitize_fuzz_lite/raw_c1_controls.bin"),
                "ok red done\n",
                &["secret", "52;c"],
            ),
            (
                "unterminated_string",
                include_bytes!("../tests/fixtures/sanitize_fuzz_lite/unterminated_string.bin"),
                "prefix ",
                &["never-terminated", "secret"],
            ),
            (
                "invalid_utf8_and_controls",
                include_bytes!(
                    "../tests/fixtures/sanitize_fuzz_lite/invalid_utf8_and_controls.bin"
                ),
                "a��b paste\n",
                &["2004", "?2004h", "?2004l"],
            ),
        ];

        for (name, seed, expected, forbidden_fragments) in corpus {
            assert!(
                seed.iter()
                    .any(|byte| matches!(byte, 0x00..=0x1f | 0x7f..=0x9f)),
                "{name} fixture should contain at least one raw control byte"
            );
            let sanitized = terminal_capture(seed);
            assert!(
                !sanitized
                    .chars()
                    .any(|ch| { is_c0_or_c1(ch) && !matches!(ch, '\t' | '\n' | '\r') }),
                "{name} left terminal controls in sanitized output: {sanitized:?}"
            );
            assert_eq!(
                sanitized, *expected,
                "{name} sanitized output changed unexpectedly"
            );
            for fragment in *forbidden_fragments {
                assert!(
                    !sanitized.contains(fragment),
                    "{name} leaked terminal payload fragment {fragment:?}: {sanitized:?}"
                );
            }
        }
    }
}
