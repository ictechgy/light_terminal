pub fn strip_controls(value: &str) -> String {
    value.chars().filter(|ch| !is_c0_or_c1(*ch)).collect()
}

pub fn osc_field(value: &str) -> String {
    value
        .chars()
        .filter_map(|ch| match ch {
            ch if is_c0_or_c1(ch) => None,
            ';' => Some(' '),
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
}
